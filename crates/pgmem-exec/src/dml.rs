//! INSERT / UPDATE / DELETE execution with constraint enforcement.
//!
//! Each statement first computes all of its effects against the snapshot it reads (so it
//! never sees its own changes, as in Postgres), then validates them against the table's
//! final state and applies them to the transaction.

use crate::bind::dml::{RowRules, UniqueCheck};
use crate::bind::{Binder, DeletePlan, InsertPlan, InsertRows, SYSTEM_COLUMNS, UpdatePlan};
use crate::eval::{EvalCtx, StatementState, eval, eval_pred};
use crate::plan::Row;
use crate::run::run;
use crate::session::SessionEnv;
use pgmem_catalog::{ConstraintKind, FkAction, Identity, TableDef};
use pgmem_storage::locks::RowLockMode;
use pgmem_storage::{LockWait, RowId, Txn};
use pgmem_types::{GroupKey, SqlError, SqlResult, Value, code, text};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

mod exclusion;

use crate::functions::Exec;
use crate::triggers::{self, Event};
use pgmem_catalog::TriggerTiming;

/// A statement's triggers on its target table, loaded once.
struct Triggers {
    before_row: Vec<(Arc<pgmem_catalog::TriggerDef>, Arc<pgmem_catalog::FunctionDef>)>,
    after_row: Vec<(Arc<pgmem_catalog::TriggerDef>, Arc<pgmem_catalog::FunctionDef>)>,
    before_stmt: Vec<(Arc<pgmem_catalog::TriggerDef>, Arc<pgmem_catalog::FunctionDef>)>,
    after_stmt: Vec<(Arc<pgmem_catalog::TriggerDef>, Arc<pgmem_catalog::FunctionDef>)>,
}

impl Triggers {
    fn load(txn: &Txn, table: &TableDef, event: Event, updated: Option<&[String]>, env: &SessionEnv) -> Triggers {
        Triggers {
            before_row: triggers::load(txn, table, event, TriggerTiming::Before, true, updated, env),
            after_row: triggers::load(txn, table, event, TriggerTiming::After, true, updated, env),
            before_stmt: triggers::load(txn, table, event, TriggerTiming::Before, false, updated, env),
            after_stmt: triggers::load(txn, table, event, TriggerTiming::After, false, updated, env),
        }
    }

    fn statement(
        &self,
        when: TriggerTiming,
        table: &TableDef,
        event: Event,
        txn: &mut Txn,
        env: &SessionEnv,
        state: &StatementState,
    ) -> SqlResult<()> {
        let list = if when == TriggerTiming::Before { &self.before_stmt } else { &self.after_stmt };
        if list.is_empty() {
            return Ok(());
        }
        let mut exec = Exec::Write { txn, env, state };
        triggers::fire_statement(list, table, event, &mut exec)
    }
}
mod merge;

pub(crate) use exclusion::validate as validate_exclusion;

pub use merge::exec_merge;

/// Output of a DML statement: affected-row count and RETURNING rows.
pub struct DmlResult {
    pub count: u64,
    pub rows: Vec<Row>,
}

/// The system column values (`bind::SYSTEM_COLUMNS` order) of the row stored as `id` (None:
/// not stored yet). pgmem has no tuple headers: `ctid` and `xmin` derive from the row id,
/// which changes with every new row version, as theirs do.
pub fn system_values(table: pgmem_catalog::Oid, id: Option<RowId>, xmax: i64) -> [Value; 6] {
    let id = id.unwrap_or(0);
    #[allow(clippy::cast_possible_truncation)]
    let ctid = Value::Tid((id >> 16) as u32, (id & 0xffff) as u16);
    let xmin = Value::Int8(i64::try_from(id & 0xffff_ffff).unwrap_or(0));
    [ctid, xmin, Value::Int8(0), Value::Int8(xmax), Value::Int8(0), Value::Oid(table)]
}

fn display(v: &Value) -> String {
    if v.is_null() { "null".into() } else { text::encode(v, &v.natural_type()).unwrap_or_default() }
}

/// `Failing row contains (1, null, x).`
pub(crate) fn failing_row(table: &TableDef, row: &Row) -> String {
    let vals: Vec<String> = table.live_columns().map(|(i, _)| display(row.get(i).unwrap_or(&Value::Null))).collect();
    format!("Failing row contains ({}).", vals.join(", "))
}

pub(crate) fn pad(row: &mut Row, table: &TableDef) {
    while row.len() < table.columns.len() {
        let i = row.len();
        row.push(table.columns[i].missing.clone());
    }
}

fn check_not_null(table: &TableDef, row: &Row) -> SqlResult<()> {
    for (i, c) in table.live_columns() {
        if c.not_null && row.get(i).is_none_or(Value::is_null) {
            return Err(SqlError::new(
                code::NOT_NULL_VIOLATION,
                format!("null value in column \"{}\" of relation \"{}\" violates not-null constraint", c.name, table.name),
            )
            .with_detail(failing_row(table, row))
            .with_table(&table.name)
            .with_column(&c.name));
        }
    }
    Ok(())
}

fn check_checks(rules: &RowRules, row: &Row, ctx: &EvalCtx) -> SqlResult<()> {
    for (name, pred) in &rules.checks {
        // A CHECK passes unless it is FALSE (NULL passes).
        if matches!(eval(pred, row, ctx)?, Value::Bool(false)) {
            return Err(SqlError::new(
                code::CHECK_VIOLATION,
                format!("new row for relation \"{}\" violates check constraint \"{name}\"", rules.table.name),
            )
            .with_detail(failing_row(&rules.table, row))
            .with_table(&rules.table.name)
            .with_constraint(name));
        }
    }
    Ok(())
}

