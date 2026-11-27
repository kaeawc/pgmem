//! Constraint checks and row lookups through the storage hash indexes, so a single-row
//! statement costs O(1) in the size of its table (#218) instead of a pass over every row.
//!
//! Each function here gives the same answer as the full-scan check it stands in for; callers
//! fall back to the full scan where an index cannot (expression unique keys, exclusion
//! constraints, reads frozen for data-modifying WITH queries).

use super::{Checks, pad, unique_key, unique_violation};
use crate::bind::dml::{RowRules, UniqueCheck};
use crate::eval::EvalCtx;
use crate::plan::{Expr, Row};
use pgmem_catalog::{ColumnDef, TableDef};
use pgmem_storage::{RowId, Txn};
use pgmem_types::{GroupKey, SqlResult, Type, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Whether the unique checks of `rules` can run through index lookups.
pub(super) fn usable(rules: &RowRules, txn: &Txn) -> bool {
    rules.exclusions.is_empty() && rules.uniques.iter().all(|u| u.exprs.is_empty()) && !txn.reads_frozen()
}

fn key_values(u: &UniqueCheck, row: &Row) -> Vec<Value> {
    u.columns.iter().map(|c| row.get(*c).cloned().unwrap_or(Value::Null)).collect()
}

/// The latest rows of `table` (padded) whose key in `u` is `k`; `row` is a row with that key.
fn latest_with_key(u: &UniqueCheck, k: &[GroupKey], row: &Row, table: &TableDef, ctx: &EvalCtx) -> SqlResult<Vec<(RowId, Row)>> {
    let mut out = Vec::new();
    for (id, mut r) in ctx.txn.lookup_latest(table, &u.columns, &key_values(u, row)) {
        pad(&mut r, table);
        if unique_key(u, &r, ctx)?.as_deref() == Some(k) {
            out.push((id, r));
        }
    }
    Ok(out)
}

/// The rows of an INSERT's target table as its ON CONFLICT search sees them: the latest rows
/// (all of them, or looked up by key) with the statement's own updates applied.
pub(super) struct Existing<'a> {
    table: &'a TableDef,
    /// Every row, when the checks cannot use indexes.
    all: Option<Vec<(RowId, Row)>>,
    /// New versions of the rows this statement updated (ON CONFLICT DO UPDATE).
    pub updated: BTreeMap<RowId, Row>,
}

