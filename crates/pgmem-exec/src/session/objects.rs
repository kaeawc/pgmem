//! Session statements over per-session objects: prepared statements, cursors, LISTEN /
//! NOTIFY, DISCARD, EXPLAIN and the maintenance commands.

use super::{QueryResult, Session, TxStatus};
use crate::bind::{self, Binder, Ctx, Level, Scope};
use crate::cast::{CoercionContext, cast_context};
use crate::eval::{EvalCtx, StatementState};
use crate::plan::OutCol;
use crate::portal::{Cursor, CursorInfo, PreparedStatement};
use pgmem_ir::{self as ir, Statement};
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use std::collections::BTreeSet;
use std::sync::Arc;

/// What the session-object catalog views and functions see (`pg_prepared_statements`,
/// `pg_cursors`, `pg_listening_channels()`).
#[derive(Clone, Debug, Default)]
pub struct SessionObjects {
    pub prepared: Vec<(String, Arc<PreparedStatement>)>,
    pub cursors: Vec<CursorInfo>,
    pub listening: Vec<String>,
}

/// A pending LISTEN (true) / UNLISTEN (false) of a channel; `None` = UNLISTEN *.
pub(super) type ListenOp = Option<(String, bool)>;

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

fn no_prepared(name: &str) -> SqlError {
    SqlError::new(code::INVALID_SQL_STATEMENT_NAME, format!("prepared statement \"{name}\" does not exist"))
}

fn no_cursor(name: &str) -> SqlError {
    SqlError::new(code::INVALID_CURSOR_NAME, format!("cursor \"{name}\" does not exist"))
}

/// Without SCROLL or NO SCROLL, Postgres allows backward fetches when the plan supports
/// them; a query without FROM (a bare Result node) does not.
fn default_scrollable(q: &ir::Query) -> bool {
    match &q.body {
        ir::SetExpr::Select(s) => !s.from.is_empty() && s.group_by.is_empty(),
        ir::SetExpr::Values(_) => true,
        _ => false,
    }
}

impl Session {
    /// The session objects snapshot builtins and catalog views read.
    pub(super) fn objects(&self) -> Arc<SessionObjects> {
        Arc::new(SessionObjects {
            prepared: self.prepared.iter().filter(|(n, _)| !n.is_empty()).map(|(n, p)| (n.clone(), Arc::clone(p))).collect(),
            cursors: self
                .cursors
                .values()
                .map(|c| CursorInfo { current: c.current_row_id(), ..c.info.clone() })
                .chain(self.protocol_portals.values().cloned())
                .collect(),
            listening: self.listening.iter().cloned().collect(),
        })
    }

    // ---- prepared statements -------------------------------------------------------------

    /// A prepared statement by name (the unnamed one is `""`).
    pub fn prepared(&self, name: &str) -> Option<Arc<PreparedStatement>> {
        self.prepared.get(name).cloned()
    }

    /// Protocol `Parse`: prepares `stmt` under `name` (the unnamed statement is replaced).
    pub fn prepare_protocol(&mut self, name: &str, text: &str, stmt: Statement, declared: &[Type]) -> SqlResult<Arc<PreparedStatement>> {
        if !name.is_empty() && self.prepared.contains_key(name) {
            return Err(SqlError::new(code::DUPLICATE_PREPARED_STATEMENT, format!("prepared statement \"{name}\" already exists")));
        }
        let desc = self.describe(&stmt, declared)?;
        let p = Arc::new(PreparedStatement {
            stmt,
            param_types: desc.param_types,
            columns: desc.columns,
            text: text.to_owned(),
            from_sql: false,
            prepare_time: self.clock.now_micros(),
        });
        self.prepared.insert(name.to_owned(), Arc::clone(&p));
        Ok(p)
    }

    /// Protocol `Bind`: a portal over prepared statement text `text`.
    pub fn open_portal(&mut self, name: &str, text: &str) {
        let info = CursorInfo {
            name: name.to_owned(),
            text: text.to_owned(),
            hold: false,
            binary: false,
            scroll: false,
            creation_time: self.clock.now_micros(),
            current: None,
            updatable: false,
        };
        self.protocol_portals.insert(name.to_owned(), info);
    }

