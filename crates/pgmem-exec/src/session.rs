//! A client session: transaction state machine, settings, and statement execution.

use crate::bind::{self, Binder, Ctx, Level, Scope};
use crate::eval::StatementState;
use crate::plan::{OutCol, Row};
use pgmem_catalog::{ConstraintKind, Oid, Relation};
use pgmem_ir::{self as ir, Statement, TransactionStmt};
use pgmem_storage::{Database, Isolation, Txn};
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

mod copy;
mod objects;

pub use copy::{CopyIn, CopyOut};
pub use objects::SessionObjects;

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
    /// Prepared statements, cursors and LISTEN channels (catalog views read them).
    pub objects: Arc<SessionObjects>,
    /// SET CONSTRAINTS state of the current transaction.
    pub constraint_modes: Arc<ConstraintModes>,
}

/// `SET CONSTRAINTS` within a transaction: ALL, then per-constraint overrides.
#[derive(Clone, Debug, Default)]
pub struct ConstraintModes {
    pub all: Option<bool>,
    pub by_oid: HashMap<Oid, bool>,
}

impl ConstraintModes {
    /// Whether the constraint is checked at commit rather than immediately.
    pub fn deferred(&self, c: &pgmem_catalog::Constraint) -> bool {
        c.deferrable && self.by_oid.get(&c.oid).copied().or(self.all).unwrap_or(c.initially_deferred)
    }
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
            objects: Arc::default(),
            constraint_modes: Arc::default(),
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
        ("work_mem", "4096"),
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
    /// Rows are sent in binary (a FETCH from a BINARY cursor).
    pub binary: bool,
    /// COPY FROM STDIN: the client sends the data next (CopyInResponse).
    pub copy_in: Option<Box<CopyIn>>,
    /// COPY TO STDOUT: the data to send (CopyOutResponse, CopyData..., CopyDone).
    pub copy_out: Option<Box<CopyOut>>,
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
    /// Settings changed inside the transaction block (undone at ROLLBACK; SET LOCAL ones at
    /// any transaction end), with savepoint marks into it.
    setting_undo: Vec<SettingUndo>,
    setting_marks: Vec<Mark>,
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
    /// The session's temporary schema (`pg_temp_<pid>`), created with its first temporary
    /// object and dropped with the session.
    pub temp_schema: Option<String>,
    /// Prepared statements by name (SQL PREPARE and protocol Parse; `""` is the unnamed one).
    prepared: BTreeMap<String, Arc<crate::portal::PreparedStatement>>,
    cursors: BTreeMap<String, crate::portal::Cursor>,
    /// Open extended-protocol portals (listed in `pg_cursors` like SQL cursors).
    protocol_portals: BTreeMap<String, crate::portal::CursorInfo>,
    /// Channels this session listens on (as of its last commit).
    listening: objects::Channels,
    /// LISTEN / UNLISTEN and notifications queued by the open transaction.
    pending_listen: Vec<objects::ListenOp>,
    pending_notify: Vec<(String, String)>,
    /// Running an extended-protocol implicit transaction, which ends at Sync.
    pipeline: bool,
    /// The current transaction is READ ONLY.
    txn_read_only: bool,
    /// SET CONSTRAINTS of the current transaction.
    constraint_modes: ConstraintModes,
    /// Deferred constraint checks owed at commit: (table OID, constraint name).
    pending_checks: Vec<(Oid, String)>,
    /// Temporary tables created `ON COMMIT DELETE ROWS` / `ON COMMIT DROP`.
    on_commit: Vec<(Oid, ir::OnCommit)>,
    /// The data of a COPY FROM STDIN being finished (`copy_from`), or the client's CopyFail
    /// message.
    copy_data: Option<Result<Vec<u8>, String>>,
}

