//! INSERT / UPDATE / DELETE execution with constraint enforcement.
//!
//! Each statement first computes all of its effects against the snapshot it reads (so it
//! never sees its own changes, as in Postgres), then validates them against the table's
//! final state and applies them to the transaction.

use crate::bind::dml::{RowRules, UniqueCheck};
use crate::bind::{Binder, DeletePlan, InsertPlan, InsertRows, UpdatePlan};
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

mod merge;

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
fn failing_row(table: &TableDef, row: &Row) -> String {
    let vals: Vec<String> = table.live_columns().map(|(i, _)| display(row.get(i).unwrap_or(&Value::Null))).collect();
    format!("Failing row contains ({}).", vals.join(", "))
}

fn pad(row: &mut Row, table: &TableDef) {
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
    for u in &rules.uniques {
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
    Ok(())
}

fn key_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| !x.is_null() && !y.is_null() && pgmem_types::compare_values(x, y).is_ok_and(|o| o.is_eq()))
}

/// Child-side foreign key check: every non-NULL key in `rows` must exist in the parent.
fn check_foreign_keys(table: &TableDef, rows: &[&Row], txn: &Txn, own_final: Option<&[(RowId, Row)]>) -> SqlResult<()> {
    for con in &table.constraints {
        let ConstraintKind::ForeignKey { columns, ref_table, ref_columns, .. } = &con.kind else { continue };
        let parent = txn.catalog().table_q(ref_table).cloned().ok_or_else(|| SqlError::undefined_table(&ref_table.name))?;
        let parent_rows: Vec<Row> = if parent.oid == table.oid {
            own_final.map_or_else(
                || txn.scan_latest(parent.oid).into_iter().map(|(_, r)| r).collect(),
                |rows| rows.iter().map(|(_, r)| r.clone()).collect(),
            )
        } else {
            txn.scan_latest(parent.oid)
                .into_iter()
                .map(|(_, mut r)| {
                    pad(&mut r, &parent);
                    r
                })
                .collect()
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
                .with_detail(format!("Key ({})=({}) is not present in table \"{ref_table}\".", names.join(", "), vals.join(", ")))
                .with_table(&table.name)
                .with_constraint(&con.name));
            }
        }
    }
    Ok(())
}