    /// Protocol `Close` of a portal, or the unnamed portal's end at Sync.
    pub fn close_portal(&mut self, name: &str) {
        self.protocol_portals.remove(name);
    }

    /// Protocol `Close` of a statement (no error when it does not exist).
    pub fn close_prepared(&mut self, name: &str) {
        self.prepared.remove(name);
    }

    /// SQL `PREPARE`.
    pub(super) fn prepare_sql(&mut self, p: &ir::Prepare) -> SqlResult<QueryResult> {
        if !matches!(p.statement, Statement::Query(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_)) {
            return Err(SqlError::syntax("syntax error at or near \"AS\""));
        }
        if self.prepared.contains_key(&p.name) {
            return Err(SqlError::new(code::DUPLICATE_PREPARED_STATEMENT, format!("prepared statement \"{}\" already exists", p.name)));
        }
        let catalog = self.catalog_snapshot();
        let declared = {
            let _ctx = pgmem_types::context::enter(self.format_context());
            p.param_types.iter().map(|t| bind::resolve_type_name(&catalog, t).map(|(ty, _)| ty)).collect::<SqlResult<Vec<_>>>()?
        };
        // Parameters whose type the statement leaves open are text, as in Postgres.
        let desc = self.describe_lenient(&p.statement, &declared)?;
        let param_types = desc.param_types.into_iter().map(|t| if t == Type::Unknown { Type::Text } else { t }).collect();
        let prepared = PreparedStatement {
            stmt: p.statement.clone(),
            param_types,
            columns: desc.columns,
            text: p.text.clone(),
            from_sql: true,
            prepare_time: self.clock.now_micros(),
        };
        self.prepared.insert(p.name.clone(), Arc::new(prepared));
        Ok(tag("PREPARE"))
    }

    pub(super) fn deallocate(&mut self, name: Option<&str>) -> SqlResult<QueryResult> {
        match name {
            Some(n) => {
                self.prepared.remove(n).ok_or_else(|| no_prepared(n))?;
                Ok(tag("DEALLOCATE"))
            }
            None => {
                self.prepared.clear();
                Ok(tag("DEALLOCATE ALL"))
            }
        }
    }

    /// SQL `EXECUTE name (args)`: the arguments are coerced to the parameter types by
    /// assignment rules, then the statement runs with them.
    pub(super) fn execute_prepared(&mut self, name: &str, args: &[ir::Expr]) -> SqlResult<QueryResult> {
        let p = self.prepared.get(name).cloned().ok_or_else(|| no_prepared(name))?;
        if args.len() != p.param_types.len() {
            return Err(SqlError::syntax(format!("wrong number of parameters for prepared statement \"{name}\"")).with_detail(format!(
                "Expected {} parameters but got {}.",
                p.param_types.len(),
                args.len()
            )));
        }
        let values = self.eval_args(args, &p.param_types)?;
        let mut r = self.run_in_txn(&p.stmt, &p.param_types, &values)?;
        if let Statement::Query(_) = p.stmt {
            r.tag = format!("SELECT {}", r.rows.len());
        }
        Ok(r)
    }

    fn eval_args(&self, args: &[ir::Expr], types: &[Type]) -> SqlResult<Vec<Value>> {
        let env = self.env();
        let state = StatementState::default();
        let txn = self.txn.as_ref().ok_or_else(|| SqlError::internal("no transaction"))?;
        let catalog = Arc::new(txn.catalog().clone());
        let binder = Binder::new(&catalog, &[]);
        let top = Level::new(Scope::default());
        let ctx = Ctx::new(&top);
        let ectx = EvalCtx::new(txn, &[], &env, &state);
        let mut values = Vec::with_capacity(args.len());
        for (i, (a, ty)) in args.iter().zip(types).enumerate() {
            let (e, from) = binder.bind_expr(a, &ctx)?;
            if from != Type::Unknown && cast_context(&from, ty).is_none_or(|c| c > CoercionContext::Assignment) {
                return Err(SqlError::datatype_mismatch(format!(
                    "parameter ${} of type {} cannot be coerced to the expected type {}",
                    i + 1,
                    from.name(),
                    ty.name()
                ))
                .with_hint("You will need to rewrite or cast the expression."));
            }
            let e = bind::coerce(&binder, e, &from, ty, CoercionContext::Assignment)?;
            values.push(crate::eval::eval(&e, &[], &ectx)?);
        }
        Ok(values)
    }