/// A savepoint's marks into the transaction's undo and queues.
struct Mark {
    name: String,
    settings: usize,
    listens: usize,
    notifies: usize,
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
            setting_undo: Vec::new(),
            setting_marks: Vec::new(),
            txn: None,
            status: TxStatus::Idle,
            txn_start: 0,
            implicit_block: false,
            currval: HashMap::new(),
            lastval: None,
            backend_pid,
            txid_counter: 1000,
            default_isolation: Isolation::ReadCommitted,
            temp_schema: Some(format!("pg_temp_{backend_pid}")),
            prepared: BTreeMap::new(),
            cursors: BTreeMap::new(),
            protocol_portals: BTreeMap::new(),
            listening: objects::Channels::new(),
            pending_listen: Vec::new(),
            pending_notify: Vec::new(),
            pipeline: false,
            txn_read_only: false,
            constraint_modes: ConstraintModes::default(),
            pending_checks: Vec::new(),
            on_commit: Vec::new(),
            copy_data: None,
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
            objects: self.objects(),
            constraint_modes: Arc::new(self.constraint_modes.clone()),
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
        // The batch's SETs are part of its implicit transaction, even when nothing else
        // started one.
        self.end_transaction_settings(!failed);
        if let Some(txn) = self.txn.take() {
            self.status = TxStatus::Idle;
            self.finish_txn(txn, !failed)?;
        }
        Ok(())
    }

    /// An extended-protocol Execute: without an open transaction, the statements up to the
    /// next Sync share one implicit transaction.
    pub fn begin_pipeline(&mut self) {
        if self.txn.is_none() {
            self.pipeline = true;
        }
    }

    /// Sync: commits (or, after an error, rolls back) the implicit transaction.
    pub fn end_pipeline(&mut self, failed: bool) -> SqlResult<()> {
        if !std::mem::take(&mut self.pipeline) {
            return Ok(());
        }
        if let Some(txn) = self.txn.take() {
            self.status = TxStatus::Idle;
            self.end_transaction_settings(!failed);
            self.finish_txn(txn, !failed)?;
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
        use pgmem_types::context::{DateStyle, IntervalStyle};
        let style = self.settings.get("datestyle").map(|s| s.to_ascii_uppercase()).unwrap_or_default();
        let date_order = if style.contains("DMY") || style.contains("EURO") {
            DateOrder::Dmy
        } else if style.contains("YMD") {
            DateOrder::Ymd
        } else {
            DateOrder::Mdy
        };
        let date_style = if style.starts_with("SQL") {
            DateStyle::Sql
        } else if style.starts_with("POSTGRES") {
            DateStyle::Postgres
        } else if style.starts_with("GERMAN") {
            DateStyle::German
        } else {
            DateStyle::Iso
        };
        let interval_style = match self.settings.get("intervalstyle").map(String::as_str) {
            Some("postgres_verbose") => IntervalStyle::PostgresVerbose,
            Some("sql_standard") => IntervalStyle::SqlStandard,
            Some("iso_8601") => IntervalStyle::Iso8601,
            _ => IntervalStyle::Postgres,
        };
        let extra_float_digits = self.settings.get("extra_float_digits").and_then(|v| v.parse().ok()).unwrap_or(1);
        let types = match &self.txn {
            Some(t) => crate::usertypes::registry(t.catalog()),
            None => crate::usertypes::registry(&self.db.current().catalog),
        };
        let bytea_escape = self.settings.get("bytea_output").is_some_and(|v| v.eq_ignore_ascii_case("escape"));
        let search_path = pgmem_types::context::SearchPath {
            entries: pgmem_types::context::SearchPath::parse(self.settings.get("search_path").map_or("", String::as_str)),
            user: self.user.clone(),
            temp_schema: self.temp_schema.clone(),
        };
        Context {
            zone,
            date_order,
            date_style,
            interval_style,
            extra_float_digits,
            now: self.clock.now_micros(),
            types,
            bytea_escape,
            search_path,
        }
    }

    /// Analyzes a statement without running it: parameter types and result columns.
    pub fn describe(&self, stmt: &Statement, declared: &[Type]) -> SqlResult<Description> {
        let d = self.describe_lenient(stmt, declared)?;
        if let Some(i) = d.param_types.iter().position(|t| *t == Type::Unknown) {
            return Err(SqlError::new(code::INDETERMINATE_DATATYPE, format!("could not determine data type of parameter ${}", i + 1)));
        }
        Ok(d)
    }

    /// [`Session::describe`], leaving parameters of undetermined type `unknown`.
    fn describe_lenient(&self, stmt: &Statement, declared: &[Type]) -> SqlResult<Description> {
        let _ctx = pgmem_types::context::enter(self.format_context());
        let catalog = self.catalog_snapshot();
        let binder = Binder::new(&catalog, declared);
        let top = Level::new(Scope::default());
        let ctx = Ctx::new(&top);
        let columns = match stmt {
            _ if crate::statement::leading_with(stmt).is_some() => crate::statement::describe_with(stmt, &binder, &ctx)?,
            Statement::Query(q) => Some(binder.bind_query(q, &ctx)?.columns),
            Statement::Insert(i) => returning(bind::bind_insert(&binder, i, &ctx)?.returning_cols),
            Statement::Update(u) => returning(bind::bind_update(&binder, u, &ctx)?.returning_cols),
            Statement::Delete(d) => returning(bind::bind_delete(&binder, d, &ctx)?.returning_cols),
            Statement::Merge(m) => returning(bind::merge::bind_merge(&binder, m, &ctx)?.returning_cols),
            Statement::Show { name } => Some(vec![OutCol::new(setting_display_name(&show_key(name)), Type::Text)]),
            Statement::Execute { name, .. } => self.prepared.get(name).and_then(|p| p.columns.clone()),
            Statement::Fetch { name, is_move: false, .. } => Some(self.cursor_columns(name)?),
            Statement::Explain(e) => {
                Some(vec![OutCol::new("QUERY PLAN", if e.format == ir::ExplainFormat::Json { Type::Json } else { Type::Text })])
            }
            _ => None,
        };
        let params = binder.params.borrow().clone();
        Ok(Description { param_types: params, columns })
    }

    fn ensure_txn(&mut self) -> bool {
        if self.txn.is_some() {
            return false;
        }
        self.txn_start = self.clock.now_micros();
        self.txid_counter += 1;
        self.txn = Some(self.db.begin(self.default_isolation));
        let read_only = self.settings.get("default_transaction_read_only").is_some_and(|v| v == "on");
        self.set_read_only(read_only);
        true
    }

    /// Ends the transaction's setting changes: a commit keeps plain SETs and undoes SET LOCAL;
    /// a rollback undoes everything (a custom placeholder stays defined, as empty).
    fn end_transaction_settings(&mut self, committed: bool) {
        let undo = std::mem::take(&mut self.setting_undo);
        self.setting_marks.clear();
        // A later plain SET of the same name makes a commit keep its value.
        for (i, u) in undo.iter().enumerate().rev() {
            if committed && (!u.local || undo[i + 1..].iter().any(|x| x.name == u.name && !x.local)) {
                continue;
            }
            self.restore_setting(&u.name, u.old.clone());
        }
        self.settings
            .insert("transaction_isolation".into(), self.settings.get("default_transaction_isolation").cloned().unwrap_or_default());
        self.txn_read_only = false;
        let ro = self.settings.get("default_transaction_read_only").cloned().unwrap_or_else(|| "off".into());
        self.settings.insert("transaction_read_only".into(), ro);
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
            Statement::Prepare(p) => return self.prepare_sql(p),
            Statement::Deallocate { name } => return self.deallocate(name.as_deref()),
            Statement::Fetch { name, direction, is_move } => return self.fetch(name, *direction, *is_move),
            Statement::CloseCursor { name } => return self.close_cursor(name.as_deref()),
            Statement::Listen { channel } => return Ok(self.listen(Some((channel.clone(), true)))),
            Statement::Unlisten { channel } => return Ok(self.listen(channel.clone().map(|c| (c, false)))),
            Statement::Notify { channel, payload } => return self.notify(channel, payload.as_deref()),
            Statement::Maintenance { tag, tables, in_block_ok } => return self.maintenance(tag, tables, *in_block_ok),
            Statement::CreateIndex(ci) if ci.concurrently && self.in_block() => {
                return Err(SqlError::new(code::ACTIVE_SQL_TRANSACTION, "CREATE INDEX CONCURRENTLY cannot run inside a transaction block"));
            }
            Statement::Lock { tables, mode, nowait } => return self.lock_tables(tables, *mode, *nowait),
            Statement::SetConstraints { names, deferred } => return self.set_constraints(names, *deferred),
            Statement::Discard(k) if *k != ir::DiscardKind::Temp && *k != ir::DiscardKind::All => return self.discard(*k),
            _ => {}
        }
        let implicit = self.ensure_txn();
        let result = self.run_in_txn(stmt, param_types, params);
        match result {
            Ok(r) => {
                if implicit && !self.implicit_block && !self.pipeline {
                    if let Some(txn) = self.txn.take() {
                        self.status = TxStatus::Idle;
                        self.finish_txn(txn, true)?;
                    }
                } else if self.status == TxStatus::Idle && !self.pipeline {
                    self.status = TxStatus::InBlock;
                }
                Ok(r)
            }
            Err(e) => {
                if (implicit && !self.implicit_block && !self.pipeline) || self.txn.is_none() {
                    if let Some(txn) = self.txn.take() {
                        let _ = self.finish_txn(txn, false);
                    }
                    self.status = TxStatus::Idle;
                } else if self.implicit_block || self.pipeline {
                    // The simple-query batch is abandoned; end_implicit_block rolls it back.
                } else {
                    self.status = TxStatus::Failed;
                }
                Err(e)
            }
        }
    }

    fn run_in_txn(&mut self, stmt: &Statement, param_types: &[Type], params: &[Value]) -> SqlResult<QueryResult> {
        if self.txn_read_only
            && let Some(what) = self.read_only_violation(stmt)
        {
            return Err(SqlError::new(code::READ_ONLY_SQL_TRANSACTION, format!("cannot execute {what} in a read-only transaction")));
        }
        match stmt {
            Statement::Execute { name, args } => return self.execute_prepared(name, args),
            Statement::DeclareCursor(d) => return self.declare_cursor(d),
            Statement::Explain(e) => return self.explain(e, param_types, params),
            Statement::Discard(k) => return self.discard(*k),
            Statement::Copy(c) => return self.copy(c),
            Statement::Update(u) if objects::is_pg_settings(&u.table) => return self.update_pg_settings(u),
            _ => {}
        }
        let env = self.env();
        let state = StatementState::default();
        let temp_schema = self.temp_schema.clone();
        let user = self.user.clone();
        let txn = self.txn.as_mut().ok_or_else(|| SqlError::internal("no transaction"))?;
        txn.begin_statement();
        // Temporary objects live in the session's pg_temp_N schema, created on first use.
        let temp_stmt;
        let stmt = match (stmt, &temp_schema) {
            (Statement::CreateTable(ct), Some(temp)) if ct.temporary => {
                if let [schema, _] = ct.name.0.as_slice()
                    && schema != "pg_temp"
                    && schema != temp
                {
                    return Err(SqlError::new(code::INVALID_TABLE_DEFINITION, "cannot create temporary relation in non-temporary schema"));
                }
                ensure_temp_schema(txn, temp, &user);
                let mut ct = (**ct).clone();
                ct.name = ir::ObjectName(vec![temp.clone(), ct.name.base().to_owned()]);
                temp_stmt = Statement::CreateTable(Box::new(ct));
                &temp_stmt
            }
            (Statement::CreateSchema(cs), _) => return create_schema_with_elements(cs, txn, &user, &env, &state),
            (Statement::AlterSchema { name, rename_to }, _) => {
                return match rename_to {
                    Some(new) => crate::ddl::alter_schema(name, new, txn),
                    None => Ok(QueryResult { tag: "ALTER SCHEMA".into(), ..Default::default() }),
                };
            }
            _ => stmt,
        };
        let catalog = Arc::new(txn.catalog().clone());
        let binder = Binder::new(&catalog, param_types);
        let top = Level::new(Scope::default());
        let ctx = Ctx::new(&top);
        crate::roles::check_statement(stmt, &catalog, &env, param_types)?;
        let result = crate::statement::exec_statement(stmt, &binder, &ctx, txn, params, &env, &state)?;
        // New relations: their owner (when not the session user) and default privileges.
        let created = match stmt {
            Statement::CreateTable(ct) => Some((&ct.name, 'r')),
            Statement::CreateView(cv) => Some((&cv.name, 'r')),
            Statement::CreateSequence(cs) => Some((&cs.name, 'S')),
            _ => None,
        };
        if let Some((name, kind)) = created
            && let Ok(Some(rel)) = txn.catalog().lookup_relation(name)
            && !catalog.relation(rel.schema(), rel.name()).is_some_and(|r| r.oid() == rel.oid())
        {
            let role = crate::roles::current_role(&env);
            if role != env.user {
                txn.catalog_mut().set_owner(rel.oid(), Some(&role));
            }
            let schema = rel.schema().to_owned();
            crate::roles::apply_defaults(txn, &env, rel.oid(), &schema, kind);
        }
        if let Statement::CreateTable(ct) = stmt
            && ct.options.on_commit != ir::OnCommit::PreserveRows
            && let Ok(Some(Relation::Table(t))) = txn.catalog().lookup_relation(&ct.name)
        {
            self.on_commit.push((t.oid, ct.options.on_commit));
        }
        // Sequence values produced by this statement become visible to currval/lastval.
        let produced = state.currval.borrow();
        for (oid, v) in produced.iter() {
            self.currval.insert(*oid, *v);
            self.lastval = Some(*v);
        }
        drop(produced);
        for (channel, payload) in state.notifies.take() {
            self.queue_notification(channel, payload);
        }
        for check in state.deferred.take() {
            if !self.pending_checks.contains(&check) {
                self.pending_checks.push(check);
            }
        }
        let mut result = result;
        result.notices.extend(state.notices.take());
        let pending = state.settings.take();
        if !pending.is_empty() {
            let autocommit = self.status == TxStatus::Idle && !self.implicit_block;
            let reported = self.apply_pending_settings(pending, autocommit);
            result.changed_settings.extend(reported);
        }
        Ok(result)
    }

    /// The command a READ ONLY transaction may not run (writes to temporary tables are allowed).
    fn read_only_violation(&self, stmt: &Statement) -> Option<&'static str> {
        let temp = |name: &ir::ObjectName| {
            let catalog = self.catalog_snapshot();
            matches!(catalog.find_relation(name), Ok(Some(Relation::Table(t))) if Some(&t.schema) == self.temp_schema.as_ref())
        };
        Some(match stmt {
            Statement::Insert(i) if !temp(&i.table) => "INSERT",
            Statement::Copy(c) if c.from && !c.table.as_ref().is_some_and(temp) => "COPY FROM",
            Statement::Update(u) if !temp(&u.table) => "UPDATE",
            Statement::Delete(d) if !temp(&d.table) => "DELETE",
            Statement::Merge(m) if !temp(&m.table) => "MERGE",
            Statement::CreateTable(ct) if !ct.temporary => "CREATE TABLE",
            Statement::CreateView(cv) if cv.materialized => "CREATE MATERIALIZED VIEW",
            Statement::CreateView(_) => "CREATE VIEW",
            Statement::RefreshMatview { .. } => "REFRESH MATERIALIZED VIEW",
            Statement::RenameView { .. } => "ALTER VIEW",
            Statement::RenameIndex { .. } => "ALTER INDEX",
            Statement::AlterRelOptions { object, .. } => crate::reloptions::alter_tag(*object),
            Statement::AlterSequence { .. } => "ALTER SEQUENCE",
            Statement::CreateFunction(cf) if cf.procedure => "CREATE PROCEDURE",
            Statement::CreateFunction(_) => "CREATE FUNCTION",
            Statement::DropFunction { .. } => "DROP FUNCTION",
            Statement::CreateTrigger(_) => "CREATE TRIGGER",
            Statement::DropTrigger { .. } => "DROP TRIGGER",
            Statement::RenameTrigger { .. } => "ALTER TRIGGER",
            Statement::CreateIndex(_) => "CREATE INDEX",
            Statement::CreateSequence(_) => "CREATE SEQUENCE",
            Statement::AlterTable(_) => "ALTER TABLE",
            Statement::CreateType(_) => "CREATE TYPE",
            Statement::CreateDomain(_) => "CREATE DOMAIN",
            Statement::AlterType(_) => "ALTER TYPE",
            Statement::AlterDomain(_) => "ALTER DOMAIN",
            Statement::Drop(d) => match d.object_type {
                ir::ObjectType::Table => "DROP TABLE",
                ir::ObjectType::View => "DROP VIEW",
                ir::ObjectType::MaterializedView => "DROP MATERIALIZED VIEW",
                ir::ObjectType::Index => "DROP INDEX",
                ir::ObjectType::Sequence => "DROP SEQUENCE",
                ir::ObjectType::Schema => "DROP SCHEMA",
                ir::ObjectType::Type => "DROP TYPE",
                ir::ObjectType::Domain => "DROP DOMAIN",
                ir::ObjectType::Extension => "DROP EXTENSION",
                ir::ObjectType::Function => "DROP FUNCTION",
            },
            Statement::Truncate(_) => "TRUNCATE TABLE",
            Statement::CreateSchema(_) => "CREATE SCHEMA",
            Statement::AlterSchema { .. } => "ALTER SCHEMA",
            Statement::SetSchema { .. } => "ALTER TABLE",
            Statement::Comment { .. } => "COMMENT",
            Statement::Query(q) => match q.locking.first().map(|l| l.strength) {
                Some(ir::LockStrength::Update) => "SELECT FOR UPDATE",
                Some(ir::LockStrength::NoKeyUpdate) => "SELECT FOR NO KEY UPDATE",
                Some(ir::LockStrength::Share) => "SELECT FOR SHARE",
                Some(ir::LockStrength::KeyShare) => "SELECT FOR KEY SHARE",
                None => return None,
            },
            _ => return None,
        })
    }

    /// Drops the session's temporary objects (at disconnect).
    pub fn drop_temp_objects(&mut self) {
        let Some(temp) = self.temp_schema.clone() else { return };
        if self.db.current().catalog.schema(&temp).is_none() {
            return;
        }
        self.txn = None;
        let mut txn = self.db.begin(Isolation::ReadCommitted);
        txn.begin_statement();
        crate::ddl::drop_schema_contents(&mut txn, &temp);
        txn.catalog_mut().drop_schema(&temp);
        let _ = txn.commit();
    }

    fn transaction(&mut self, t: &TransactionStmt) -> SqlResult<QueryResult> {
        let tag = |s: &str| QueryResult { tag: s.to_owned(), ..Default::default() };
        let warning = |msg: &str, c: &'static str| Notice { severity: "WARNING", code: c, message: msg.to_owned() };
        match t {
            TransactionStmt::Begin { isolation, read_only } => {
                if self.txn.is_some() && !self.implicit_block && !self.pipeline {
                    let mut r = tag("BEGIN");
                    r.notices.push(warning("there is already a transaction in progress", code::ACTIVE_SQL_TRANSACTION));
                    return Ok(r);
                }
                self.implicit_block = false;
                self.pipeline = false;
                self.ensure_txn();
                if let Some(ro) = read_only {
                    self.set_read_only(*ro);
                }
                if let Some(level) = isolation {
                    self.set_isolation(*level)?;
                }
                self.status = TxStatus::InBlock;
                Ok(tag("BEGIN"))
            }
            TransactionStmt::Commit { chain } => self.end_block(true, *chain),
            TransactionStmt::Rollback { chain } => self.end_block(false, *chain),
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
                self.setting_marks.push(Mark {
                    name: name.clone(),
                    settings: self.setting_undo.len(),
                    listens: self.pending_listen.len(),
                    notifies: self.pending_notify.len(),
                });
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
                if let Some(i) = self.setting_marks.iter().rposition(|m| m.name == *name) {
                    self.setting_marks.truncate(i);
                }
                Ok(tag("RELEASE"))
            }
            TransactionStmt::RollbackTo(name) => {
                let txn = self.txn.as_mut().filter(|_| self.status != TxStatus::Idle).ok_or_else(|| {
                    SqlError::new(code::NO_ACTIVE_SQL_TRANSACTION, "ROLLBACK TO SAVEPOINT can only be used in transaction blocks")
                })?;
                txn.rollback_to(name)?;
                self.status = TxStatus::InBlock;
                if let Some(i) = self.setting_marks.iter().rposition(|m| m.name == *name) {
                    let (mark, listens, notifies) =
                        (self.setting_marks[i].settings, self.setting_marks[i].listens, self.setting_marks[i].notifies);
                    self.rollback_objects_to(i, listens, notifies);
                    self.setting_marks.truncate(i + 1);
                    while self.setting_undo.len() > mark {
                        if let Some(u) = self.setting_undo.pop() {
                            self.restore_setting(&u.name, u.old);
                        }
                    }
                }
                Ok(tag("ROLLBACK"))
            }
            TransactionStmt::SetTransaction { isolation, read_only } => {
                if self.txn.is_none() {
                    let mut r = tag("SET");
                    r.notices.push(warning("SET TRANSACTION can only be used in transaction blocks", code::NO_ACTIVE_SQL_TRANSACTION));
                    return Ok(r);
                }
                if let Some(level) = isolation {
                    self.set_isolation(*level)?;
                }
                if let Some(ro) = read_only {
                    self.set_read_only(*ro);
                }
                Ok(tag("SET"))
            }
        }
    }

    /// COMMIT / ROLLBACK [AND [NO] CHAIN]. A chained transaction keeps the characteristics of
    /// the one that ended.
    fn end_block(&mut self, commit: bool, chain: bool) -> SqlResult<QueryResult> {
        let name = if commit { "COMMIT" } else { "ROLLBACK" };
        let Some(txn) = self.txn.take() else {
            if chain {
                return Err(SqlError::new(
                    code::NO_ACTIVE_SQL_TRANSACTION,
                    format!("{name} AND CHAIN can only be used in transaction blocks"),
                ));
            }
            return Ok(QueryResult {
                tag: name.into(),
                notices: vec![Notice {
                    severity: "WARNING",
                    code: code::NO_ACTIVE_SQL_TRANSACTION,
                    message: "there is no transaction in progress".into(),
                }],
                ..Default::default()
            });
        };
        let failed = self.status == TxStatus::Failed;
        let isolation = self.settings.get("transaction_isolation").cloned().unwrap_or_default();
        let read_only = self.txn_read_only;
        self.status = TxStatus::Idle;
        self.pipeline = false;
        self.end_transaction_settings(commit && !failed);
        self.finish_txn(txn, commit && !failed)?;
        if chain {
            self.ensure_txn();
            self.status = TxStatus::InBlock;
            // COMMIT AND CHAIN of an aborted transaction starts a default one; ROLLBACK AND
            // CHAIN keeps the characteristics.
            if !(commit && failed) {
                let level = match isolation.as_str() {
                    "repeatable read" => ir::IsolationLevel::RepeatableRead,
                    "serializable" => ir::IsolationLevel::Serializable,
                    "read uncommitted" => ir::IsolationLevel::ReadUncommitted,
                    _ => ir::IsolationLevel::ReadCommitted,
                };
                self.set_isolation(level)?;
                self.set_read_only(read_only);
            }
        }
        Ok(QueryResult { tag: if commit && failed { "ROLLBACK" } else { name }.into(), ..Default::default() })
    }

    /// READ ONLY / READ WRITE for the current transaction.
    fn set_read_only(&mut self, read_only: bool) {
        self.txn_read_only = read_only;
        self.settings.insert("transaction_read_only".into(), if read_only { "on" } else { "off" }.into());
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

    /// The stored value of a setting: set in the session, else its default.
    pub fn setting_value(&self, name: &str) -> Option<String> {
        let key = name.to_ascii_lowercase();
        self.settings.get(&key).cloned().or_else(|| crate::settings::lookup(&key).and_then(|d| d.boot).map(str::to_owned))
    }

    /// Inside a transaction block (explicit, or a multi-statement simple query).
    fn in_block(&self) -> bool {
        self.implicit_block || (self.txn.is_some() && self.status != TxStatus::Idle)
    }

    fn restore_setting(&mut self, name: &str, old: Option<String>) {
        match old {
            Some(v) => {
                self.settings.insert(name.to_owned(), v);
            }
            // A custom placeholder outlives the transaction that created it, as empty.
            None if crate::settings::is_custom(name) => {
                self.settings.insert(name.to_owned(), String::new());
            }
            None => {
                self.settings.remove(name);
            }
        }
        if name == "default_transaction_isolation" {
            self.default_isolation = isolation_of(self.settings.get(name).map_or("read committed", String::as_str));
        }
    }

    /// Records a change for transaction / savepoint undo, then applies it.
    fn store_setting(&mut self, name: &str, value: String, local: bool) {
        if self.in_block() {
            let old = self.settings.get(name).cloned();
            self.setting_undo.push(SettingUndo { name: name.to_owned(), old, local });
        }
        self.settings.insert(name.to_owned(), value);
    }

    /// Validates `value` for setting `name` (lower case): the stored form.
    pub fn validate_setting(&self, name: &str, raw: &str) -> SqlResult<String> {
        validate_setting(name, raw, self.setting_value(name).as_deref())
    }

    /// Applies `set_config` calls made by a statement.
    fn apply_pending_settings(&mut self, pending: Vec<(String, String, bool)>, autocommit: bool) -> Vec<(String, String)> {
        let mut reported = Vec::new();
        for (name, value, local) in pending {
            if local && autocommit {
                // SET LOCAL semantics: gone with the statement's own transaction.
                if crate::settings::is_custom(&name) && !self.settings.contains_key(&name) {
                    self.settings.insert(name, String::new());
                }
                continue;
            }
            if name == "default_transaction_isolation" {
                self.default_isolation = isolation_of(&value);
            }
            self.store_setting(&name, value, local);
            if is_reported(&name) {
                reported.push((setting_display_name(&name).to_owned(), self.show_value(&name)));
            }
        }
        reported
    }
}

