//! Relational operator execution. Every operator materializes its output: pgmem optimizes for
//! fidelity and simplicity, not speed.

use crate::agg::{Accumulator, run_aggregate};
use crate::eval::{EvalCtx, eval, eval_pred};
use crate::plan::{JoinKind, Rel, Row, SetOpKind, SortKey};
use crate::window::run_window;
use pgmem_types::{GroupKey, SqlError, SqlResult, Value, code, compare_values};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

pub fn run(rel: &Rel, ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    match rel {
        Rel::Values { rows } => rows.iter().map(|r| r.iter().map(|e| eval(e, &[], ctx)).collect()).collect(),
        Rel::Virtual { rows } => (rows.0)(ctx),
        Rel::Scan { table, slots, missing } => Ok(ctx
            .txn
            .scan(*table)
            .into_iter()
            .map(|(_, stored)| slots.iter().zip(missing).map(|(s, m)| stored.get(*s).cloned().unwrap_or_else(|| m.clone())).collect())
            .collect()),
        Rel::FuncScan { srf, args, ordinality } => {
            let vals = args.iter().map(|a| eval(a, &[], ctx)).collect::<SqlResult<Vec<_>>>()?;
            let mut rows = (srf.f)(&vals, ctx)?;
            if *ordinality {
                for (i, r) in rows.iter_mut().enumerate() {
                    r.push(Value::Int8(i64::try_from(i + 1).unwrap_or(i64::MAX)));
                }
            }
            Ok(rows)
        }
        Rel::Filter { input, pred } => {
            let rows = run(input, ctx)?;
            let mut out = Vec::with_capacity(rows.len());
            for r in rows {
                if eval_pred(pred, &r, ctx)? {
                    out.push(r);
                }
            }
            Ok(out)
        }
        Rel::Project { input, exprs } => {
            let rows = run(input, ctx)?;
            rows.iter().map(|r| exprs.iter().map(|e| eval(e, r, ctx)).collect()).collect()
        }
        Rel::Join { kind, left, right, on, lateral, left_width, right_width } => {
            run_join(*kind, left, right, on.as_ref(), *lateral, *left_width, *right_width, ctx)
        }
        Rel::Aggregate { input, group, aggs } => run_aggregate(&run(input, ctx)?, group, aggs, ctx),
        Rel::Window { input, calls } => run_window(run(input, ctx)?, calls, ctx),
        Rel::Sort { input, keys } => {
            let mut rows = run(input, ctx)?;
            sort_rows(&mut rows, keys, ctx)?;
            Ok(rows)
        }
        Rel::Limit { input, limit, offset, ties } => {
            let rows = run(input, ctx)?;
            let off = match offset {
                None => 0,
                Some(e) => match eval(e, &[], ctx)? {
                    Value::Null => 0,
                    v => {
                        let n = v.as_i64().ok_or_else(|| SqlError::internal("OFFSET must be an integer"))?;
                        if n < 0 {
                            return Err(SqlError::new("2201X", "OFFSET must not be negative"));
                        }
                        usize::try_from(n).unwrap_or(usize::MAX)
                    }
                },
            };
            let lim = match limit {
                None => None,
                Some(e) => match eval(e, &[], ctx)? {
                    Value::Null => None,
                    v => {
                        let n = v.as_i64().ok_or_else(|| SqlError::internal("LIMIT must be an integer"))?;
                        if n < 0 {
                            return Err(SqlError::new("2201W", "LIMIT must not be negative"));
                        }
                        Some(usize::try_from(n).unwrap_or(usize::MAX))
                    }
                },
            };
            let mut rows: Vec<Row> = rows.into_iter().skip(off).collect();
            if let Some(n) = lim
                && n < rows.len()
            {
                let mut end = n;
                if let Some(keys) = ties
                    && n > 0
                {
                    let key = |r: &Row| keys.iter().map(|k| eval(&k.expr, r, ctx)).collect::<SqlResult<Vec<_>>>();
                    let last = key(&rows[n - 1])?;
                    while end < rows.len() && key(&rows[end])?.iter().zip(&last).all(|(a, b)| a.group_key() == b.group_key()) {
                        end += 1;
                    }
                }
                rows.truncate(end);
            }
            Ok(rows)
        }
        Rel::Distinct { input, on } => {
            let rows = run(input, ctx)?;
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for r in rows {
                let key: Vec<GroupKey> = match on {
                    None => r.iter().map(Value::group_key).collect(),
                    Some(exprs) => exprs.iter().map(|e| eval(e, &r, ctx).map(|v| v.group_key())).collect::<SqlResult<Vec<_>>>()?,
                };
                if seen.insert(key) {
                    out.push(r);
                }
            }
            Ok(out)
        }
        Rel::SetOp { op, all, left, right } => {
            let l = run(left, ctx)?;
            let r = run(right, ctx)?;
            Ok(set_op(*op, *all, l, r))
        }
        Rel::CteScan { id } => {
            let rows = ctx.state.ctes.borrow().get(id).cloned().ok_or_else(|| SqlError::internal(format!("CTE {id} not materialized")))?;
            Ok(rows.as_ref().clone())
        }
        Rel::With { ctes, body } => {
            for cte in ctes {
                let rows = match &cte.recursive {
                    None => run(&cte.plan, ctx)?,
                    Some((step, union_all)) => run_recursive(cte.id, &cte.plan, step, *union_all, ctx)?,
                };
                ctx.state.ctes.borrow_mut().insert(cte.id, Rc::new(rows));
            }
            run(body, ctx)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_join(
    kind: JoinKind,
    left: &Rel,
    right: &Rel,
    on: Option<&crate::plan::Expr>,
    lateral: bool,
    lw: usize,
    rw: usize,
    ctx: &EvalCtx,
) -> SqlResult<Vec<Row>> {
    let lrows = run(left, ctx)?;
    let shared_right = if lateral { None } else { Some(run(right, ctx)?) };
    let mut out = Vec::new();
    let mut right_matched = vec![false; shared_right.as_ref().map_or(0, Vec::len)];
    for l in &lrows {
        let lateral_rows;
        let rrows: &[Row] = match &shared_right {
            Some(r) => r,
            None => {
                lateral_rows = run(right, &ctx.child(l))?;
                &lateral_rows
            }
        };
        let mut matched = false;
        for (ri, r) in rrows.iter().enumerate() {
            let mut combined = Vec::with_capacity(lw + rw);
            combined.extend_from_slice(l);
            combined.extend_from_slice(r);
            let ok = match on {
                None => true,
                Some(pred) => eval_pred(pred, &combined, ctx)?,
            };
            if ok {
                matched = true;
                if shared_right.is_some() && ri < right_matched.len() {
                    right_matched[ri] = true;
                }
                out.push(combined);
            }
        }
        if !matched && matches!(kind, JoinKind::Left | JoinKind::Full) {
            let mut combined = l.clone();
            combined.extend(std::iter::repeat_n(Value::Null, rw));
            out.push(combined);
        }
    }
    if matches!(kind, JoinKind::Right | JoinKind::Full)
        && let Some(rrows) = &shared_right
    {
        for (ri, r) in rrows.iter().enumerate() {
            if !right_matched[ri] {
                let mut combined: Row = std::iter::repeat_n(Value::Null, lw).collect();
                combined.extend_from_slice(r);
                out.push(combined);
            }
        }
    }
    Ok(out)
}

fn run_recursive(id: usize, base: &Rel, step: &Rel, union_all: bool, ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let mut seen = std::collections::HashSet::new();
    let mut all_rows = Vec::new();
    let mut working = Vec::new();
    for r in run(base, ctx)? {
        if union_all || seen.insert(r.iter().map(Value::group_key).collect::<Vec<_>>()) {
            all_rows.push(r.clone());
            working.push(r);
        }
    }
    let mut iterations = 0usize;
    while !working.is_empty() {
        iterations += 1;
        if iterations > 1_000_000 {
            return Err(SqlError::new(code::QUERY_CANCELED, "recursive query did not terminate"));
        }
        ctx.state.ctes.borrow_mut().insert(id, Rc::new(std::mem::take(&mut working)));
        for r in run(step, ctx)? {
            if union_all || seen.insert(r.iter().map(Value::group_key).collect::<Vec<_>>()) {
                all_rows.push(r.clone());
                working.push(r);
            }
        }
    }
    Ok(all_rows)
}

fn set_op(op: SetOpKind, all: bool, left: Vec<Row>, right: Vec<Row>) -> Vec<Row> {
    let key = |r: &Row| r.iter().map(Value::group_key).collect::<Vec<_>>();
    match op {
        SetOpKind::Union => {
            let mut out = left;
            out.extend(right);
            if all {
                out
            } else {
                let mut seen = std::collections::HashSet::new();
                out.into_iter().filter(|r| seen.insert(key(r))).collect()
            }
        }
        SetOpKind::Intersect | SetOpKind::Except => {
            let mut counts: HashMap<Vec<GroupKey>, usize> = HashMap::new();
            for r in &right {
                *counts.entry(key(r)).or_default() += 1;
            }
            let mut emitted = std::collections::HashSet::new();
            let mut out = Vec::new();
            for r in left {
                let k = key(&r);
                let c = counts.get(&k).copied().unwrap_or(0);
                match (op, all) {
                    (SetOpKind::Intersect, true) => {
                        if c > 0 {
                            counts.insert(k, c - 1);
                            out.push(r);
                        }
                    }
                    (SetOpKind::Intersect, false) => {
                        if c > 0 && emitted.insert(k) {
                            out.push(r);
                        }
                    }
                    (SetOpKind::Except, true) => {
                        if c > 0 {
                            counts.insert(k, c - 1);
                        } else {
                            out.push(r);
                        }
                    }
                    (SetOpKind::Except, false) => {
                        if c == 0 && emitted.insert(k) {
                            out.push(r);
                        }
                    }
                    (SetOpKind::Union, _) => {}
                }
            }
            out
        }
    }
}

/// Compares two values for ORDER BY: NULL placement per key, then the type's order.
pub fn compare_sort(a: &Value, b: &Value, desc: bool, nulls_first: bool) -> SqlResult<Ordering> {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ok(Ordering::Equal),
        (true, false) => Ok(if nulls_first { Ordering::Less } else { Ordering::Greater }),
        (false, true) => Ok(if nulls_first { Ordering::Greater } else { Ordering::Less }),
        (false, false) => {
            let o = compare_values(a, b)?;
            Ok(if desc { o.reverse() } else { o })
        }
    }
}

/// Stable sort by keys (evaluated once per row).
pub fn sort_rows(rows: &mut Vec<Row>, keys: &[SortKey], ctx: &EvalCtx) -> SqlResult<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
    for r in rows.drain(..) {
        let k = keys.iter().map(|k| eval(&k.expr, &r, ctx)).collect::<SqlResult<Vec<_>>>()?;
        keyed.push((k, r));
    }
    let mut err = None;
    keyed.sort_by(|(a, _), (b, _)| {
        for (i, k) in keys.iter().enumerate() {
            match compare_sort(&a[i], &b[i], k.desc, k.nulls_first) {
                Ok(Ordering::Equal) => continue,
                Ok(o) => return o,
                Err(e) => {
                    err.get_or_insert(e);
                    return Ordering::Equal;
                }
            }
        }
        Ordering::Equal
    });
    if let Some(e) = err {
        return Err(e);
    }
    rows.extend(keyed.into_iter().map(|(_, r)| r));
    Ok(())
}

#[allow(dead_code)]
fn unused(_: &Accumulator) {}