    // ---- cursors -------------------------------------------------------------------------

    pub(super) fn declare_cursor(&mut self, d: &ir::DeclareCursor) -> SqlResult<QueryResult> {
        if !d.hold && !self.in_block() {
            return Err(SqlError::new(code::NO_ACTIVE_SQL_TRANSACTION, "DECLARE CURSOR can only be used in transaction blocks"));
        }
        if self.cursors.contains_key(&d.name) {
            return Err(SqlError::new(code::DUPLICATE_CURSOR, format!("cursor \"{}\" already exists", d.name)));
        }
        let (r, row_ids) = self.cursor_rows(&d.query)?;
        let info = CursorInfo {
            name: d.name.clone(),
            text: d.text.clone(),
            hold: d.hold,
            binary: d.binary,
            scroll: d.scroll.unwrap_or_else(|| default_scrollable(&d.query)),
            creation_time: self.clock.now_micros(),
            current: None,
            updatable: !row_ids.is_empty(),
        };
        let cursor = Cursor::new(info, r.columns, r.rows, self.setting_marks.len()).with_row_ids(row_ids);
        self.cursors.insert(d.name.clone(), cursor);
        Ok(tag("DECLARE CURSOR"))
    }

    /// A cursor query's result and, for a plain SELECT from one table, each row's table OID
    /// and `ctid` (what `WHERE CURRENT OF` compares with).
    fn cursor_rows(&mut self, q: &ir::Query) -> SqlResult<(QueryResult, Vec<Option<crate::portal::RowIdentity>>)> {
        let plain = |q: &ir::Query| -> bool {
            let simple_scan = q.order_by.is_empty() && q.limit.is_none() && q.offset.is_none();
            (simple_scan || !q.locking.is_empty())
                && matches!(&q.body, ir::SetExpr::Select(s)
                if s.distinct == ir::Distinct::All && s.group_by.is_empty() && s.having.is_none()
                    && matches!(s.from.as_slice(), [ir::TableRef::Table { .. }]))
        };
        if plain(q) {
            let mut with_ids = q.clone();
            if let ir::SetExpr::Select(s) = &mut with_ids.body {
                for col in ["tableoid", "ctid"] {
                    s.projection.push(ir::SelectItem::Expr { expr: ir::Expr::Column(vec![col.to_owned()]), alias: None });
                }
            }
            if let Ok(mut r) = self.run_in_txn(&Statement::Query(Box::new(with_ids)), &[], &[]) {
                let ids = r
                    .rows
                    .iter_mut()
                    .map(|row| {
                        let ctid = row.pop();
                        match (row.pop(), ctid) {
                            (Some(Value::Oid(t)), Some(c)) => Some((t, c)),
                            _ => None,
                        }
                    })
                    .collect();
                r.columns.truncate(r.columns.len().saturating_sub(2));
                return Ok((r, ids));
            }
        }
        let r = self.run_in_txn(&Statement::Query(Box::new(q.clone())), &[], &[])?;
        Ok((r, Vec::new()))
    }

    /// The columns a FETCH from `name` returns (Describe).
    pub(super) fn cursor_columns(&self, name: &str) -> SqlResult<Vec<OutCol>> {
        self.cursors.get(name).map(|c| c.columns.clone()).ok_or_else(|| no_cursor(name))
    }