/// Validates `raw` for setting `name` (lower case) given its current value: the stored form.
pub fn validate_setting(name: &str, raw: &str, current: Option<&str>) -> SqlResult<String> {
    if crate::settings::is_custom(name) && crate::settings::lookup(name).is_none() {
        return Ok(raw.to_owned());
    }
    let def = crate::settings::lookup(name).ok_or_else(|| crate::settings::unrecognized(name))?;
    crate::settings::check_settable(def)?;
    match name {
        "timezone" => timezone_setting(raw),
        "client_encoding" => {
            if raw.eq_ignore_ascii_case("utf8") || raw.eq_ignore_ascii_case("utf-8") || raw.eq_ignore_ascii_case("unicode") {
                Ok("UTF8".to_owned())
            } else {
                Err(SqlError::feature_not_supported(format!("conversion to encoding \"{raw}\" is not supported by pgmem")))
            }
        }
        "application_name" => Ok(raw.chars().filter(|c| (' '..='~').contains(c)).take(63).collect()),
        _ => crate::settings::normalize(def, raw, current),
    }
}

impl Session {
    fn set(&mut self, name: &str, value: &[String], local: bool) -> SqlResult<QueryResult> {
        let name = name.to_ascii_lowercase();
        let name = match name.as_str() {
            "names" => "client_encoding".to_owned(),
            "time zone" => "timezone".to_owned(),
            "session characteristics as transaction isolation level" => "default_transaction_isolation".to_owned(),
            _ => name,
        };
        if name == "role" || name == "session_authorization" {
            let target = value.join(", ");
            let catalog = self.catalog_snapshot();
            let env = self.env();
            if name == "role" {
                crate::roles::check_set_role(&catalog, &env, &target)?;
                let stored =
                    if target.eq_ignore_ascii_case("none") || target.eq_ignore_ascii_case("default") { "none".to_owned() } else { target };
                self.store_setting("role", stored, local);
            } else {
                if !target.eq_ignore_ascii_case("default") && !crate::roles::role_exists(&catalog, &env, &target) {
                    return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("role \"{target}\" does not exist")));
                }
                self.store_setting("role", "none".to_owned(), local);
            }
            return Ok(QueryResult { tag: "SET".into(), ..Default::default() });
        }
        if name == "transaction_isolation" {
            let level = match value.join(", ").to_ascii_lowercase().as_str() {
                "repeatable read" => ir::IsolationLevel::RepeatableRead,
                "serializable" => ir::IsolationLevel::Serializable,
                "read uncommitted" => ir::IsolationLevel::ReadUncommitted,
                "read committed" => ir::IsolationLevel::ReadCommitted,
                other => {
                    return Err(SqlError::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for parameter \"transaction_isolation\": \"{other}\""),
                    ));
                }
            };
            self.set_isolation(level)?;
            return Ok(QueryResult { tag: "SET".into(), ..Default::default() });
        }
        if local && !self.in_block() {
            // Validated anyway, then ignored outside a transaction block.
            if crate::settings::lookup(&name).is_none() && !crate::settings::is_custom(&name) {
                return Err(crate::settings::unrecognized(&name));
            }
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
        let stored = if value.len() == 1 && value[0].eq_ignore_ascii_case("default") {
            if crate::settings::lookup(&name).is_none() && !crate::settings::is_custom(&name) {
                return Err(crate::settings::unrecognized(&name));
            }
            if let Some(def) = crate::settings::lookup(&name) {
                crate::settings::check_settable(def)?;
            }
            default_settings()
                .get(&name)
                .cloned()
                .or_else(|| crate::settings::lookup(&name).and_then(|d| d.boot).map(str::to_owned))
                .unwrap_or_default()
        } else {
            let joined = if name == "search_path" {
                value
                    .iter()
                    .map(|v| if v.contains('$') || v.chars().any(|c| c.is_ascii_uppercase()) { format!("\"{v}\"") } else { v.clone() })
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                value.join(", ")
            };
            self.validate_setting(&name, &joined)?
        };
        if name == "default_transaction_isolation" {
            self.default_isolation = isolation_of(&stored);
            if self.txn.is_none() {
                self.settings.insert("transaction_isolation".into(), stored.clone());
            }
        }
        self.store_setting(&name, stored.clone(), local);
        let reported =
            if is_reported(&name) { vec![(setting_display_name(&name).to_owned(), self.show_value(&name))] } else { Default::default() };
        Ok(QueryResult { tag: "SET".into(), changed_settings: reported, ..Default::default() })
    }

    /// A setting as SHOW prints it.
    pub fn show_value(&self, name: &str) -> String {
        crate::settings::show(crate::settings::lookup(name), &self.setting_value(name).unwrap_or_default())
    }

    fn reset(&mut self, name: Option<&str>) -> SqlResult<QueryResult> {
        let defaults = default_settings();
        match name {
            None => {
                let keys: Vec<String> = self.settings.keys().cloned().collect();
                for k in keys {
                    let settable = crate::settings::lookup(&k).is_none_or(|d| crate::settings::check_settable(d).is_ok());
                    if k == "session_authorization" || !settable {
                        continue;
                    }
                    let v = defaults.get(&k).cloned();
                    match v {
                        Some(v) => self.store_setting(&k, v, false),
                        None if crate::settings::is_custom(&k) => self.store_setting(&k, String::new(), false),
                        None => {
                            if self.in_block() {
                                let old = self.settings.get(&k).cloned();
                                self.setting_undo.push(SettingUndo { name: k.clone(), old, local: false });
                            }
                            self.settings.remove(&k);
                        }
                    }
                }
                self.default_isolation = Isolation::ReadCommitted;
            }
            Some(n) => {
                let n = n.to_ascii_lowercase();
                let n = if n == "time zone" { "timezone".to_owned() } else { n };
                if n == "role" || n == "session_authorization" {
                    // Not in pg_settings; SET ROLE / SESSION AUTHORIZATION state.
                    if n == "role" {
                        self.settings.remove(&n);
                    } else {
                        self.settings.insert(n, self.user.clone());
                    }
                    return Ok(QueryResult { tag: "RESET".into(), ..Default::default() });
                }
                match crate::settings::lookup(&n) {
                    Some(def) => crate::settings::check_settable(def)?,
                    None if crate::settings::is_custom(&n) => {}
                    None => return Err(crate::settings::unrecognized(&n)),
                }
                match defaults.get(&n) {
                    Some(v) => self.store_setting(&n, v.clone(), false),
                    None if crate::settings::is_custom(&n) => self.store_setting(&n, String::new(), false),
                    None => {
                        if self.in_block() {
                            let old = self.settings.get(&n).cloned();
                            self.setting_undo.push(SettingUndo { name: n.clone(), old, local: false });
                        }
                        self.settings.remove(&n);
                    }
                }
                if n == "default_transaction_isolation" {
                    self.default_isolation = Isolation::ReadCommitted;
                }
            }
        }
        Ok(QueryResult { tag: "RESET".into(), ..Default::default() })
    }

    fn show(&self, name: &str) -> SqlResult<QueryResult> {
        let key = show_key(name);
        if key == "all" {
            let mut rows = Vec::new();
            for d in crate::settings::defs() {
                let k = d.name.to_ascii_lowercase();
                rows.push(vec![
                    Value::Text(d.name.to_owned()),
                    Value::Text(self.show_value(&k)),
                    d.short_desc.map_or(Value::Null, |s| Value::Text(s.to_owned())),
                ]);
            }
            return Ok(QueryResult {
                columns: vec![OutCol::new("name", Type::Text), OutCol::new("setting", Type::Text), OutCol::new("description", Type::Text)],
                returns_rows: true,
                rows,
                tag: "SHOW".into(),
                ..Default::default()
            });
        }
        let known = crate::settings::lookup(&key).is_some() || self.settings.contains_key(&key);
        if !known {
            return Err(crate::settings::unrecognized(&key));
        }
        let header = crate::settings::lookup(&key).map_or_else(|| key.clone(), |d| d.name.to_owned());
        Ok(QueryResult {
            columns: vec![OutCol::new(&header, Type::Text)],
            returns_rows: true,
            rows: vec![vec![Value::Text(self.show_value(&key))]],
            tag: "SHOW".into(),
            ..Default::default()
        })
    }

    /// Rows of a table, for the `seed` test helper's validation.
    pub fn table_exists(&self, name: &str) -> bool {
        let (schema, table) = name.split_once('.').unwrap_or((pgmem_catalog::PUBLIC_SCHEMA, name));
        matches!(self.db.current().catalog.relation(schema, table), Some(Relation::Table(_)))
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
        "session authorization" => "session_authorization".to_owned(),
        _ => key,
    }
}