/// Parent-side referential actions after rows of `table` were deleted or had referenced
/// keys changed. `removed` holds the old versions of those rows.
fn referential_actions(txn: &mut Txn, table: &TableDef, removed: &[Row], is_delete: bool, depth: usize) -> SqlResult<()> {
    if removed.is_empty() {
        return Ok(());
    }
    if depth > 64 {
        return Err(SqlError::new("54001", "stack depth limit exceeded in cascading foreign key actions"));
    }
    let children: Vec<Arc<TableDef>> = txn.catalog().tables().cloned().collect();
    let parent_now: Vec<Row> = txn.scan_latest(table.oid).into_iter().map(|(_, r)| r).collect();
    for child in children {
        for con in &child.constraints {
            let ConstraintKind::ForeignKey { columns, ref_table, ref_columns, on_delete, on_update } = &con.kind else { continue };
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
                FkAction::NoAction | FkAction::Restrict => {
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
                    referential_actions(txn, &child, &removed_children, true, depth + 1)?;
                }
                FkAction::Cascade => {
                    // ON UPDATE CASCADE: copy the parent's new key into the children.
                    let mut old_versions = Vec::new();
                    for (id, row) in referencing {
                        let old_key: Vec<Value> = columns.iter().map(|c| row.get(*c).cloned().unwrap_or(Value::Null)).collect();
                        let Some(pos) = removed.iter().position(|r| {
                            key_equal(&old_key, &ref_columns.iter().map(|c| r.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>())
                        }) else {
                            continue;
                        };
                        let _ = pos;
                        // The new parent version is the one whose other columns match; use the
                        // most recent parent rows to find a key not present before.
                        let new_parent = parent_now.iter().find(|p| {
                            !removed.iter().any(|r| {
                                key_equal(
                                    &ref_columns.iter().map(|c| r.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
                                    &ref_columns.iter().map(|c| p.get(*c).cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
                                )
                            })
                        });
                        let Some(np) = new_parent else { continue };
                        let mut new_row = row.clone();
                        for (c, rc) in columns.iter().zip(ref_columns) {
                            new_row[*c] = np.get(*rc).cloned().unwrap_or(Value::Null);
                        }
                        txn.update(child.oid, id, new_row);
                        old_versions.push(row);
                    }
                    referential_actions(txn, &child, &old_versions, false, depth + 1)?;
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
                    for (id, row) in referencing {
                        let mut new_row = row.clone();
                        for (i, c) in columns.iter().enumerate() {
                            new_row[*c] = defaults[i].clone().unwrap_or(Value::Null);
                        }
                        check_not_null(&child, &new_row)?;
                        txn.update(child.oid, id, new_row);
                        old_versions.push(row);
                    }
                    if action == FkAction::SetDefault {
                        let rows: Vec<Row> = txn.scan_latest(child.oid).into_iter().map(|(_, r)| r).collect();
                        check_foreign_keys(&child, &rows.iter().collect::<Vec<_>>(), txn, None)?;
                    }
                    referential_actions(txn, &child, &old_versions, false, depth + 1)?;
                }
            }
        }
    }
    Ok(())
}

/// The row RETURNING is evaluated over: the statement's row, then the old and the new
/// version of the target row (all NULL when there is none).
fn returning_row(current: &[Value], old: Option<&Row>, new: Option<&Row>, width: usize) -> Row {
    let mut out = current.to_vec();
    for version in [old, new] {
        match version {
            Some(r) => {
                out.extend(r.iter().take(width).cloned());
                out.extend(std::iter::repeat_n(Value::Null, width.saturating_sub(r.len())));
            }
            None => out.extend(std::iter::repeat_n(Value::Null, width)),
        }
    }
    out
}

fn identity_always_error(col: &str) -> SqlError {
    SqlError::new(code::GENERATED_ALWAYS, format!("cannot insert a non-DEFAULT value into column \"{col}\""))
        .with_detail(format!("Column \"{col}\" is an identity column defined as GENERATED ALWAYS."))
        .with_hint("Use OVERRIDING SYSTEM VALUE to override.")
}

pub fn exec_insert(plan: &InsertPlan, txn: &mut Txn, params: &[Value], env: &SessionEnv, state: &StatementState) -> SqlResult<DmlResult> {
    let table = Arc::clone(&plan.rules.table);
    let width = table.columns.len();
    // Phase 1: compute rows (reads only).
    enum Effect {
        Insert(Row),
        Update(RowId, Row, Row),
    }
    let mut effects: Vec<Effect> = Vec::new();
    let mut returning_rows = Vec::new();
    {
        let ctx = EvalCtx::new(txn, params, env, state);
        let sources: Vec<Vec<Option<Value>>> = match &plan.rows {
            InsertRows::DefaultValues => vec![vec![]],
            InsertRows::Values(rows) => rows
                .iter()
                .map(|r| r.iter().map(|e| e.as_ref().map(|e| eval(e, &[], &ctx)).transpose()).collect::<SqlResult<Vec<_>>>())
                .collect::<SqlResult<Vec<_>>>()?,
            InsertRows::Query(rel) => run(rel, &ctx)?.into_iter().map(|r| r.into_iter().map(Some).collect()).collect(),
        };
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
                    if col.identity == Some(Identity::Always) && !plan.overriding_system_value {
                        return Err(identity_always_error(&col.name));
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
                    row[i] = eval(d, &[], &ctx)?;
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
                                let r = returning_row(&current, Some(&existing_row), Some(&new_row), width);
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
            if !plan.returning.is_empty() {
                let mut current = row.clone();
                current.extend(system_values(table.oid, None, 0));
                let r = returning_row(&current, None, Some(&row), width);
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
    for e in effects {
        match e {
            Effect::Insert(row) => {
                changed_rows.push(row.clone());
                txn.insert(table.oid, row);
            }
            Effect::Update(id, old, new) => {
                changed_rows.push(new.clone());
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
    check_foreign_keys(&table, &changed_rows.iter().collect::<Vec<_>>(), txn, Some(&final_rows))?;
    referential_actions(txn, &table, &old_versions, false, 0)?;
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
            apply_generated(&plan.rules, &mut new_row, &ctx)?;
            check_not_null(&table, &new_row)?;
            check_checks(&plan.rules, &new_row, &ctx)?;
            if !plan.returning.is_empty() {
                let mut out_row = new_row.clone();
                out_row.extend(combined.iter().skip(width).cloned());
                let out_row = returning_row(&out_row, Some(&old), Some(&new_row), width);
                returning_rows.push(plan.returning.iter().map(|e| eval(e, &out_row, &ctx)).collect::<SqlResult<Vec<_>>>()?);
            }
            changes.push((id, old, new_row));
        }
        // Unique validation over the post-update table.
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
        final_rows.extend(changes.iter().map(|(id, _, n)| (*id, n.clone())));
        check_uniques(&plan.rules, &final_rows, &changed_ids, &ctx)?;
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
    check_foreign_keys(&table, &new_versions.iter().collect::<Vec<_>>(), txn, Some(&final_rows))?;
    referential_actions(txn, &table, &old_versions, false, 0)?;
    Ok(DmlResult { count, rows: returning_rows })
}

pub fn exec_delete(plan: &DeletePlan, txn: &mut Txn, params: &[Value], env: &SessionEnv, state: &StatementState) -> SqlResult<DmlResult> {
    let table = Arc::clone(&plan.table);
    let mut targets = Vec::new();
    let mut returning_rows = Vec::new();
    {
        let ctx = EvalCtx::new(txn, params, env, state);
        for (id, row, combined) in matching_rows(&table, plan.using.as_ref(), plan.filter.as_ref(), &ctx)? {
            if !plan.returning.is_empty() {
                let r = returning_row(&combined, Some(&row), None, table.columns.len());
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
    referential_actions(txn, &table, &removed, true, 0)?;
    Ok(DmlResult { count, rows: returning_rows })
}