    pub(super) fn fetch(&mut self, name: &str, direction: ir::FetchDirection, is_move: bool) -> SqlResult<QueryResult> {
        let cursor = self.cursors.get_mut(name).ok_or_else(|| no_cursor(name))?;
        let fetched = cursor.fetch(direction, is_move)?;
        if is_move {
            return Ok(tag(&format!("MOVE {}", fetched.count)));
        }
        Ok(QueryResult {
            columns: cursor.columns.clone(),
            returns_rows: true,
            rows: fetched.rows,
            tag: format!("FETCH {}", fetched.count),
            binary: cursor.info.binary,
            ..Default::default()
        })
    }

    pub(super) fn close_cursor(&mut self, name: Option<&str>) -> SqlResult<QueryResult> {
        match name {
            Some(n) => {
                self.cursors.remove(n).ok_or_else(|| no_cursor(n))?;
                Ok(tag("CLOSE CURSOR"))
            }
            None => {
                self.cursors.clear();
                Ok(tag("CLOSE CURSOR ALL"))
            }
        }
    }

    // ---- LISTEN / NOTIFY -----------------------------------------------------------------

    pub(super) fn listen(&mut self, op: ListenOp) -> QueryResult {
        let t = match &op {
            Some((_, true)) => "LISTEN",
            _ => "UNLISTEN",
        };
        self.pending_listen.push(op);
        if self.txn.is_none() {
            self.apply_listens();
        }
        tag(t)
    }

    fn apply_listens(&mut self) {
        for op in std::mem::take(&mut self.pending_listen) {
            match op {
                Some((ch, true)) => {
                    self.listening.insert(ch);
                }
                Some((ch, false)) => {
                    self.listening.remove(&ch);
                }
                None => self.listening.clear(),
            }
        }
        self.db.notifications().set_channels(self.backend_pid, &self.listening);
    }

    pub(super) fn notify(&mut self, channel: &str, payload: Option<&str>) -> SqlResult<QueryResult> {
        let payload = payload.unwrap_or("");
        crate::portal::check_notify(channel, payload)?;
        self.queue_notification(channel.to_owned(), payload.to_owned());
        if self.txn.is_none() {
            let pending = std::mem::take(&mut self.pending_notify);
            self.db.notifications().publish(self.backend_pid, &pending);
        }
        Ok(tag("NOTIFY"))
    }

    /// Queues a notification for commit; duplicates within a transaction are folded.
    pub(super) fn queue_notification(&mut self, channel: String, payload: String) {
        if !self.pending_notify.iter().any(|(c, p)| *c == channel && *p == payload) {
            self.pending_notify.push((channel, payload));
        }
    }

    /// Notifications delivered to this session since the last call.
    pub fn take_notifications(&self) -> Vec<pgmem_storage::Notification> {
        self.db.notifications().take(self.backend_pid)
    }

    /// Ends the current transaction: commits or rolls back the data, closes the cursors that
    /// do not survive it, and (on commit) applies LISTEN / UNLISTEN and sends notifications.
    pub(super) fn finish_txn(&mut self, mut txn: Txn, commit: bool) -> SqlResult<()> {
        let pending = std::mem::take(&mut self.pending_checks);
        self.constraint_modes = super::ConstraintModes::default();
        let result = if commit {
            // Deferred constraints are checked at commit; a violation rolls everything back.
            match self.check_constraints(&mut txn, pending) {
                Ok(()) => {
                    self.apply_on_commit(&mut txn);
                    txn.commit()
                }
                Err(e) => {
                    txn.rollback();
                    Err(e)
                }
            }
        } else {
            txn.rollback();
            Ok(())
        };
        let committed = commit && result.is_ok();
        self.db.advisory().release(self.backend_pid, true);
        self.cursors.retain(|_, c| c.info.hold && (committed || !c.in_txn));
        for c in self.cursors.values_mut() {
            c.in_txn = false;
        }
        if committed {
            self.apply_listens();
            let pending = std::mem::take(&mut self.pending_notify);
            if !pending.is_empty() {
                self.db.notifications().publish(self.backend_pid, &pending);
            }
        } else {
            self.pending_listen.clear();
            self.pending_notify.clear();
        }
        result
    }

