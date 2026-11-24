//! A client session: transaction state machine, settings, and statement execution.

use crate::bind::{self, Binder, Ctx, Level, Scope};
use crate::eval::{EvalCtx, StatementState};
use crate::plan::{OutCol, Row};
use crate::run::run;
use pgmem_catalog::{ConstraintKind, Oid, Relation};
use pgmem_ir::{self as ir, Statement, TransactionStmt};
use pgmem_storage::{Database, Isolation, Txn};
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

/// The version pgmem reports (`server_version`, `version()`): the Postgres release it targets.
pub const SERVER_VERSION: &str = "18.0";
pub const SERVER_VERSION_NUM: &str = "180000";
pub const VERSION_STRING: &str = "PostgreSQL 18.0 (pgmem) on x86_64-pc-linux-gnu, compiled by rustc, 64-bit";

/// Source of "now". Tests pin it with `Server::set_now`.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Microseconds since the Postgres epoch (2000-01-01 UTC).
    fn now_micros(&self) -> i64;
}

/// The wall clock, optionally pinned to a fixed instant.
#[derive(Debug, Default)]
pub struct ServerClock {
    pinned: RwLock<Option<i64>>,
}

impl ServerClock {
    pub fn pin(&self, micros: Option<i64>) {
        *self.pinned.write().unwrap_or_else(std::sync::PoisonError::into_inner) = micros;
    }
}

impl Clock for ServerClock {
    fn now_micros(&self) -> i64 {
        if let Some(p) = *self.pinned.read().unwrap_or_else(std::sync::PoisonError::into_inner) {
            return p;
        }
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let unix_micros = i64::try_from(d.as_micros()).unwrap_or(i64::MAX);
        unix_micros - pgmem_types::datetime::PG_EPOCH_UNIX_SECS * 1_000_000
    }
}

/// Read-only session information available to builtins during a statement.
#[derive(Debug, Clone)]
pub struct SessionEnv {
    pub txn_start: i64,
    pub stmt_start: i64,
    pub clock: Arc<dyn Clock>,
    pub user: String,
    pub database: String,
    pub settings: BTreeMap<String, String>,
    pub backend_pid: i32,
    pub txid: i64,
    pub currval: HashMap<Oid, i64>,
    pub lastval: Option<i64>,
}

impl SessionEnv {
    pub fn setting(&self, name: &str) -> Option<String> {
        self.settings.get(&name.to_ascii_lowercase()).cloned()
    }

    /// An environment for internal evaluation outside a session (FK SET DEFAULT actions).
    pub fn detached() -> SessionEnv {
        let clock: Arc<dyn Clock> = Arc::new(ServerClock::default());
        let now = clock.now_micros();
        SessionEnv {
            txn_start: now,
            stmt_start: now,
            clock,
            user: "pgmem".into(),
            database: "pgmem".into(),
            settings: default_settings(),
            backend_pid: 0,
            txid: 0,
            currval: HashMap::new(),
            lastval: None,
        }
    }
}