/// One setting change inside a transaction block, for undo.
struct SettingUndo {
    name: String,
    old: Option<String>,
    local: bool,
}

fn isolation_of(v: &str) -> Isolation {
    match v.to_ascii_lowercase().as_str() {
        "repeatable read" => Isolation::RepeatableRead,
        "serializable" => Isolation::Serializable,
        _ => Isolation::ReadCommitted,
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

#[allow(dead_code)]
fn unused(_: ConstraintKind) {}

fn ensure_temp_schema(txn: &mut Txn, temp: &str, user: &str) {
    if txn.catalog().schema(temp).is_none() {
        let cat = txn.catalog_mut();
        let oid = cat.alloc_oid();
        let _ = cat.create_schema(pgmem_catalog::SchemaDef { oid, name: temp.to_owned(), owner: user.to_owned() });
    }
}

/// `CREATE SCHEMA s CREATE TABLE ... CREATE VIEW ...`: the elements are created with the new
/// schema as the only search path entry, as Postgres does.
fn create_schema_with_elements(
    cs: &ir::CreateSchema,
    txn: &mut Txn,
    user: &str,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let result = crate::ddl::create_schema(cs, txn, user)?;
    let Some(schema) = cs.name.clone().or_else(|| cs.authorization.clone()) else { return Ok(result) };
    if !result.notices.is_empty() {
        return Ok(result);
    }
    let mut ctx = pgmem_types::context::current();
    ctx.search_path.entries = vec![schema];
    let _g = pgmem_types::context::enter(ctx);
    for element in &cs.elements {
        let catalog = Arc::new(txn.catalog().clone());
        let binder = Binder::new(&catalog, &[]);
        crate::ddl::execute_ddl(element, txn, &binder, env, state)?;
    }
    Ok(result)
}

impl Drop for Session {
    /// A session's temporary objects disappear when it ends.
    fn drop(&mut self) {
        self.unregister_listener();
        self.drop_temp_objects();
    }
}