    /// Temporary tables' ON COMMIT actions, run just before the commit.
    fn apply_on_commit(&mut self, txn: &mut Txn) {
        let entries = std::mem::take(&mut self.on_commit);
        for (oid, action) in entries {
            let Some(t) = txn.catalog().table_by_oid(oid).cloned() else { continue };
            match action {
                pgmem_ir::OnCommit::DeleteRows => {
                    txn.truncate(oid);
                    self.on_commit.push((oid, action));
                }
                pgmem_ir::OnCommit::Drop => {
                    let cat = txn.catalog_mut();
                    cat.drop_comments(oid);
                    cat.drop_table(&t.qualified());
                }
                pgmem_ir::OnCommit::PreserveRows => {}
            }
        }
    }

    /// Runs deferred constraint checks: (table OID, constraint name) against the table's rows.
    fn check_constraints(&self, txn: &mut Txn, checks: Vec<(pgmem_catalog::Oid, String)>) -> SqlResult<()> {
        if checks.is_empty() {
            return Ok(());
        }
        let _ctx = pgmem_types::context::enter(self.format_context());
        let env = self.env();
        let state = StatementState::default();
        for (oid, name) in checks {
            let catalog = txn.catalog().clone();
            let Some(def) = catalog.table_by_oid(oid).cloned() else { continue };
            if def.constraints.iter().any(|c| c.name == name) {
                // A unique violation reads as an insertion's, not as index creation's.
                crate::ddl::validate_constraint(txn, &def, &name, &catalog, &env, &state).map_err(|e| {
                    if e.code == code::UNIQUE_VIOLATION && e.message.starts_with("could not create unique index") {
                        let detail = e.detail().unwrap_or_default().replace(" is duplicated.", " already exists.");
                        SqlError::new(code::UNIQUE_VIOLATION, format!("duplicate key value violates unique constraint \"{name}\""))
                            .with_detail(detail)
                    } else {
                        e
                    }
                })?;
            }
        }
        Ok(())
    }