/// WITH CHECK OPTION: a row written through a view must satisfy its WHERE.
fn check_view_options(table: &TableDef, checks: &[(String, crate::plan::Expr)], row: &Row, ctx: &EvalCtx) -> SqlResult<()> {
    for (view, pred) in checks {
        if !eval_pred(pred, row, ctx)? {
            return Err(
                SqlError::new("44000", format!("new row violates check option for view \"{view}\"")).with_detail(failing_row(table, row))
            );
        }
    }
    Ok(())
}

fn apply_generated(rules: &RowRules, row: &mut Row, ctx: &EvalCtx) -> SqlResult<()> {
    for (i, g) in rules.generated.iter().enumerate() {
        if let Some(g) = g {
            let v = eval(g, row, ctx)?;
            row[i] = v;
        }
    }
    Ok(())
}

/// The key of `row` in a unique index, or None if the index doesn't apply (NULL in a key
/// column, or the partial-index predicate is false).
fn unique_key(u: &UniqueCheck, row: &Row, ctx: &EvalCtx) -> SqlResult<Option<Vec<GroupKey>>> {
    if let Some(p) = &u.predicate
        && !eval_pred(p, row, ctx)?
    {
        return Ok(None);
    }
    let vals: Vec<Value> = if u.exprs.is_empty() {
        u.columns.iter().map(|c| row.get(*c).cloned().unwrap_or(Value::Null)).collect()
    } else {
        u.exprs.iter().map(|e| eval(e, row, ctx)).collect::<SqlResult<Vec<_>>>()?
    };
    if !u.nulls_not_distinct && vals.iter().any(Value::is_null) {
        return Ok(None);
    }
    Ok(Some(vals.iter().map(Value::group_key).collect()))
}

fn unique_violation(table: &TableDef, u: &UniqueCheck, row: &Row, ctx: &EvalCtx) -> SqlError {
    let (cols, vals): (Vec<String>, Vec<String>) = if u.exprs.is_empty() {
        u.columns.iter().map(|c| (table.columns[*c].name.clone(), display(row.get(*c).unwrap_or(&Value::Null)))).unzip()
    } else {
        u.exprs.iter().map(|e| ("expr".to_owned(), eval(e, row, ctx).map(|v| display(&v)).unwrap_or_default())).unzip()
    };
    SqlError::new(code::UNIQUE_VIOLATION, format!("duplicate key value violates unique constraint \"{}\"", u.name))
        .with_detail(format!("Key ({})=({}) already exists.", cols.join(", "), vals.join(", ")))
        .with_table(&table.name)
        .with_constraint(&u.name)
}

/// Validates every unique index over a table's final row set. Only rows in `changed` can be
/// the source of a violation (existing data is already consistent).
fn check_uniques(rules: &RowRules, final_rows: &[(RowId, Row)], changed: &HashSet<RowId>, ctx: &EvalCtx) -> SqlResult<()> {
    check_uniques_where(rules, final_rows, changed, ctx, |_| true)
}

/// Whether a unique index enforces a DEFERRABLE constraint (checked at statement end or
/// commit rather than row by row).
fn deferrable(rules: &RowRules, u: &UniqueCheck) -> bool {
    rules.table.constraints.iter().any(|c| c.name == u.name && c.deferrable)
}

/// Non-deferrable unique indexes are checked as each row is updated, in scan order, as
/// Postgres' index insertions are: `SET pos = pos + 1` fails on the transient duplicate.
fn check_uniques_per_row(rules: &RowRules, untouched: &[(RowId, Row)], changes: &[(RowId, Row, Row)], ctx: &EvalCtx) -> SqlResult<()> {
    for u in rules.uniques.iter().filter(|u| !deferrable(rules, u)) {
        let mut keys: HashMap<Vec<GroupKey>, usize> = HashMap::new();
        for row in untouched.iter().map(|(_, r)| r).chain(changes.iter().map(|(_, old, _)| old)) {
            if let Some(k) = unique_key(u, row, ctx)? {
                *keys.entry(k).or_default() += 1;
            }
        }
        for (_, old, new) in changes {
            if let Some(k) = unique_key(u, old, ctx)?
                && let Some(n) = keys.get_mut(&k)
            {
                *n = n.saturating_sub(1);
            }
            if let Some(k) = unique_key(u, new, ctx)? {
                let n = keys.entry(k).or_default();
                if *n > 0 {
                    return Err(unique_violation(&rules.table, u, new, ctx));
                }
                *n += 1;
            }
        }
    }
    Ok(())
}

fn check_uniques_where(
    rules: &RowRules,
    final_rows: &[(RowId, Row)],
    changed: &HashSet<RowId>,
    ctx: &EvalCtx,
    include: impl Fn(&UniqueCheck) -> bool,
) -> SqlResult<()> {
    let ck = Checks { env: ctx.env, state: ctx.state };
    for u in rules.uniques.iter().filter(|u| include(u)) {
        if rules.table.constraints.iter().any(|c| c.name == u.name && ck.defer(&rules.table, c)) {
            continue;
        }
        let mut seen: HashMap<Vec<GroupKey>, RowId> = HashMap::new();
        // Unchanged rows first, so the error reports the new row's key.
        for (id, row) in final_rows.iter().filter(|(id, _)| !changed.contains(id)) {
            if let Some(k) = unique_key(u, row, ctx)? {
                seen.insert(k, *id);
            }
        }
        for (id, row) in final_rows.iter().filter(|(id, _)| changed.contains(id)) {
            if let Some(k) = unique_key(u, row, ctx)? {
                if seen.contains_key(&k) {
                    return Err(unique_violation(&rules.table, u, row, ctx));
                }
                seen.insert(k, *id);
            }
        }
    }
    exclusion::check_exclusions(rules, final_rows, changed, ctx)
}

