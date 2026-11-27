//! What a constraint violation's DETAIL may show the current role (Postgres'
//! `ExecBuildSlotValueDescription`, `BuildIndexValueDescription`, `ri_ReportViolation`): a
//! role without SELECT on the table sees the failing row's columns it may select or that the
//! statement writes, by name, and key values only when it may select every key column.
//! Superusers and roles with SELECT on the table see everything.

use crate::roles::{current_role, has_privilege, is_superuser, relation_columns};
use crate::session::SessionEnv;
use pgmem_catalog::{Catalog, GrantObject, Oid, Relation, TableDef};
use pgmem_ir as ir;
use std::cell::RefCell;
use std::sync::Arc;

struct Viewer {
    catalog: Arc<Catalog>,
    env: SessionEnv,
    /// None: a superuser, who sees everything.
    role: Option<String>,
    /// The columns each DML target of the statement inserts or updates, by name.
    written: Vec<(Oid, Vec<String>)>,
}

thread_local! {
    /// The running statements' viewers, innermost last.
    static VIEWERS: RefCell<Vec<Viewer>> = const { RefCell::new(Vec::new()) };
}

struct Pop;

impl Drop for Pop {
    fn drop(&mut self) {
        VIEWERS.with(|v| v.borrow_mut().pop());
    }
}

/// Runs `f` (which executes `stmt`) with error details limited to what the current role
/// may read.
pub(crate) fn as_viewer<T>(catalog: &Arc<Catalog>, env: &SessionEnv, stmt: &ir::Statement, f: impl FnOnce() -> T) -> T {
    with_written(catalog, env, |c, names| written_columns(stmt, c, names), f)
}

/// Runs `f` (deferred constraint checks) with error details limited to what the current role
/// may read, counting no column as written.
pub(crate) fn as_reader<T>(catalog: &Arc<Catalog>, env: &SessionEnv, f: impl FnOnce() -> T) -> T {
    with_written(catalog, env, |_, _| Vec::new(), f)
}

fn with_written<T>(
    catalog: &Arc<Catalog>,
    env: &SessionEnv,
    written: impl FnOnce(&Catalog, bool) -> Vec<(Oid, Vec<String>)>,
    f: impl FnOnce() -> T,
) -> T {
    let role = Some(current_role(env)).filter(|r| !is_superuser(catalog, env, r));
    // A superuser's viewer only places routed rows in the target's layout: no column names.
    let written = written(catalog, role.is_some());
    let viewer = Viewer { catalog: Arc::clone(catalog), env: env.clone(), role, written };
    VIEWERS.with(|v| v.borrow_mut().push(viewer));
    let _pop = Pop;
    f()
}

fn with_viewer<T>(f: impl FnOnce(&Viewer) -> T) -> Option<T> {
    VIEWERS.with(|v| v.borrow().last().map(f))
}

impl Viewer {
    fn selects_table(&self, rel: Oid) -> bool {
        self.role.as_ref().is_none_or(|r| has_privilege(&self.catalog, &self.env, r, &GrantObject::Relation(rel), 'r'))
    }

    fn selects_column(&self, rel: Oid, attnum: i16) -> bool {
        attnum > 0 && self.role.as_ref().is_none_or(|r| has_privilege(&self.catalog, &self.env, r, &GrantObject::Column(rel, attnum), 'r'))
    }

    /// The statement's target `table` is, or a partition of (rows routed into a partition
    /// are described as the target's).
    fn root(&self, table: Oid) -> Option<(Oid, &[String])> {
        let mut cur = table;
        loop {
            if let Some((oid, names)) = self.written.iter().find(|(o, _)| *o == cur) {
                return Some((*oid, names));
            }
            cur = self.catalog.partition_of(cur)?.parent;
        }
    }
}

/// The statement target a row of partition `table` was routed through, when it is not
/// `table` itself: Postgres describes such a row in the target's column layout.
pub(crate) fn routed_layout(table: &TableDef) -> Option<Arc<TableDef>> {
    with_viewer(|v| {
        let (rel, _) = v.root(table.oid)?;
        if rel == table.oid { None } else { v.catalog.table_by_oid(rel).cloned() }
    })
    .flatten()
}

/// The columns of `table` (slots) a failing-row DETAIL shows: None for every column,
/// unnamed; otherwise the visible ones, named (none: no DETAIL).
pub(crate) fn row_columns(table: &TableDef) -> Option<Vec<usize>> {
    with_viewer(|v| {
        let (rel, written) = v.root(table.oid).unwrap_or((table.oid, &[]));
        if v.selects_table(rel) {
            return None;
        }
        let columns = relation_columns(&v.catalog, rel);
        let visible = table
            .live_columns()
            .filter(|(_, c)| written.contains(&c.name) || columns.iter().any(|(a, n)| *n == c.name && v.selects_column(rel, *a)))
            .map(|(i, _)| i)
            .collect();
        Some(visible)
    })
    .flatten()
}

