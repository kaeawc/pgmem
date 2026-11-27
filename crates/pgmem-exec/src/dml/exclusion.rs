//! Exclusion constraint enforcement (`EXCLUDE ...`, `WITHOUT OVERLAPS` keys): no two rows
//! whose keys satisfy every operator. NULL keys never conflict.

use super::{Checks, display};
use crate::bind::dml::{ExclusionCheck, RowRules};
use crate::eval::{EvalCtx, eval, eval_pred};
use crate::plan::Row;
use pgmem_storage::RowId;
use pgmem_types::{SqlError, SqlResult, Value, code};
use std::collections::HashSet;

/// The row's key values, or None when the predicate excludes it or a key is NULL.
fn keys(x: &ExclusionCheck, row: &Row, ctx: &EvalCtx) -> SqlResult<Option<Vec<Value>>> {
    if let Some(p) = &x.predicate
        && !eval_pred(p, row, ctx)?
    {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(x.keys.len());
    for k in &x.keys {
        let v = eval(k, row, ctx)?;
        if v.is_null() {
            return Ok(None);
        }
        out.push(v);
    }
    Ok(Some(out))
}

fn conflicts(x: &ExclusionCheck, a: &[Value], b: &[Value], ctx: &EvalCtx) -> SqlResult<bool> {
    for ((op, l), r) in x.ops.iter().zip(a).zip(b) {
        if !(op.f)(&[l.clone(), r.clone()], ctx)?.is_true() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn key_text(vals: &[Value]) -> String {
    vals.iter().map(display).collect::<Vec<_>>().join(", ")
}

/// The first conflicting pair among `rows`, where only `changed` rows can be new.
fn find_conflict(
    x: &ExclusionCheck,
    rows: &[(RowId, Row)],
    changed: &HashSet<RowId>,
    ctx: &EvalCtx,
) -> SqlResult<Option<(Vec<Value>, Vec<Value>)>> {
    let mut seen: Vec<Vec<Value>> = Vec::new();
    for (_, row) in rows.iter().filter(|(id, _)| !changed.contains(id)) {
        if let Some(k) = keys(x, row, ctx)? {
            seen.push(k);
        }
    }
    for (_, row) in rows.iter().filter(|(id, _)| changed.contains(id)) {
        let Some(k) = keys(x, row, ctx)? else { continue };
        for other in &seen {
            if conflicts(x, &k, other, ctx)? {
                return Ok(Some((k, other.clone())));
            }
        }
        seen.push(k);
    }
    Ok(None)
}

/// Checks the exclusion constraints over a table's final rows (deferred ones are queued).
pub(crate) fn check_exclusions(rules: &RowRules, rows: &[(RowId, Row)], changed: &HashSet<RowId>, ctx: &EvalCtx) -> SqlResult<()> {
    let ck = Checks { env: ctx.env, state: ctx.state };
    for x in &rules.exclusions {
        if rules.table.constraints.iter().any(|c| c.name == x.name && ck.defer(&rules.table, c)) {
            continue;
        }
        if let Some((new, old)) = find_conflict(x, rows, changed, ctx)? {
            let keys = x.columns.iter().map(|c| rules.table.column_index(c));
            let detail = if crate::detail_privileges::key_visible(rules.table.oid, keys) {
                format!(
                    "Key ({cols})=({}) conflicts with existing key ({cols})=({}).",
                    key_text(&new),
                    key_text(&old),
                    cols = x.columns.join(", ")
                )
            } else {
                "Key conflicts with existing key.".to_owned()
            };
            return Err(SqlError::new(
                code::EXCLUSION_VIOLATION,
                format!("conflicting key value violates exclusion constraint \"{}\"", x.name),
            )
            .with_detail(detail)
            .with_table(&rules.table.name)
            .with_constraint(&x.name));
        }
    }
    Ok(())
}

/// Validates exclusion constraint `name` over every row (ADD CONSTRAINT; deferred checks at
/// commit reword the error).
pub(crate) fn validate(rules: &RowRules, name: &str, rows: &[(RowId, Row)], ctx: &EvalCtx) -> SqlResult<()> {
    let Some(x) = rules.exclusions.iter().find(|x| x.name == name) else { return Ok(()) };
    let all: HashSet<RowId> = rows.iter().map(|(id, _)| *id).collect();
    let Some((new, old)) = find_conflict(x, rows, &all, ctx)? else { return Ok(()) };
    let cols = x.columns.join(", ");
    let keys = x.columns.iter().map(|c| rules.table.column_index(c));
    let detail = if crate::detail_privileges::key_visible(rules.table.oid, keys) {
        format!("Key ({cols})=({}) conflicts with key ({cols})=({}).", key_text(&old), key_text(&new))
    } else {
        "Key conflicts with key.".to_owned()
    };
    Err(SqlError::new(code::EXCLUSION_VIOLATION, format!("could not create exclusion constraint \"{name}\""))
        .with_detail(detail)
        .with_table(&rules.table.name)
        .with_constraint(name))
}