fn key_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| !x.is_null() && !y.is_null() && pgmem_types::compare_values(x, y).is_ok_and(|o| o.is_eq()))
}

/// The statement's view of constraint timing: deferred checks are queued, not run.
#[derive(Clone, Copy)]
pub(crate) struct Checks<'a> {
    pub env: &'a SessionEnv,
    pub state: &'a StatementState,
}

impl Checks<'_> {
    /// True (and the check queued for commit) when `con` of `table` is deferred.
    fn defer(&self, table: &TableDef, con: &pgmem_catalog::Constraint) -> bool {
        if !self.env.constraint_modes.deferred(con) {
            return false;
        }
        let key = (table.oid, con.name.clone());
        let mut d = self.state.deferred.borrow_mut();
        if !d.contains(&key) {
            d.push(key);
        }
        true
    }
}

/// Child-side foreign key check: every non-NULL key in `rows` must exist in the parent.
fn check_foreign_keys(table: &TableDef, rows: &[&Row], txn: &Txn, own_final: Option<&[(RowId, Row)]>, ck: Checks) -> SqlResult<()> {
    for con in &table.constraints {
        let ConstraintKind::ForeignKey { columns, ref_table, ref_columns, match_full, .. } = &con.kind else { continue };
        if rows.is_empty() || ck.defer(table, con) {
            continue;
        }
        if *match_full {
            for row in rows {
                let nulls = columns.iter().filter(|c| row.get(**c).is_none_or(Value::is_null)).count();
                if nulls > 0 && nulls < columns.len() {
                    return Err(SqlError::new(
                        code::FOREIGN_KEY_VIOLATION,
                        format!("insert or update on table \"{}\" violates foreign key constraint \"{}\"", table.name, con.name),
                    )
                    .with_detail("MATCH FULL does not allow mixing of null and nonnull key values.")
                    .with_table(&table.name)
                    .with_constraint(&con.name));
                }
            }
        }
        let parent = txn.catalog().table_q(ref_table).cloned().ok_or_else(|| SqlError::undefined_table(&ref_table.name))?;
        let parent_rows: Vec<Row> = if parent.oid == table.oid {
            own_final.map_or_else(
                || txn.scan_latest(parent.oid).into_iter().map(|(_, r)| r).collect(),
                |rows| rows.iter().map(|(_, r)| r.clone()).collect(),
            )
        } else {
            crate::partition::latest_rows(txn, &parent)
        };
        for row in rows {
            let key: Vec<Value> = columns.iter().map(|c| row.get(*c).cloned().unwrap_or(Value::Null)).collect();
            if key.iter().any(Value::is_null) {
                continue;
            }
            let found = parent_rows
                .iter()
                .any(|p| key_equal(&key, &ref_columns.iter().map(|c| p.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>()));
            if !found {
                let names: Vec<&str> = columns.iter().map(|c| table.columns[*c].name.as_str()).collect();
                let vals: Vec<String> = key.iter().map(display).collect();
                return Err(SqlError::new(
                    code::FOREIGN_KEY_VIOLATION,
                    format!("insert or update on table \"{}\" violates foreign key constraint \"{}\"", table.name, con.name),
                )
                .with_detail(format!("Key ({})=({}) is not present in table \"{}\".", names.join(", "), vals.join(", "), ref_table.name))
                .with_table(&table.name)
                .with_constraint(&con.name));
            }
        }
    }
    Ok(())
}

/// Parent-side referential actions after rows of `table` were deleted or had referenced
/// keys changed. `removed` holds the old versions of those rows.
/// `replaced` holds the new versions of updated rows, aligned with `removed` (empty for a
/// delete).
fn referential_actions(
    txn: &mut Txn,
    table: &TableDef,
    removed: &[Row],
    replaced: &[Row],
    is_delete: bool,
    depth: usize,
    ck: Checks,
) -> SqlResult<()> {
    if removed.is_empty() {
        return Ok(());
    }
    if depth > 64 {
        return Err(SqlError::new("54001", "stack depth limit exceeded in cascading foreign key actions"));
    }
    // Rows of a partition are rows of its ancestors, which foreign keys may reference.
    if let Some(of) = txn.catalog().partition_of(table.oid).cloned()
        && let Some(parent) = txn.catalog().table_by_oid(of.parent).cloned()
    {
        let up = |rows: &[Row]| rows.iter().map(|r| crate::partition::remap(r, table, &parent)).collect::<Vec<_>>();
        let (removed_up, replaced_up) = (up(removed), up(replaced));
        referential_actions(txn, &parent, &removed_up, &replaced_up, is_delete, depth + 1, ck)?;
    }
    let children: Vec<Arc<TableDef>> = txn.catalog().tables().cloned().collect();
    let parent_now: Vec<Row> = crate::partition::latest_rows(txn, table);
    for child in children {
        for con in &child.constraints {
            let ConstraintKind::ForeignKey { columns, ref_table, ref_columns, on_delete, on_update, delete_set_columns, .. } = &con.kind
            else {
                continue;
            };
            if *ref_table != table.qualified() {
                continue;
            }
            let action = if is_delete { *on_delete } else { *on_update };
            // Keys that disappeared from the parent.
            let gone: Vec<Vec<Value>> = removed
                .iter()
                .map(|r| ref_columns.iter().map(|c| r.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>())
                .filter(|k: &Vec<Value>| !k.iter().any(Value::is_null))
                .filter(|k| {
                    !parent_now
                        .iter()
                        .any(|p| key_equal(k, &ref_columns.iter().map(|c| p.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>()))
                })
                .collect();
            if gone.is_empty() {
                continue;
            }
            let referencing: Vec<(RowId, Row)> = txn
                .scan_latest(child.oid)
                .into_iter()
                .filter(|(_, r)| {
                    let k: Vec<Value> = columns.iter().map(|c| r.get(*c).cloned().unwrap_or(Value::Null)).collect();
                    gone.iter().any(|g| key_equal(g, &k))
                })
                .collect();
            if referencing.is_empty() {
                continue;
            }
            match action {
                // NO ACTION of a deferred constraint is checked at commit (RESTRICT never is).
                FkAction::NoAction if ck.defer(&child, con) => {}
                FkAction::Restrict => {
                    let names: Vec<&str> = ref_columns.iter().map(|c| table.columns[*c].name.as_str()).collect();
                    let vals: Vec<String> = gone[0].iter().map(display).collect();
                    return Err(SqlError::new(
                        "23001",
                        format!(
                            "update or delete on table \"{}\" violates RESTRICT setting of foreign key constraint \"{}\" on table \"{}\"",
                            table.name, con.name, child.name
                        ),
                    )
                    .with_detail(format!("Key ({})=({}) is referenced from table \"{}\".", names.join(", "), vals.join(", "), child.name))
                    .with_table(&child.name)
                    .with_constraint(&con.name));
                }
                FkAction::NoAction => {
                    let key = &gone[0];
                    let names: Vec<&str> = ref_columns.iter().map(|c| table.columns[*c].name.as_str()).collect();
                    let vals: Vec<String> = key.iter().map(display).collect();
                    return Err(SqlError::new(
                        code::FOREIGN_KEY_VIOLATION,
                        format!(
                            "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                            table.name, con.name, child.name
                        ),
                    )
                    .with_detail(format!(
                        "Key ({})=({}) is still referenced from table \"{}\".",
                        names.join(", "),
                        vals.join(", "),
                        child.name
                    ))
                    .with_table(&child.name)
                    .with_constraint(&con.name));
                }
                FkAction::Cascade if is_delete => {
                    let mut removed_children = Vec::new();
                    for (id, row) in referencing {
                        txn.delete(child.oid, id);
                        removed_children.push(row);
                    }
                    referential_actions(txn, &child, &removed_children, &[], true, depth + 1, ck)?;
                }
                FkAction::Cascade => {
                    // ON UPDATE CASCADE: copy the parent's new key into the children.
                    let mut old_versions = Vec::new();
                    let mut new_versions = Vec::new();
                    for (id, row) in referencing {
                        let old_key: Vec<Value> = columns.iter().map(|c| row.get(*c).cloned().unwrap_or(Value::Null)).collect();
                        let Some(pos) = removed.iter().position(|r| {
                            key_equal(&old_key, &ref_columns.iter().map(|c| r.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>())
                        }) else {
                            continue;
                        };
                        // The parent's own new version when known; otherwise a parent row with a
                        // key that was not there before.
                        let new_parent = replaced.get(pos).or_else(|| {
                            parent_now.iter().find(|p| {
                                !removed.iter().any(|r| {
                                    key_equal(
                                        &ref_columns.iter().map(|c| r.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
                                        &ref_columns.iter().map(|c| p.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
                                    )
                                })
                            })
                        });
                        let Some(np) = new_parent.cloned() else { continue };
                        let mut new_row = row.clone();
                        for (c, rc) in columns.iter().zip(ref_columns) {
                            new_row[*c] = np.get(*rc).cloned().unwrap_or(Value::Null);
                        }
                        txn.update(child.oid, id, new_row.clone());
                        old_versions.push(row);
                        new_versions.push(new_row);
                    }
                    referential_actions(txn, &child, &old_versions, &new_versions, false, depth + 1, ck)?;
                }
                FkAction::SetNull | FkAction::SetDefault => {
                    let defaults: Vec<Option<Value>> = if action == FkAction::SetDefault {
                        let catalog = txn.catalog().clone();
                        let binder = Binder::new(&catalog, &[]);
                        let rules = binder.bind_row_rules(&child)?;
                        let env = SessionEnv::detached();
                        let state = StatementState::default();
                        let ctx = EvalCtx::new(txn, &[], &env, &state);
                        columns
                            .iter()
                            .map(|c| rules.defaults[*c].as_ref().map(|d| eval(d, &[], &ctx)).transpose())
                            .collect::<SqlResult<Vec<_>>>()?
                    } else {
                        vec![None; columns.len()]
                    };
                    let mut old_versions = Vec::new();
                    let mut new_versions = Vec::new();
                    for (id, row) in referencing {
                        let mut new_row = row.clone();
                        for (i, c) in columns.iter().enumerate() {
                            // `ON DELETE SET NULL (cols)` sets only those columns.
                            if is_delete && !delete_set_columns.is_empty() && !delete_set_columns.contains(c) {
                                continue;
                            }
                            new_row[*c] = defaults[i].clone().unwrap_or(Value::Null);
                        }
                        check_not_null(&child, &new_row)?;
                        txn.update(child.oid, id, new_row.clone());
                        old_versions.push(row);
                        new_versions.push(new_row);
                    }
                    if action == FkAction::SetDefault {
                        // A default with no parent row fails as the parent's update / delete.
                        let rows: Vec<Row> = txn.scan_latest(child.oid).into_iter().map(|(_, r)| r).collect();
                        if check_foreign_keys(&child, &rows.iter().collect::<Vec<_>>(), txn, None, ck).is_err() {
                            let names: Vec<&str> = ref_columns.iter().map(|c| table.columns[*c].name.as_str()).collect();
                            let vals: Vec<String> = gone[0].iter().map(display).collect();
                            return Err(SqlError::new(
                                code::FOREIGN_KEY_VIOLATION,
                                format!(
                                    "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                                    table.name, con.name, child.name
                                ),
                            )
                            .with_detail(format!(
                                "Key ({})=({}) is still referenced from table \"{}\".",
                                names.join(", "),
                                vals.join(", "),
                                child.name
                            )));
                        }
                    }
                    referential_actions(txn, &child, &old_versions, &new_versions, false, depth + 1, ck)?;
                }
            }
        }
    }
    Ok(())
}

/// The row RETURNING is evaluated over: the statement's row, then the old and the new
/// version of the target row with its system columns (all NULL when there is none).
fn returning_row(current: &[Value], old: Option<(&Row, &[Value])>, new: Option<(&Row, &[Value])>, width: usize) -> Row {
    let mut out = current.to_vec();
    for version in [old, new] {
        match version {
            Some((r, system)) => {
                out.extend(r.iter().take(width).cloned());
                out.extend(std::iter::repeat_n(Value::Null, width.saturating_sub(r.len())));
                out.extend(system.iter().cloned());
            }
            None => out.extend(std::iter::repeat_n(Value::Null, width + SYSTEM_COLUMNS.len())),
        }
    }
    out
}

/// The system columns of the statement's target row in `combined` (target ++ system ++ ...).
fn system_of(combined: &[Value], width: usize) -> &[Value] {
    &combined[width..width + SYSTEM_COLUMNS.len()]
}

fn identity_always_error(col: &str) -> SqlError {
    SqlError::new(code::GENERATED_ALWAYS, format!("cannot insert a non-DEFAULT value into column \"{col}\""))
        .with_detail(format!("Column \"{col}\" is an identity column defined as GENERATED ALWAYS."))
        .with_hint("Use OVERRIDING SYSTEM VALUE to override.")
}

/// One INSERT row from its source values: explicit values in their slots (with the generated
/// and identity column rules), defaults elsewhere.
fn source_row(plan: &InsertPlan, table: &TableDef, src: Vec<Option<Value>>, ctx: &EvalCtx) -> SqlResult<Row> {
    let width = table.columns.len();
    let mut row: Row = vec![Value::Null; width];
    let mut explicit = vec![false; width];
    for (pos, v) in src.into_iter().enumerate() {
        let slot = plan.columns[pos];
        if let Some(v) = v {
            let col = &table.columns[slot];
            if col.generated.is_some() {
                return Err(SqlError::new(
                    code::GENERATED_ALWAYS,
                    format!("cannot insert a non-DEFAULT value into column \"{}\"", col.name),
                )
                .with_detail(format!("Column \"{}\" is a generated column.", col.name)));
            }
            if col.identity == Some(Identity::Always) && !plan.overriding_system_value && !plan.overriding_user_value {
                return Err(identity_always_error(&col.name));
            }
            if col.identity.is_some() && plan.overriding_user_value {
                continue;
            }
            row[slot] = v;
            explicit[slot] = true;
        }
    }
    for (i, c) in table.columns.iter().enumerate() {
        if !explicit[i]
            && !c.dropped
            && c.generated.is_none()
            && let Some(d) = &plan.rules.defaults[i]
        {
            row[i] = eval(d, &[], ctx)?;
        }
    }
    Ok(row)
}

fn insert_sources(plan: &InsertPlan, ctx: &EvalCtx) -> SqlResult<Vec<Vec<Option<Value>>>> {
    Ok(match &plan.rows {
        InsertRows::DefaultValues => vec![vec![]],
        InsertRows::Values(rows) => rows
            .iter()
            .map(|r| r.iter().map(|e| e.as_ref().map(|e| eval(e, &[], ctx)).transpose()).collect::<SqlResult<Vec<_>>>())
            .collect::<SqlResult<Vec<_>>>()?,
        InsertRows::Query(rel) => run(rel, ctx)?.into_iter().map(|r| r.into_iter().map(Some).collect()).collect(),
    })
}

/// The rows an INSERT would add (explicit values and defaults; generated columns left NULL),
/// without adding them: INSERT into a partitioned table routes them to its partitions.
pub(crate) fn insert_preview(
    plan: &InsertPlan,
    txn: &Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<Vec<Row>> {
    let ctx = EvalCtx::new(txn, params, env, state);
    let table = Arc::clone(&plan.rules.table);
    let mut rows = Vec::new();
    for src in insert_sources(plan, &ctx)? {
        let row = source_row(plan, &table, src, &ctx)?;
        check_checks(&plan.rules, &row, &ctx)?;
        rows.push(row);
    }
    Ok(rows)
}

pub fn exec_insert(plan: &InsertPlan, txn: &mut Txn, params: &[Value], env: &SessionEnv, state: &StatementState) -> SqlResult<DmlResult> {
    let table = Arc::clone(&plan.rules.table);
    let width = table.columns.len();
    let trig = Triggers::load(txn, &table, Event::Insert, None, env);
    let is_partition = txn.catalog().partition_of(table.oid).is_some();
    trig.statement(TriggerTiming::Before, &table, Event::Insert, txn, env, state)?;
    // Phase 1: compute rows (reads only).
    enum Effect {
        Insert(Row),
        Update(RowId, Row, Row),
    }
    let mut effects: Vec<Effect> = Vec::new();
    let mut returning_rows = Vec::new();
    {
        let ctx = EvalCtx::new(txn, params, env, state);
        let sources = insert_sources(plan, &ctx)?;
        let existing: Vec<(RowId, Row)> = txn
            .scan_latest(table.oid)
            .into_iter()
            .map(|(id, mut r)| {
                pad(&mut r, &table);
                (id, r)
            })
            .collect();
        let mut pending: Vec<Row> = Vec::new();
        let mut updated_ids: HashSet<RowId> = HashSet::new();
        let mut current: Vec<(RowId, Row)> = existing.clone();
        for src in sources {
            let mut row = source_row(plan, &table, src, &ctx)?;
            if !trig.before_row.is_empty() {
                let mut exec = Exec::Read(&ctx);
                match triggers::fire_row(&trig.before_row, &table, Event::Insert, Some(row), None, &mut exec)? {
                    Some(r) => row = r,
                    None => continue,
                }
            }
            apply_generated(&plan.rules, &mut row, &ctx)?;
            // ON CONFLICT: look for a row (existing or inserted by this statement) clashing on
            // the arbiter index (or, for bare DO NOTHING, on any unique index).
            if let Some(oc) = &plan.on_conflict {
                let arbiters: Vec<&UniqueCheck> = match &oc.arbiter {
                    Some(a) => vec![a],
                    None => plan.rules.uniques.iter().collect(),
                };
                let mut conflict: Option<(Option<RowId>, Row)> = None;
                'find: for a in arbiters {
                    let Some(k) = unique_key(a, &row, &ctx)? else { continue };
                    for (id, r) in &current {
                        if unique_key(a, r, &ctx)?.as_ref() == Some(&k) {
                            conflict = Some((Some(*id), r.clone()));
                            break 'find;
                        }
                    }
                    for r in &pending {
                        if unique_key(a, r, &ctx)?.as_ref() == Some(&k) {
                            conflict = Some((None, r.clone()));
                            break 'find;
                        }
                    }
                }
                if let Some((id, existing_row)) = conflict {
                    match &oc.update {
                        None => continue,
                        Some(up) => {
                            let (assigns, filter) = (&up.assignments, &up.filter);
                            let Some(id) = id else {
                                return Err(SqlError::new(
                                    code::CARDINALITY_VIOLATION,
                                    "ON CONFLICT DO UPDATE command cannot affect row a second time",
                                )
                                .with_hint(
                                    "Ensure that no rows proposed for insertion within the same command have duplicate constrained values.",
                                ));
                            };
                            if updated_ids.contains(&id) {
                                return Err(SqlError::new(
                                    code::CARDINALITY_VIOLATION,
                                    "ON CONFLICT DO UPDATE command cannot affect row a second time",
                                )
                                .with_hint(
                                    "Ensure that no rows proposed for insertion within the same command have duplicate constrained values.",
                                ));
                            }
                            let mut combined = existing_row.clone();
                            combined.extend(row.iter().cloned());
                            if let Some(f) = filter
                                && !eval_pred(f, &combined, &ctx)?
                            {
                                continue;
                            }
                            let mut new_row = existing_row.clone();
                            for (slot, e) in assigns {
                                new_row[*slot] = match e {
                                    Some(e) => eval(e, &combined, &ctx)?,
                                    None => match &plan.rules.defaults[*slot] {
                                        Some(d) => eval(d, &[], &ctx)?,
                                        None => Value::Null,
                                    },
                                };
                            }
                            apply_generated(&plan.rules, &mut new_row, &ctx)?;
                            check_not_null(&table, &new_row)?;
                            check_checks(&plan.rules, &new_row, &ctx)?;
                            updated_ids.insert(id);
                            for entry in current.iter_mut() {
                                if entry.0 == id {
                                    entry.1 = new_row.clone();
                                }
                            }
                            if !plan.returning.is_empty() {
                                // The locked-then-updated row version: xmax is the updating transaction.
                                let mut current = new_row.clone();
                                current.extend(system_values(table.oid, Some(id), env.txid));
                                let system = system_of(&current, width);
                                let r = returning_row(&current, Some((&existing_row, system)), Some((&new_row, system)), width);
                                returning_rows.push(plan.returning.iter().map(|e| eval(e, &r, &ctx)).collect::<SqlResult<Vec<_>>>()?);
                            }
                            effects.push(Effect::Update(id, existing_row, new_row));
                            continue;
                        }
                    }
                }
            }
            check_not_null(&table, &row)?;
            check_checks(&plan.rules, &row, &ctx)?;
            check_view_options(&table, &plan.view_checks, &row, &ctx)?;
            if is_partition && !crate::partition::satisfies_constraint(ctx.txn.catalog(), &table, &row, &ctx)? {
                return Err(crate::partition::constraint_error(&table, &row));
            }
            if !plan.returning.is_empty() {
                let mut current = row.clone();
                current.extend(system_values(table.oid, None, 0));
                let r = returning_row(&current, None, Some((&row, system_of(&current, width))), width);
                returning_rows.push(plan.returning.iter().map(|e| eval(e, &r, &ctx)).collect::<SqlResult<Vec<_>>>()?);
            }
            pending.push(row.clone());
            effects.push(Effect::Insert(row));
        }
        // Unique validation over the final state.
        let mut final_rows = current;
        let mut changed: HashSet<RowId> = updated_ids;
        for (i, r) in pending.iter().enumerate() {
            let fake = u64::MAX - u64::try_from(i).unwrap_or(0);
            changed.insert(fake);
            final_rows.push((fake, r.clone()));
        }
        check_uniques(&plan.rules, &final_rows, &changed, &ctx)?;
    }
    // Phase 2: apply.
    let mut count = 0u64;
    let mut changed_rows: Vec<Row> = Vec::new();
    let mut old_versions: Vec<Row> = Vec::new();
    let mut updated_new: Vec<Row> = Vec::new();
    for e in effects {
        match e {
            Effect::Insert(row) => {
                changed_rows.push(row.clone());
                txn.insert(table.oid, row);
            }
            Effect::Update(id, old, new) => {
                changed_rows.push(new.clone());
                updated_new.push(new.clone());
                old_versions.push(old);
                txn.update(table.oid, id, new);
            }
        }
        count += 1;
    }
    let final_rows: Vec<(RowId, Row)> = txn
        .scan_latest(table.oid)
        .into_iter()
        .map(|(id, mut r)| {
            pad(&mut r, &table);
            (id, r)
        })
        .collect();
    let ck = Checks { env, state };
    check_foreign_keys(&table, &changed_rows.iter().collect::<Vec<_>>(), txn, Some(&final_rows), ck)?;
    referential_actions(txn, &table, &old_versions, &updated_new, false, 0, ck)?;
    for row in changed_rows.iter().filter(|_| !trig.after_row.is_empty()) {
        let mut exec = Exec::Write { txn, env, state };
        triggers::fire_row(&trig.after_row, &table, Event::Insert, Some(row.clone()), None, &mut exec)?;
    }
    trig.statement(TriggerTiming::After, &table, Event::Insert, txn, env, state)?;
    Ok(DmlResult { count, rows: returning_rows })
}

/// Evaluates a WHERE over target rows, optionally joined with FROM/USING rows; returns the
/// matching target rows with the combined row used for evaluation (first FROM match wins).
fn matching_rows(
    table: &TableDef,
    extra: Option<&(crate::plan::Rel, usize)>,
    filter: Option<&crate::plan::Expr>,
    ctx: &EvalCtx,
) -> SqlResult<Vec<(RowId, Row, Row)>> {
    let extra_rows = match extra {
        Some((rel, _)) => Some(run(rel, ctx)?),
        None => None,
    };
    let mut out = Vec::new();
    for (id, mut row) in ctx.txn.scan(table.oid) {
        pad(&mut row, table);
        // The statement's row: the target row, its system columns, then any FROM / USING row.
        let mut target = row.clone();
        target.extend(system_values(table.oid, Some(id), 0));
        match &extra_rows {
            None => {
                let ok = match filter {
                    None => true,
                    Some(f) => eval_pred(f, &target, ctx)?,
                };
                if ok {
                    out.push((id, row, target));
                }
            }
            Some(extra_rows) => {
                for er in extra_rows {
                    let mut combined = target.clone();
                    combined.extend(er.iter().cloned());
                    let ok = match filter {
                        None => true,
                        Some(f) => eval_pred(f, &combined, ctx)?,
                    };
                    if ok {
                        out.push((id, row.clone(), combined));
                        break;
                    }
                }
            }
        }
    }
    Ok(out)
}

pub fn exec_update(plan: &UpdatePlan, txn: &mut Txn, params: &[Value], env: &SessionEnv, state: &StatementState) -> SqlResult<DmlResult> {
    let table = Arc::clone(&plan.rules.table);
    let set_columns: Vec<String> = plan.assignments.iter().map(|(slot, _)| table.columns[*slot].name.clone()).collect();
    let trig = Triggers::load(txn, &table, Event::Update, Some(&set_columns), env);
    let is_partition = txn.catalog().partition_of(table.oid).is_some();
    trig.statement(TriggerTiming::Before, &table, Event::Update, txn, env, state)?;
    let mut changes: Vec<(RowId, Row, Row)> = Vec::new();
    let mut returning_rows = Vec::new();
    {
        let ctx = EvalCtx::new(txn, params, env, state);
        let targets = matching_rows(&table, plan.from.as_ref(), plan.filter.as_ref(), &ctx)?;
        let width = table.columns.len();
        for (id, old, combined) in targets {
            let mut new_row = old.clone();
            for (slot, e) in &plan.assignments {
                new_row[*slot] = match e {
                    Some(e) => eval(e, &combined, &ctx)?,
                    None => match &plan.rules.defaults[*slot] {
                        Some(d) => eval(d, &[], &ctx)?,
                        None => Value::Null,
                    },
                };
            }
            if !trig.before_row.is_empty() {
                let mut exec = Exec::Read(&ctx);
                match triggers::fire_row(&trig.before_row, &table, Event::Update, Some(new_row), Some(&old), &mut exec)? {
                    Some(r) => new_row = r,
                    None => continue,
                }
            }
            apply_generated(&plan.rules, &mut new_row, &ctx)?;
            // A row whose new partition key belongs to another partition: moved there when the
            // UPDATE came through the partitioned table, an error otherwise.
            if is_partition && !crate::partition::satisfies_constraint(ctx.txn.catalog(), &table, &new_row, &ctx)? {
                match state.moved_rows.borrow_mut().as_mut() {
                    Some(moved) => {
                        moved.push((table.oid, id, new_row));
                        continue;
                    }
                    None => return Err(crate::partition::constraint_error(&table, &new_row)),
                }
            }
            check_not_null(&table, &new_row)?;
            check_checks(&plan.rules, &new_row, &ctx)?;
            check_view_options(&table, &plan.view_checks, &new_row, &ctx)?;
            if !plan.returning.is_empty() {
                let mut out_row = new_row.clone();
                out_row.extend(combined.iter().skip(width).cloned());
                let system = system_of(&combined, width);
                let out_row = returning_row(&out_row, Some((&old, system)), Some((&new_row, system)), width);
                returning_rows.push(plan.returning.iter().map(|e| eval(e, &out_row, &ctx)).collect::<SqlResult<Vec<_>>>()?);
            }
            changes.push((id, old, new_row));
        }
        // Unique validation: non-deferrable indexes row by row, deferrable ones over the
        // post-update table (or at commit).
        let changed_ids: HashSet<RowId> = changes.iter().map(|(id, _, _)| *id).collect();
        let mut final_rows: Vec<(RowId, Row)> = ctx
            .txn
            .scan_latest(table.oid)
            .into_iter()
            .filter(|(id, _)| !changed_ids.contains(id))
            .map(|(id, mut r)| {
                pad(&mut r, &table);
                (id, r)
            })
            .collect();
        check_uniques_per_row(&plan.rules, &final_rows, &changes, &ctx)?;
        final_rows.extend(changes.iter().map(|(id, _, n)| (*id, n.clone())));
        check_uniques_where(&plan.rules, &final_rows, &changed_ids, &ctx, |u| deferrable(&plan.rules, u))?;
    }
    let count = changes.len() as u64;
    let mut old_versions = Vec::new();
    let mut new_versions = Vec::new();
    for (id, old, new) in &changes {
        // FOR UPDATE when a unique key changes, else FOR NO KEY UPDATE (as Postgres' UPDATE).
        let key_changed = plan.rules.uniques.iter().any(|u| u.columns.iter().any(|c| old.get(*c) != new.get(*c)));
        let mode = if key_changed { RowLockMode::Update } else { RowLockMode::NoKeyUpdate };
        txn.lock_row(table.oid, &table.name, *id, mode, LockWait::Block)?;
    }
    for (id, old, new) in changes {
        old_versions.push(old);
        new_versions.push(new.clone());
        txn.update(table.oid, id, new);
    }
    let final_rows: Vec<(RowId, Row)> = txn
        .scan_latest(table.oid)
        .into_iter()
        .map(|(id, mut r)| {
            pad(&mut r, &table);
            (id, r)
        })
        .collect();
    let ck = Checks { env, state };
    check_foreign_keys(&table, &new_versions.iter().collect::<Vec<_>>(), txn, Some(&final_rows), ck)?;
    referential_actions(txn, &table, &old_versions, &new_versions, false, 0, ck)?;
    for (old, new) in old_versions.iter().zip(&new_versions).filter(|_| !trig.after_row.is_empty()) {
        let mut exec = Exec::Write { txn, env, state };
        triggers::fire_row(&trig.after_row, &table, Event::Update, Some(new.clone()), Some(old), &mut exec)?;
    }
    trig.statement(TriggerTiming::After, &table, Event::Update, txn, env, state)?;
    Ok(DmlResult { count, rows: returning_rows })
}

pub fn exec_delete(plan: &DeletePlan, txn: &mut Txn, params: &[Value], env: &SessionEnv, state: &StatementState) -> SqlResult<DmlResult> {
    let table = Arc::clone(&plan.table);
    let trig = Triggers::load(txn, &table, Event::Delete, None, env);
    trig.statement(TriggerTiming::Before, &table, Event::Delete, txn, env, state)?;
    let mut targets = Vec::new();
    let mut returning_rows = Vec::new();
    {
        let ctx = EvalCtx::new(txn, params, env, state);
        for (id, row, combined) in matching_rows(&table, plan.using.as_ref(), plan.filter.as_ref(), &ctx)? {
            if !trig.before_row.is_empty() {
                let mut exec = Exec::Read(&ctx);
                if triggers::fire_row(&trig.before_row, &table, Event::Delete, None, Some(&row), &mut exec)?.is_none() {
                    continue;
                }
            }
            if !plan.returning.is_empty() {
                let width = table.columns.len();
                let r = returning_row(&combined, Some((&row, system_of(&combined, width))), None, width);
                returning_rows.push(plan.returning.iter().map(|e| eval(e, &r, &ctx)).collect::<SqlResult<Vec<_>>>()?);
            }
            targets.push((id, row));
        }
    }
    let count = targets.len() as u64;
    for (id, _) in &targets {
        txn.lock_row(table.oid, &table.name, *id, RowLockMode::Update, LockWait::Block)?;
    }
    let mut removed = Vec::new();
    for (id, row) in targets {
        txn.delete(table.oid, id);
        removed.push(row);
    }
    referential_actions(txn, &table, &removed, &[], true, 0, Checks { env, state })?;
    for row in removed.iter().filter(|_| !trig.after_row.is_empty()) {
        let mut exec = Exec::Write { txn, env, state };
        triggers::fire_row(&trig.after_row, &table, Event::Delete, None, Some(row), &mut exec)?;
    }
    trig.statement(TriggerTiming::After, &table, Event::Delete, txn, env, state)?;
    Ok(DmlResult { count, rows: returning_rows })
}