impl<'a> Existing<'a> {
    pub fn new(table: &'a TableDef, indexed: bool, txn: &Txn) -> Existing<'a> {
        let all = (!indexed).then(|| {
            txn.scan_latest(table.oid)
                .into_iter()
                .map(|(id, mut r)| {
                    pad(&mut r, table);
                    (id, r)
                })
                .collect()
        });
        Existing { table, all, updated: BTreeMap::new() }
    }

    /// The first row (in heap order) whose key in `u` is `k`; `row` is a row with that key.
    pub fn first_match(&self, u: &UniqueCheck, k: &[GroupKey], row: &Row, ctx: &EvalCtx) -> SqlResult<Option<(RowId, Row)>> {
        if let Some(all) = &self.all {
            for (id, r) in all {
                let cur = self.updated.get(id).unwrap_or(r);
                if unique_key(u, cur, ctx)?.as_deref() == Some(k) {
                    return Ok(Some((*id, cur.clone())));
                }
            }
            return Ok(None);
        }
        let mut found = latest_with_key(u, k, row, self.table, ctx)?.into_iter().find(|(id, _)| !self.updated.contains_key(id));
        for (id, r) in &self.updated {
            if found.as_ref().is_some_and(|(f, _)| f < id) {
                break;
            }
            if unique_key(u, r, ctx)?.as_deref() == Some(k) {
                found = Some((*id, r.clone()));
                break;
            }
        }
        Ok(found)
    }

    /// The final rows for the full-scan unique check: every row with updates applied.
    pub fn into_all(self) -> Option<Vec<(RowId, Row)>> {
        let updated = self.updated;
        self.all.map(|all| all.into_iter().map(|(id, r)| (id, updated.get(&id).cloned().unwrap_or(r))).collect())
    }
}

/// The index form of `check_uniques_where`: the table's final rows are its latest rows with
/// the `replaced` ones swapped for `changed` (in final-row order). Only a changed row can be
/// the source of a violation.
pub(super) fn check_uniques_where(
    rules: &RowRules,
    replaced: &HashSet<RowId>,
    changed: &[&Row],
    ctx: &EvalCtx,
    include: impl Fn(&UniqueCheck) -> bool,
) -> SqlResult<()> {
    let ck = Checks { env: ctx.env, state: ctx.state };
    for u in rules.uniques.iter().filter(|u| include(u)) {
        if rules.table.constraints.iter().any(|c| c.name == u.name && ck.defer(&rules.table, c)) {
            continue;
        }
        let mut seen: HashSet<Vec<GroupKey>> = HashSet::new();
        for row in changed {
            let Some(k) = unique_key(u, row, ctx)? else { continue };
            let clash = latest_with_key(u, &k, row, &rules.table, ctx)?.iter().any(|(id, _)| !replaced.contains(id));
            if clash || !seen.insert(k) {
                return Err(unique_violation(&rules.table, u, row, ctx));
            }
        }
    }
    Ok(())
}

/// The index form of `check_uniques_per_row` (non-deferrable indexes, row by row in scan
/// order): the table's rows are its latest rows, `changes` holding (id, old, new).
pub(super) fn check_uniques_per_row(rules: &RowRules, changes: &[(RowId, Row, Row)], ctx: &EvalCtx) -> SqlResult<()> {
    for u in rules.uniques.iter().filter(|u| !super::deferrable(rules, u)) {
        // The statement's changes to each key's row count so far.
        let mut delta: HashMap<Vec<GroupKey>, i64> = HashMap::new();
        for (_, old, new) in changes {
            if let Some(k) = unique_key(u, old, ctx)? {
                *delta.entry(k).or_default() -= 1;
            }
            if let Some(k) = unique_key(u, new, ctx)? {
                let stored = i64::try_from(latest_with_key(u, &k, new, &rules.table, ctx)?.len()).unwrap_or(i64::MAX);
                let d = delta.entry(k).or_default();
                if stored + *d > 0 {
                    return Err(unique_violation(&rules.table, u, new, ctx));
                }
                *d += 1;
            }
        }
    }
    Ok(())
}

/// Whether every stored value of `col` equal (by `=`) to `v` has `v`'s group key, so an
/// index lookup finds every row `col = v` accepts.
fn keyed_alike(col: &ColumnDef, v: &Value) -> bool {
    match (&col.ty, v) {
        (Type::Int2 | Type::Int4 | Type::Int8, Value::Int2(_) | Value::Int4(_) | Value::Int8(_))
        | (Type::Uuid, Value::Uuid(_))
        | (Type::Bool, Value::Bool(_))
        | (Type::Date, Value::Date(_))
        | (Type::Timestamp, Value::Timestamp(_))
        | (Type::Timestamptz, Value::Timestamptz(_))
        | (Type::Bytea, Value::Bytea(_)) => true,
        (Type::Text | Type::Varchar, Value::Text(_)) => col.collation.is_none(),
        _ => false,
    }
}

/// A `column = constant` conjunct of a WHERE over `table`'s rows that an index lookup can
/// answer: (column slot, value).
pub(super) fn probe(table: &TableDef, filter: &Expr, ctx: &EvalCtx) -> Option<(usize, Value)> {
    match filter {
        Expr::And(parts) => parts.iter().find_map(|p| probe(table, p, ctx)),
        Expr::Call { func, args } if *func == crate::funcs::EQ && args.len() == 2 => {
            let (slot, other) = match (&args[0], &args[1]) {
                (Expr::Col(i), e) | (e, Expr::Col(i)) => (*i, e),
                _ => return None,
            };
            let v = match other {
                Expr::Const(v) => v.clone(),
                Expr::Param(p) => ctx.params.get(*p)?.clone(),
                _ => return None,
            };
            let col = table.columns.get(slot)?;
            (!col.dropped && keyed_alike(col, &v)).then_some((slot, v))
        }
        _ => None,
    }
}