    /// `SET CONSTRAINTS {ALL | names} {DEFERRED | IMMEDIATE}`; IMMEDIATE runs the checks owed
    /// so far.
    pub(super) fn set_constraints(&mut self, names: &[ir::ObjectName], deferred: bool) -> SqlResult<QueryResult> {
        // Outside a block it warns, still checks the names, and has no lasting effect.
        let in_block = self.in_block();
        let catalog = self.catalog_snapshot();
        let mut chosen: Vec<(pgmem_catalog::Oid, pgmem_catalog::Oid, String)> = Vec::new();
        if names.is_empty() {
            self.constraint_modes.all = Some(deferred);
            self.constraint_modes.by_oid.clear();
        } else {
            let _ctx = pgmem_types::context::enter(self.format_context());
            for n in names {
                let matches: Vec<_> = catalog
                    .tables()
                    .filter(|t| {
                        n.schema().is_none_or(|s| s == t.schema) && (n.schema().is_some() || catalog.relation_visible(&t.schema, &t.name))
                    })
                    .flat_map(|t| t.constraints.iter().filter(|c| c.name == n.base()).map(move |c| (t.oid, c)))
                    .collect();
                if matches.is_empty() {
                    return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("constraint \"{}\" does not exist", n.base())));
                }
                for (table, c) in matches {
                    if !c.deferrable {
                        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("constraint \"{}\" is not deferrable", c.name)));
                    }
                    chosen.push((table, c.oid, c.name.clone()));
                }
            }
            for (_, oid, _) in &chosen {
                self.constraint_modes.by_oid.insert(*oid, deferred);
            }
        }
        if !in_block {
            self.constraint_modes = super::ConstraintModes::default();
            return Ok(QueryResult {
                tag: "SET CONSTRAINTS".into(),
                notices: vec![super::Notice {
                    severity: "WARNING",
                    code: code::NO_ACTIVE_SQL_TRANSACTION,
                    message: "SET CONSTRAINTS can only be used in transaction blocks".into(),
                }],
                ..Default::default()
            });
        }
        if !deferred {
            let due: Vec<_> = if names.is_empty() {
                std::mem::take(&mut self.pending_checks)
            } else {
                let (due, rest) = std::mem::take(&mut self.pending_checks)
                    .into_iter()
                    .partition(|(t, n)| chosen.iter().any(|(ct, _, cn)| ct == t && cn == n));
                self.pending_checks = rest;
                due
            };
            if let Some(mut txn) = self.txn.take() {
                let result = self.check_constraints(&mut txn, due);
                self.txn = Some(txn);
                result?;
            }
        }
        Ok(tag("SET CONSTRAINTS"))
    }

    /// ROLLBACK TO the savepoint at `depth`: forgets what was queued after it.
    pub(super) fn rollback_objects_to(&mut self, depth: usize, listens: usize, notifies: usize) {
        self.cursors.retain(|_, c| c.savepoint_depth <= depth || !c.in_txn);
        self.pending_listen.truncate(listens);
        self.pending_notify.truncate(notifies);
    }

    // ---- DISCARD, EXPLAIN, maintenance ---------------------------------------------------

    pub(super) fn discard(&mut self, kind: ir::DiscardKind) -> SqlResult<QueryResult> {
        use ir::DiscardKind as K;
        match kind {
            K::Plans => Ok(tag("DISCARD PLANS")),
            K::Sequences => {
                self.currval.clear();
                self.lastval = None;
                Ok(tag("DISCARD SEQUENCES"))
            }
            K::Temp => {
                self.discard_temp();
                Ok(tag("DISCARD TEMP"))
            }
            K::All => {
                if self.in_block() {
                    return Err(SqlError::new(code::ACTIVE_SQL_TRANSACTION, "DISCARD ALL cannot run inside a transaction block"));
                }
                self.cursors.clear();
                self.prepared.clear();
                self.pending_listen.push(None);
                self.apply_listens();
                self.settings.insert("session_authorization".into(), self.user.clone());
                self.reset(None)?;
                self.currval.clear();
                self.lastval = None;
                self.discard_temp();
                Ok(tag("DISCARD ALL"))
            }
        }
    }

    /// Drops the session's temporary objects inside the current transaction.
    fn discard_temp(&mut self) {
        let Some(temp) = self.temp_schema.clone() else { return };
        let Some(txn) = self.txn.as_mut() else { return };
        if txn.catalog().schema(&temp).is_none() {
            return;
        }
        crate::ddl::drop_schema_contents(txn, &temp);
        txn.catalog_mut().drop_schema(&temp);
    }

    /// EXPLAIN: the statement is analyzed (and run, with ANALYZE); pgmem has no planner, so
    /// the plan is a single node.
    pub(super) fn explain(&mut self, e: &ir::Explain, param_types: &[Type], params: &[Value]) -> SqlResult<QueryResult> {
        let rows = if e.analyze {
            let r = self.run_in_txn(&e.statement, param_types, params)?;
            Some(r.rows.len())
        } else {
            match &e.statement {
                Statement::Execute { name, .. } => {
                    self.prepared.get(name).ok_or_else(|| no_prepared(name))?;
                }
                other => {
                    self.describe_lenient(other, param_types)?;
                }
            }
            None
        };
        Ok(explain_result(e.format, rows))
    }

    /// VACUUM / ANALYZE / CLUSTER / REINDEX: checked, otherwise no effect.
    pub(super) fn maintenance(&self, tag_name: &str, tables: &[ir::ObjectName], in_block_ok: bool) -> SqlResult<QueryResult> {
        if !in_block_ok && self.in_block() {
            return Err(SqlError::new(code::ACTIVE_SQL_TRANSACTION, format!("{tag_name} cannot run inside a transaction block")));
        }
        self.check_relations(tables)?;
        Ok(tag(tag_name))
    }

    pub(super) fn lock_tables(&self, tables: &[ir::ObjectName], mode: u8, nowait: bool) -> SqlResult<QueryResult> {
        if !self.in_block() {
            return Err(SqlError::new(code::NO_ACTIVE_SQL_TRANSACTION, "LOCK TABLE can only be used in transaction blocks"));
        }
        self.check_relations(tables)?;
        let _ctx = pgmem_types::context::enter(self.format_context());
        let catalog = self.catalog_snapshot();
        if let Some(txn) = self.txn.as_ref() {
            for t in tables {
                if let Some(rel) = catalog.find_relation(t)? {
                    txn.lock_table(rel.oid(), t.base(), mode, nowait)?;
                }
            }
        }
        Ok(tag("LOCK TABLE"))
    }

    fn check_relations(&self, tables: &[ir::ObjectName]) -> SqlResult<()> {
        let _ctx = pgmem_types::context::enter(self.format_context());
        let catalog = self.catalog_snapshot();
        for t in tables {
            if catalog.find_relation(t)?.is_none() {
                return Err(SqlError::undefined_table(&t.to_string()));
            }
        }
        Ok(())
    }

    /// Whether the session is outside any transaction (notifications are delivered then).
    pub fn is_idle(&self) -> bool {
        self.txn.is_none() && self.status == TxStatus::Idle
    }

    /// Forgets this session's LISTEN registrations (at disconnect).
    pub(super) fn unregister_listener(&self) {
        self.db.notifications().remove(self.backend_pid);
        self.db.advisory().release(self.backend_pid, false);
        self.db.advisory().release(self.backend_pid, true);
    }
}