/// Settings and their defaults (the subset clients read or set).
pub fn default_settings() -> BTreeMap<String, String> {
    [
        ("application_name", ""),
        ("bytea_output", "hex"),
        ("client_encoding", "UTF8"),
        ("client_min_messages", "notice"),
        ("datestyle", "ISO, MDY"),
        ("default_transaction_isolation", "read committed"),
        ("default_transaction_read_only", "off"),
        ("extra_float_digits", "1"),
        ("in_hot_standby", "off"),
        ("integer_datetimes", "on"),
        ("intervalstyle", "postgres"),
        ("is_superuser", "on"),
        ("lc_collate", "C"),
        ("lc_ctype", "C"),
        ("max_identifier_length", "63"),
        ("search_path", "\"$user\", public"),
        ("server_encoding", "UTF8"),
        ("server_version", SERVER_VERSION),
        ("server_version_num", SERVER_VERSION_NUM),
        ("standard_conforming_strings", "on"),
        ("statement_timeout", "0"),
        ("lock_timeout", "0"),
        ("idle_in_transaction_session_timeout", "0"),
        ("timezone", "UTC"),
        ("transaction_isolation", "read committed"),
        ("transaction_read_only", "off"),
        ("row_security", "on"),
        ("synchronous_commit", "on"),
        ("work_mem", "4MB"),
        ("max_connections", "100"),
        ("session_authorization", "pgmem"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

/// Canonical display names of settings (as reported in ParameterStatus and SHOW headers).
pub fn setting_display_name(name: &str) -> &str {
    match name {
        "datestyle" => "DateStyle",
        "timezone" => "TimeZone",
        "intervalstyle" => "IntervalStyle",
        other => other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    Idle,
    InBlock,
    Failed,
}

impl TxStatus {
    /// The ReadyForQuery status byte.
    pub fn byte(self) -> u8 {
        match self {
            TxStatus::Idle => b'I',
            TxStatus::InBlock => b'T',
            TxStatus::Failed => b'E',
        }
    }
}

/// A notice or warning to deliver alongside a result.
#[derive(Clone, Debug)]
pub struct Notice {
    pub severity: &'static str,
    pub code: &'static str,
    pub message: String,
}

/// The outcome of executing one statement.
#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    /// Result columns; empty for statements that return no rows.
    pub columns: Vec<OutCol>,
    pub returns_rows: bool,
    pub rows: Vec<Row>,
    pub tag: String,
    /// The statement was empty (EmptyQueryResponse).
    pub empty: bool,
    pub notices: Vec<Notice>,
    /// Settings changed by this statement (ParameterStatus to report).
    pub changed_settings: Vec<(String, String)>,
}

/// What a statement will return, known before execution (Describe).
#[derive(Clone, Debug)]
pub struct Description {
    pub param_types: Vec<Type>,
    /// None = NoData.
    pub columns: Option<Vec<OutCol>>,
}

pub struct Session {
    db: Arc<Database>,
    clock: Arc<dyn Clock>,
    pub user: String,
    pub database: String,
    pub settings: BTreeMap<String, String>,
    /// Values of settings set with SET LOCAL, restored at transaction end.
    local_restore: Vec<(String, String)>,
    txn: Option<Txn>,
    status: TxStatus,
    txn_start: i64,
    /// Running an implicit multi-statement block from a simple Query message.
    implicit_block: bool,
    currval: HashMap<Oid, i64>,
    lastval: Option<i64>,
    pub backend_pid: i32,
    txid_counter: i64,
    default_isolation: Isolation,
}

impl Session {
    pub fn new(db: Arc<Database>, clock: Arc<dyn Clock>, user: &str, database: &str, backend_pid: i32) -> Session {
        let mut settings = default_settings();
        settings.insert("session_authorization".into(), user.to_owned());
        Session {
            db,
            clock,
            user: user.to_owned(),
            database: database.to_owned(),
            settings,
            local_restore: Vec::new(),
            txn: None,
            status: TxStatus::Idle,
            txn_start: 0,
            implicit_block: false,
            currval: HashMap::new(),
            lastval: None,
            backend_pid,
            txid_counter: 1000,
            default_isolation: Isolation::ReadCommitted,
        }
    }

    pub fn status(&self) -> TxStatus {
        self.status
    }

    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    fn env(&self) -> SessionEnv {
        SessionEnv {
            txn_start: self.txn_start,
            stmt_start: self.clock.now_micros(),
            clock: Arc::clone(&self.clock),
            user: self.user.clone(),
            database: self.database.clone(),
            settings: self.settings.clone(),
            backend_pid: self.backend_pid,
            txid: self.txid_counter,
            currval: self.currval.clone(),
            lastval: self.lastval,
        }
    }

    /// Starts a simple-query batch: multiple statements share one implicit transaction.
    pub fn begin_implicit_block(&mut self, statements: usize) {
        self.implicit_block = statements > 1 && self.txn.is_none();
    }

    /// Ends a simple-query batch: commits (or discards, after an error) the implicit block.
    pub fn end_implicit_block(&mut self, failed: bool) -> SqlResult<()> {
        if !self.implicit_block {
            return Ok(());
        }
        self.implicit_block = false;
        if let Some(txn) = self.txn.take() {
            self.status = TxStatus::Idle;
            self.end_transaction_settings();
            if failed {
                txn.rollback();
            } else {
                txn.commit()?;
            }
        }
        Ok(())
    }

    fn catalog_snapshot(&self) -> Arc<pgmem_catalog::Catalog> {
        match &self.txn {
            Some(t) => Arc::new(t.catalog().clone()),
            None => Arc::clone(&self.db.current().catalog),
        }
    }

    /// The input/output context of this session's settings (`TimeZone`, `DateStyle` order).
    pub fn format_context(&self) -> pgmem_types::context::Context {
        use pgmem_types::context::{Context, DateOrder};
        let zone = self
            .settings
            .get("timezone")
            .and_then(|z| pgmem_types::zone::Zone::from_setting(z))
            .unwrap_or_else(pgmem_types::zone::Zone::utc);
        let style = self.settings.get("datestyle").map(|s| s.to_ascii_uppercase()).unwrap_or_default();
        let date_order = if style.contains("DMY") || style.contains("EURO") {
            DateOrder::Dmy
        } else if style.contains("YMD") {
            DateOrder::Ymd
        } else {
            DateOrder::Mdy
        };
        Context { zone, date_order, now: self.clock.now_micros() }
    }

    /// Analyzes a statement without running it: parameter types and result columns.
    pub fn describe(&self, stmt: &Statement, declared: &[Type]) -> SqlResult<Description> {
        let _ctx = pgmem_types::context::enter(self.format_context());
        let catalog = self.catalog_snapshot();
        let binder = Binder::new(&catalog, declared);
        let top = Level::new(Scope::default());
        let ctx = Ctx::new(&top);
        let columns = match stmt {
            Statement::Query(q) => Some(binder.bind_query(q, &ctx)?.columns),
            Statement::Insert(i) => returning(bind::bind_insert(&binder, i, &ctx)?.returning_cols),
            Statement::Update(u) => returning(bind::bind_update(&binder, u, &ctx)?.returning_cols),
            Statement::Delete(d) => returning(bind::bind_delete(&binder, d, &ctx)?.returning_cols),
            Statement::Show { name } => Some(vec![OutCol::new(setting_display_name(&show_key(name)), Type::Text)]),
            _ => None,
        };
        let params = binder.params.borrow().clone();
        if let Some(i) = params.iter().position(|t| *t == Type::Unknown) {
            return Err(SqlError::new(code::INDETERMINATE_DATATYPE, format!("could not determine data type of parameter ${}", i + 1)));
        }
        Ok(Description { param_types: params, columns })
    }

    fn ensure_txn(&mut self) -> bool {
        if self.txn.is_some() {
            return false;
        }
        self.txn_start = self.clock.now_micros();
        self.txid_counter += 1;
        self.txn = Some(self.db.begin(self.default_isolation));
        true
    }

    fn end_transaction_settings(&mut self) {
        for (k, v) in std::mem::take(&mut self.local_restore) {
            self.settings.insert(k, v);
        }
        self.settings
            .insert("transaction_isolation".into(), self.settings.get("default_transaction_isolation").cloned().unwrap_or_default());
    }

    /// Executes one statement with bound parameter values.
    pub fn execute(&mut self, stmt: &Statement, param_types: &[Type], params: &[Value]) -> SqlResult<QueryResult> {
        let _ctx = pgmem_types::context::enter(self.format_context());
        if let Statement::Transaction(t) = stmt {
            return self.transaction(t);
        }
        if self.status == TxStatus::Failed {
            return Err(SqlError::new(
                code::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        match stmt {
            Statement::Empty => return Ok(QueryResult { empty: true, ..Default::default() }),
            Statement::NoOp { tag } => return Ok(QueryResult { tag: tag.clone(), ..Default::default() }),
            Statement::Set { name, value, local } => return self.set(name, value, *local),
            Statement::Reset { name } => return self.reset(name.as_deref()),
            Statement::Show { name } => return self.show(name),
            _ => {}
        }
        let implicit = self.ensure_txn();
        let result = self.run_in_txn(stmt, param_types, params);
        match result {
            Ok(r) => {
                if implicit && !self.implicit_block {
                    if let Some(txn) = self.txn.take() {
                        self.status = TxStatus::Idle;
                        txn.commit()?;
                    }
                } else if self.status == TxStatus::Idle {
                    self.status = TxStatus::InBlock;
                }
                Ok(r)
            }
            Err(e) => {
                if (implicit && !self.implicit_block) || self.txn.is_none() {
                    self.txn = None;
                    self.status = TxStatus::Idle;
                } else if self.implicit_block {
                    // The simple-query batch is abandoned; end_implicit_block rolls it back.
                } else {
                    self.status = TxStatus::Failed;
                }
                Err(e)
            }
        }
    }

    fn run_in_txn(&mut self, stmt: &Statement, param_types: &[Type], params: &[Value]) -> SqlResult<QueryResult> {
        let env = self.env();
        let state = StatementState::default();
        let txn = self.txn.as_mut().ok_or_else(|| SqlError::internal("no transaction"))?;
        txn.begin_statement();
        let catalog = Arc::new(txn.catalog().clone());
        let binder = Binder::new(&catalog, param_types);
        let top = Level::new(Scope::default());
        let ctx = Ctx::new(&top);
        let result = match stmt {
            Statement::Query(q) => {
                let bound = binder.bind_query(q, &ctx)?;
                let ectx = EvalCtx::new(txn, params, &env, &state);
                let rows = run(&bound.rel, &ectx)?;
                QueryResult {
                    tag: format!("SELECT {}", rows.len()),
                    columns: bound.columns,
                    returns_rows: true,
                    rows,
                    ..Default::default()
                }
            }
            Statement::Insert(i) => {
                let plan = bind::bind_insert(&binder, i, &ctx)?;
                let r = crate::dml::exec_insert(&plan, txn, params, &env, &state)?;
                dml_result(format!("INSERT 0 {}", r.count), plan.returning_cols, r)
            }
            Statement::Update(u) => {
                let plan = bind::bind_update(&binder, u, &ctx)?;
                let r = crate::dml::exec_update(&plan, txn, params, &env, &state)?;
                dml_result(format!("UPDATE {}", r.count), plan.returning_cols, r)
            }
            Statement::Delete(d) => {
                let plan = bind::bind_delete(&binder, d, &ctx)?;
                let r = crate::dml::exec_delete(&plan, txn, params, &env, &state)?;
                dml_result(format!("DELETE {}", r.count), plan.returning_cols, r)
            }
            other => crate::ddl::execute_ddl(other, txn, &binder, &env, &state)?,
        };
        // Sequence values produced by this statement become visible to currval/lastval.
        let produced = state.currval.borrow();
        for (oid, v) in produced.iter() {
            self.currval.insert(*oid, *v);
            self.lastval = Some(*v);
        }
        Ok(result)
    }

    fn transaction(&mut self, t: &TransactionStmt) -> SqlResult<QueryResult> {
        let tag = |s: &str| QueryResult { tag: s.to_owned(), ..Default::default() };
        let warning = |msg: &str, c: &'static str| Notice { severity: "WARNING", code: c, message: msg.to_owned() };
        match t {
            TransactionStmt::Begin { isolation, read_only } => {
                if self.txn.is_some() && !self.implicit_block {
                    let mut r = tag("BEGIN");
                    r.notices.push(warning("there is already a transaction in progress", code::ACTIVE_SQL_TRANSACTION));
                    return Ok(r);
                }
                self.implicit_block = false;
                self.ensure_txn();
                let _ = read_only;
                if let Some(level) = isolation {
                    self.set_isolation(*level)?;
                }
                self.status = TxStatus::InBlock;
                Ok(tag("BEGIN"))
            }
            TransactionStmt::Commit { .. } => {
                let Some(txn) = self.txn.take() else {
                    let mut r = tag("COMMIT");
                    r.notices.push(warning("there is no transaction in progress", code::NO_ACTIVE_SQL_TRANSACTION));
                    return Ok(r);
                };
                let failed = self.status == TxStatus::Failed;
                self.status = TxStatus::Idle;
                self.end_transaction_settings();
                if failed {
                    txn.rollback();
                    return Ok(tag("ROLLBACK"));
                }
                txn.commit()?;
                Ok(tag("COMMIT"))
            }
            TransactionStmt::Rollback { .. } => {
                let Some(txn) = self.txn.take() else {
                    let mut r = tag("ROLLBACK");
                    r.notices.push(warning("there is no transaction in progress", code::NO_ACTIVE_SQL_TRANSACTION));
                    return Ok(r);
                };
                self.status = TxStatus::Idle;
                self.end_transaction_settings();
                txn.rollback();
                Ok(tag("ROLLBACK"))
            }
            TransactionStmt::Savepoint(name) => {
                if self.status == TxStatus::Failed {
                    return Err(SqlError::new(
                        code::IN_FAILED_SQL_TRANSACTION,
                        "current transaction is aborted, commands ignored until end of transaction block",
                    ));
                }
                let txn =
                    self.txn.as_mut().filter(|_| self.status == TxStatus::InBlock).ok_or_else(|| {
                        SqlError::new(code::NO_ACTIVE_SQL_TRANSACTION, "SAVEPOINT can only be used in transaction blocks")
                    })?;
                txn.savepoint(name);
                Ok(tag("SAVEPOINT"))
            }
            TransactionStmt::Release(name) => {
                if self.status == TxStatus::Failed {
                    return Err(SqlError::new(
                        code::IN_FAILED_SQL_TRANSACTION,
                        "current transaction is aborted, commands ignored until end of transaction block",
                    ));
                }
                let txn = self.txn.as_mut().filter(|_| self.status != TxStatus::Idle).ok_or_else(|| {
                    SqlError::new(code::NO_ACTIVE_SQL_TRANSACTION, "RELEASE SAVEPOINT can only be used in transaction blocks")
                })?;
                txn.release(name)?;
                Ok(tag("RELEASE"))
            }
            TransactionStmt::RollbackTo(name) => {
                let txn = self.txn.as_mut().filter(|_| self.status != TxStatus::Idle).ok_or_else(|| {
                    SqlError::new(code::NO_ACTIVE_SQL_TRANSACTION, "ROLLBACK TO SAVEPOINT can only be used in transaction blocks")
                })?;
                txn.rollback_to(name)?;
                self.status = TxStatus::InBlock;
                Ok(tag("ROLLBACK"))
            }
            TransactionStmt::SetTransaction { isolation, .. } => {
                if self.txn.is_none() {
                    let mut r = tag("SET");
                    r.notices.push(warning("SET TRANSACTION can only be used in transaction blocks", code::NO_ACTIVE_SQL_TRANSACTION));
                    return Ok(r);
                }
                if let Some(level) = isolation {
                    self.set_isolation(*level)?;
                }
                Ok(tag("SET"))
            }
        }
    }

    fn set_isolation(&mut self, level: ir::IsolationLevel) -> SqlResult<()> {
        let iso = match level {
            ir::IsolationLevel::ReadUncommitted | ir::IsolationLevel::ReadCommitted => Isolation::ReadCommitted,
            ir::IsolationLevel::RepeatableRead => Isolation::RepeatableRead,
            ir::IsolationLevel::Serializable => Isolation::Serializable,
        };
        if let Some(t) = self.txn.as_mut() {
            t.set_isolation(iso)?;
        }
        let name = match level {
            ir::IsolationLevel::ReadUncommitted => "read uncommitted",
            other => match other {
                ir::IsolationLevel::RepeatableRead => "repeatable read",
                ir::IsolationLevel::Serializable => "serializable",
                _ => "read committed",
            },
        };
        self.settings.insert("transaction_isolation".into(), name.into());
        Ok(())
    }

    fn set(&mut self, name: &str, value: &[String], local: bool) -> SqlResult<QueryResult> {
        let name = name.to_ascii_lowercase();
        let name = match name.as_str() {
            "names" => "client_encoding".to_owned(),
            "time zone" => "timezone".to_owned(),
            "session characteristics as transaction isolation level" => "default_transaction_isolation".to_owned(),
            _ => name,
        };
        let joined = if value.len() == 1 && value[0].eq_ignore_ascii_case("default") {
            default_settings().get(&name).cloned().unwrap_or_default()
        } else if name == "search_path" {
            value
                .iter()
                .map(|v| if v.contains('$') || v.chars().any(|c| c.is_ascii_uppercase()) { format!("\"{v}\"") } else { v.clone() })
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            value.join(", ")
        };
        let joined = match name.as_str() {
            "timezone" => timezone_setting(&joined)?,
            "client_encoding"
                if !joined.eq_ignore_ascii_case("utf8")
                    && !joined.eq_ignore_ascii_case("utf-8")
                    && !joined.eq_ignore_ascii_case("unicode") =>
            {
                return Err(SqlError::feature_not_supported(format!("conversion to encoding \"{joined}\" is not supported by pgmem")));
            }
            "client_encoding" => "UTF8".to_owned(),
            "default_transaction_isolation" => {
                let lower = joined.to_ascii_lowercase();
                self.default_isolation = match lower.as_str() {
                    "repeatable read" => Isolation::RepeatableRead,
                    "serializable" => Isolation::Serializable,
                    _ => Isolation::ReadCommitted,
                };
                lower
            }
            "transaction_isolation" => {
                let level = match joined.to_ascii_lowercase().as_str() {
                    "repeatable read" => ir::IsolationLevel::RepeatableRead,
                    "serializable" => ir::IsolationLevel::Serializable,
                    "read uncommitted" => ir::IsolationLevel::ReadUncommitted,
                    _ => ir::IsolationLevel::ReadCommitted,
                };
                self.set_isolation(level)?;
                return Ok(QueryResult { tag: "SET".into(), ..Default::default() });
            }
            _ => joined,
        };
        if local {
            if self.txn.is_some() {
                let old = self.settings.get(&name).cloned().unwrap_or_default();
                self.local_restore.push((name.clone(), old));
            } else {
                return Ok(QueryResult {
                    tag: "SET".into(),
                    notices: vec![Notice {
                        severity: "WARNING",
                        code: code::NO_ACTIVE_SQL_TRANSACTION,
                        message: "SET LOCAL can only be used in transaction blocks".into(),
                    }],
                    ..Default::default()
                });
            }
        }
        self.settings.insert(name.clone(), joined.clone());
        let reported = if is_reported(&name) { vec![(setting_display_name(&name).to_owned(), joined)] } else { Default::default() };
        Ok(QueryResult { tag: "SET".into(), changed_settings: reported, ..Default::default() })
    }

    fn reset(&mut self, name: Option<&str>) -> SqlResult<QueryResult> {
        let defaults = default_settings();
        match name {
            None => {
                for (k, v) in defaults {
                    if k != "session_authorization" {
                        self.settings.insert(k, v);
                    }
                }
            }
            Some(n) => {
                let n = n.to_ascii_lowercase();
                if let Some(v) = defaults.get(&n) {
                    self.settings.insert(n, v.clone());
                } else {
                    self.settings.remove(&n);
                }
            }
        }
        Ok(QueryResult { tag: "RESET".into(), ..Default::default() })
    }

    fn show(&self, name: &str) -> SqlResult<QueryResult> {
        let key = show_key(name);
        let value = self
            .settings
            .get(&key)
            .cloned()
            .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("unrecognized configuration parameter \"{name}\"")))?;
        Ok(QueryResult {
            columns: vec![OutCol::new(setting_display_name(&key), Type::Text)],
            returns_rows: true,
            rows: vec![vec![Value::Text(value)]],
            tag: "SHOW".into(),
            ..Default::default()
        })
    }

    /// Rows of a table, for the `seed` test helper's validation.
    pub fn table_exists(&self, name: &str) -> bool {
        matches!(self.db.current().catalog.relation(name), Some(Relation::Table(_)))
    }
}

/// The value `TimeZone` takes from `SET TIME ZONE ...`: an IANA name (canonical spelling), a
/// POSIX-style spec, a number of hours east, an `INTERVAL '...' HOUR TO MINUTE`, or LOCAL /
/// DEFAULT.
fn timezone_setting(value: &str) -> SqlResult<String> {
    use pgmem_types::zone::Zone;
    let invalid = || SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"TimeZone\": \"{value}\""));
    let v = value.trim();
    if v.eq_ignore_ascii_case("local") || v.eq_ignore_ascii_case("default") {
        return Ok("UTC".to_owned());
    }
    if v.len() > 9 && v[..9].eq_ignore_ascii_case("interval ") {
        let literal = v[9..].split('\'').nth(1).ok_or_else(invalid)?;
        let iv = pgmem_types::datetime::parse_interval(literal).map_err(|_| invalid())?;
        if iv.months != 0 || iv.days != 0 {
            return Err(SqlError::new(
                code::INVALID_PARAMETER_VALUE,
                String::from("invalid interval value for time zone: day not allowed"),
            ));
        }
        let secs = i32::try_from(iv.micros / 1_000_000).map_err(|_| invalid())?;
        return Ok(Zone::fixed(secs).name);
    }
    if let Ok(hours) = v.parse::<f64>() {
        #[allow(clippy::cast_possible_truncation)]
        let secs = (hours * 3600.0).round() as i32;
        return Ok(Zone::fixed(secs).name);
    }
    Zone::from_setting(v).map(|z| z.name).ok_or_else(invalid)
}

/// The setting a `SHOW` name refers to (`SHOW TIME ZONE` is `timezone`).
fn show_key(name: &str) -> String {
    let key = name.to_ascii_lowercase();
    match key.as_str() {
        "time zone" => "timezone".to_owned(),
        "transaction isolation level" => "transaction_isolation".to_owned(),
        _ => key,
    }
}

/// Settings that Postgres reports to clients with ParameterStatus when they change.
pub fn is_reported(name: &str) -> bool {
    matches!(
        name,
        "application_name"
            | "client_encoding"
            | "datestyle"
            | "default_transaction_read_only"
            | "in_hot_standby"
            | "integer_datetimes"
            | "intervalstyle"
            | "is_superuser"
            | "search_path"
            | "server_encoding"
            | "server_version"
            | "session_authorization"
            | "standard_conforming_strings"
            | "timezone"
            | "scram_iterations"
    )
}

fn returning(cols: Vec<OutCol>) -> Option<Vec<OutCol>> {
    if cols.is_empty() { None } else { Some(cols) }
}

fn dml_result(tag: String, cols: Vec<OutCol>, r: crate::dml::DmlResult) -> QueryResult {
    let returns_rows = !cols.is_empty();
    QueryResult { tag, columns: cols, returns_rows, rows: r.rows, ..Default::default() }
}

#[allow(dead_code)]
fn unused(_: ConstraintKind) {}