/// Whether a key DETAIL over `keys` of relation `rel` may show values: SELECT on the table,
/// or on every key column (`None`: an expression, never shown without the table privilege).
pub(crate) fn key_visible(rel: Oid, keys: impl IntoIterator<Item = Option<usize>>) -> bool {
    with_viewer(|v| {
        v.selects_table(rel)
            || keys.into_iter().all(|k| k.and_then(|i| i16::try_from(i + 1).ok()).is_some_and(|a| v.selects_column(rel, a)))
    })
    .unwrap_or(true)
}

/// The leading `n` live column names of `rel`.
fn leading(catalog: &Catalog, rel: Oid, n: usize) -> Vec<String> {
    relation_columns(catalog, rel).into_iter().take(n).map(|(_, c)| c).collect()
}

fn assigned(assignments: &[ir::Assignment]) -> impl Iterator<Item = String> + '_ {
    assignments.iter().flat_map(|a| a.columns.iter()).filter_map(|c| c.first().cloned())
}

/// The columns each DML target of `stmt` (and of its data-modifying WITH queries) inserts
/// or updates (Postgres' insertedCols / updatedCols).
/// With `names` false only the targets are listed.
fn written_columns(stmt: &ir::Statement, catalog: &Catalog, names: bool) -> Vec<(Oid, Vec<String>)> {
    let mut out = Vec::new();
    collect_written(stmt, catalog, names, &mut out);
    out
}

/// The number of columns an INSERT's source query returns (every column when unknown).
fn query_width(q: &ir::Query, catalog: &Catalog) -> usize {
    if let ir::SetExpr::Select(s) = &q.body
        && s.projection.iter().all(|p| matches!(p, ir::SelectItem::Expr { .. }))
    {
        return s.projection.len();
    }
    let binder = crate::bind::Binder::new(catalog, &[]);
    let top = crate::bind::Level::new(crate::bind::Scope::default());
    binder.bind_query(q, &crate::bind::Ctx::new(&top)).map_or(usize::MAX, |b| b.columns.len())
}

/// For a write through an automatically updatable view: the base table and the base columns
/// the view columns `names` are (Postgres checks and describes the rewritten base relation).
fn view_base(catalog: &Catalog, v: &pgmem_catalog::ViewDef, names: &[String]) -> Option<(Oid, Vec<String>)> {
    if catalog.table_triggers(v.oid).iter().any(|t| t.enabled != 'D' && t.timing == pgmem_catalog::TriggerTiming::InsteadOf) {
        return None;
    }
    let Ok(Ok(u)) = crate::views::resolve(catalog, v) else { return None };
    let Ok(Some(Relation::Table(base))) = catalog.find_relation(&u.base) else { return None };
    let mapped = names.iter().filter_map(|n| u.columns.iter().find(|(c, _)| c == n).and_then(|(_, b)| b.clone())).collect();
    Some((base.oid, mapped))
}

fn collect_written(stmt: &ir::Statement, catalog: &Catalog, with_names: bool, out: &mut Vec<(Oid, Vec<String>)>) {
    let (with, table) = match stmt {
        ir::Statement::Query(q) => (q.with.as_ref(), None),
        ir::Statement::Insert(i) => (i.with.as_ref(), Some(&i.table)),
        ir::Statement::Update(u) => (u.with.as_ref(), Some(&u.table)),
        ir::Statement::Delete(d) => (d.with.as_ref(), Some(&d.table)),
        ir::Statement::Merge(m) => (m.with.as_ref(), Some(&m.table)),
        _ => return,
    };
    for cte in with.iter().flat_map(|w| &w.ctes) {
        if let ir::SetExpr::Dml(inner) = &cte.query.body {
            collect_written(inner, catalog, with_names, out);
        }
    }
    let Some(Ok(Some(rel @ (Relation::Table(_) | Relation::View(_))))) = table.map(|t| catalog.find_relation(t)) else { return };
    let oid = rel.oid();
    let insert_columns =
        |columns: &[String], width: usize| if columns.is_empty() { leading(catalog, oid, width) } else { columns.to_vec() };
    let names: Vec<String> = match stmt {
        _ if !with_names => Vec::new(),
        ir::Statement::Insert(i) => {
            let width = match &i.source {
                ir::InsertSource::Values(rows) => rows.first().map_or(0, Vec::len),
                ir::InsertSource::Query(q) => query_width(q, catalog),
                ir::InsertSource::DefaultValues => 0,
            };
            let mut names = insert_columns(&i.columns, width);
            if let Some(ir::OnConflict { action: ir::ConflictAction::Update { assignments, .. }, .. }) = &i.on_conflict {
                names.extend(assigned(assignments));
            }
            names
        }
        ir::Statement::Update(u) => assigned(&u.assignments).collect(),
        ir::Statement::Merge(m) => {
            let mut names = Vec::new();
            for c in &m.clauses {
                match &c.action {
                    ir::MergeAction::Update(a) => names.extend(assigned(a)),
                    ir::MergeAction::Insert { columns, values: Some(v), .. } => names.extend(insert_columns(columns, v.len())),
                    _ => {}
                }
            }
            names
        }
        _ => Vec::new(),
    };
    if let Relation::View(v) = &rel
        && let Some(base) = view_base(catalog, v, &names)
    {
        out.push(base);
    }
    out.push((oid, names));
}