/// The single-node plan EXPLAIN reports, in the requested format.
fn explain_result(format: ir::ExplainFormat, analyzed_rows: Option<usize>) -> QueryResult {
    use ir::ExplainFormat as F;
    let actual = analyzed_rows.map(|n| n.to_string());
    let (ty, lines): (Type, Vec<String>) = match format {
        F::Text => {
            let mut lines = vec![match &actual {
                Some(n) => format!("Result  (cost=0.00..0.01 rows=1 width=0) (actual time=0.000..0.000 rows={n} loops=1)"),
                None => "Result  (cost=0.00..0.01 rows=1 width=0)".to_owned(),
            }];
            if actual.is_some() {
                lines.push("Planning Time: 0.000 ms".to_owned());
                lines.push("Execution Time: 0.000 ms".to_owned());
            }
            (Type::Text, lines)
        }
        F::Json => {
            let extra = actual.as_ref().map_or_else(String::new, |n| format!(",\n      \"Actual Rows\": {n},\n      \"Actual Loops\": 1"));
            let plan = format!(
                "[\n  {{\n    \"Plan\": {{\n      \"Node Type\": \"Result\",\n      \"Startup Cost\": 0.00,\n      \"Total Cost\": 0.01,\n      \"Plan Rows\": 1,\n      \"Plan Width\": 0{extra}\n    }}\n  }}\n]"
            );
            (Type::Json, vec![plan])
        }
        F::Yaml => {
            let extra = actual.as_ref().map_or_else(String::new, |n| format!("\n    Actual Rows: {n}\n    Actual Loops: 1"));
            (Type::Text, vec![format!("- Plan: \n    Node Type: \"Result\"\n    Startup Cost: 0.00\n    Total Cost: 0.01\n    Plan Rows: 1\n    Plan Width: 0{extra}")])
        }
        F::Xml => (Type::Text, vec!["<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n  <Query>\n    <Plan>\n      <Node-Type>Result</Node-Type>\n    </Plan>\n  </Query>\n</explain>".to_owned()]),
    };
    let value = |s: String| if ty == Type::Json { Value::Json(s) } else { Value::Text(s) };
    QueryResult {
        columns: vec![OutCol::new("QUERY PLAN", ty.clone())],
        returns_rows: true,
        tag: "EXPLAIN".into(),
        rows: lines.into_iter().map(|l| vec![value(l)]).collect(),
        ..Default::default()
    }
}

/// The channel set type the session keeps.
pub(super) type Channels = BTreeSet<String>;
