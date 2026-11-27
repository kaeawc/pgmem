//! Rewrites of expressions together with the plans of their subqueries, which see the
//! enclosing query's row through outer references (`Expr::Outer`): an aggregate of an
//! enclosing level bound inside a subquery, and the outer references of a grouped query's
//! subqueries, which must read the grouped row.

use crate::plan::{AggCall, CtePlan, Expr, FrameBound, Rel, SortKey, SubqueryKind, WinCall};
use pgmem_types::SqlResult;

/// Called on each node with its subquery nesting depth (0 for the expression itself, 1 inside
/// one of its subqueries' plans, ...): `Some` replaces the node, `None` descends into it.
pub(crate) type Visit<'a> = dyn FnMut(&Expr, usize) -> SqlResult<Option<Expr>> + 'a;

/// Rewrites `e` and every expression of the subquery plans it holds.
pub(crate) fn rewrite_expr(e: Expr, k: usize, f: &mut Visit) -> SqlResult<Expr> {
    if let Some(r) = f(&e, k)? {
        return Ok(r);
    }
    Ok(match e {
        Expr::Subquery { id, plan, kind, correlated } => {
            let kind = match kind {
                SubqueryKind::Quantified { probe, cmp, all } => {
                    SubqueryKind::Quantified { probe: Box::new(rewrite_expr(*probe, k, f)?), cmp, all }
                }
                other => other,
            };
            Expr::Subquery { id, plan: Box::new(rewrite_rel(*plan, k + 1, f)?), kind, correlated }
        }
        // An aggregate of an enclosing level has its arguments in that level's terms.
        Expr::AggCall(a) if a.levels_up == 0 => Expr::AggCall(Box::new(rewrite_agg(*a, k, f)?)),
        Expr::WindowCall(w) => Expr::WindowCall(Box::new(rewrite_win(*w, k, f)?)),
        other => super::select::map_children(other, &mut |c| rewrite_expr(c, k, f))?,
    })
}

fn exprs(v: Vec<Expr>, k: usize, f: &mut Visit) -> SqlResult<Vec<Expr>> {
    v.into_iter().map(|e| rewrite_expr(e, k, f)).collect()
}

fn opt(e: Option<Expr>, k: usize, f: &mut Visit) -> SqlResult<Option<Expr>> {
    e.map(|e| rewrite_expr(e, k, f)).transpose()
}

fn keys(v: Vec<SortKey>, k: usize, f: &mut Visit) -> SqlResult<Vec<SortKey>> {
    v.into_iter().map(|s| Ok(SortKey { expr: rewrite_expr(s.expr, k, f)?, ..s })).collect()
}

pub(crate) fn rewrite_agg(mut a: AggCall, k: usize, f: &mut Visit) -> SqlResult<AggCall> {
    a.args = exprs(a.args, k, f)?;
    a.filter = opt(a.filter, k, f)?;
    a.order_by = keys(a.order_by, k, f)?;
    Ok(a)
}

fn rewrite_win(mut w: WinCall, k: usize, f: &mut Visit) -> SqlResult<WinCall> {
    w.args = exprs(w.args, k, f)?;
    w.partition_by = exprs(w.partition_by, k, f)?;
    w.order_by = keys(w.order_by, k, f)?;
    w.agg = w.agg.map(|a| rewrite_agg(a, k, f)).transpose()?;
    let bound = |b: FrameBound, f: &mut Visit| -> SqlResult<FrameBound> {
        Ok(match b {
            FrameBound::Preceding(e) => FrameBound::Preceding(rewrite_expr(e, k, f)?),
            FrameBound::Following(e) => FrameBound::Following(rewrite_expr(e, k, f)?),
            other => other,
        })
    };
    w.frame.start = bound(w.frame.start, f)?;
    w.frame.end = bound(w.frame.end, f)?;
    w.frame.range_offsets = exprs(w.frame.range_offsets, k, f)?;
    w.frame.range_key = opt(w.frame.range_key, k, f)?;
    Ok(w)
}

/// Rewrites every expression of a plan whose rows are evaluated at nesting depth `k` (the
/// right side of a lateral join sees its left row as one more enclosing level).
pub(crate) fn rewrite_rel(rel: Rel, k: usize, f: &mut Visit) -> SqlResult<Rel> {
    let b = |r: Box<Rel>, k: usize, f: &mut Visit| -> SqlResult<Box<Rel>> { Ok(Box::new(rewrite_rel(*r, k, f)?)) };
    Ok(match rel {
        leaf @ (Rel::Virtual { .. } | Rel::Scan { .. } | Rel::CteScan { .. }) => leaf,
        Rel::Values { rows } => Rel::Values { rows: rows.into_iter().map(|r| exprs(r, k, f)).collect::<SqlResult<_>>()? },
        Rel::ProjectSet { input, srfs } => Rel::ProjectSet {
            input: b(input, k, f)?,
            srfs: srfs.into_iter().map(|(s, args, n)| Ok((s, exprs(args, k, f)?, n))).collect::<SqlResult<_>>()?,
        },
        Rel::FuncScan { srf, args, ordinality } => Rel::FuncScan { srf, args: exprs(args, k, f)?, ordinality },
        Rel::Filter { input, pred } => Rel::Filter { input: b(input, k, f)?, pred: rewrite_expr(pred, k, f)? },
        Rel::Project { input, exprs: es } => Rel::Project { input: b(input, k, f)?, exprs: exprs(es, k, f)? },
        Rel::Join { kind, left, right, on, lateral, left_width, right_width } => Rel::Join {
            kind,
            left: b(left, k, f)?,
            right: b(right, if lateral { k + 1 } else { k }, f)?,
            on: opt(on, k, f)?,
            lateral,
            left_width,
            right_width,
        },
        Rel::Aggregate { input, group, aggs, sets } => Rel::Aggregate {
            input: b(input, k, f)?,
            group: exprs(group, k, f)?,
            aggs: aggs.into_iter().map(|a| rewrite_agg(a, k, f)).collect::<SqlResult<_>>()?,
            sets,
        },
        Rel::Window { input, calls } => {
            Rel::Window { input: b(input, k, f)?, calls: calls.into_iter().map(|w| rewrite_win(w, k, f)).collect::<SqlResult<_>>()? }
        }
        Rel::Sort { input, keys: ks } => Rel::Sort { input: b(input, k, f)?, keys: keys(ks, k, f)? },
        Rel::Limit { input, limit, offset, ties } => Rel::Limit {
            input: b(input, k, f)?,
            limit: opt(limit, k, f)?,
            offset: opt(offset, k, f)?,
            ties: ties.map(|t| keys(t, k, f)).transpose()?,
        },
        Rel::Distinct { input, on } => Rel::Distinct { input: b(input, k, f)?, on: on.map(|o| exprs(o, k, f)).transpose()? },
        Rel::SetOp { op, all, left, right } => Rel::SetOp { op, all, left: b(left, k, f)?, right: b(right, k, f)? },
        Rel::LockRows { input, targets, mode, wait, limit, offset, visible } => {
            Rel::LockRows { input: b(input, k, f)?, targets, mode, wait, limit: opt(limit, k, f)?, offset: opt(offset, k, f)?, visible }
        }
        Rel::With { ctes, body } => Rel::With {
            ctes: ctes
                .into_iter()
                .map(|c| {
                    Ok(CtePlan {
                        id: c.id,
                        plan: rewrite_rel(c.plan, k, f)?,
                        recursive: c.recursive.map(|(r, all)| Ok::<_, pgmem_types::SqlError>((rewrite_rel(r, k, f)?, all))).transpose()?,
                    })
                })
                .collect::<SqlResult<_>>()?,
            body: b(body, k, f)?,
        },
    })
}

/// The query level an aggregate call belongs to, counted out from where it appears: the
/// nearest level its arguments, FILTER and ORDER BY reference (0 when they reference none).
pub(crate) fn agg_level(a: &AggCall) -> usize {
    let mut level: Option<usize> = None;
    let mut note = |e: &Expr, k: usize| -> SqlResult<Option<Expr>> {
        let at = match e {
            Expr::Col(_) if k == 0 => Some(0),
            Expr::Outer { depth, .. } if *depth >= k => Some(depth - k),
            _ => None,
        };
        if let Some(at) = at {
            level = Some(level.map_or(at, |l: usize| l.min(at)));
        }
        Ok(None)
    };
    // The visitor never fails.
    let _ = rewrite_agg(a.clone(), 0, &mut note);
    level.unwrap_or(0)
}

/// Rebases an aggregate's arguments onto the level `up` levels out, which it belongs to.
pub(crate) fn rebase_agg(a: AggCall, up: usize) -> SqlResult<AggCall> {
    rewrite_agg(a, 0, &mut |e, k| {
        Ok(match e {
            Expr::Outer { depth, idx } if *depth >= k + up => {
                Some(if *depth == up { Expr::Col(*idx) } else { Expr::Outer { depth: depth - up, idx: *idx } })
            }
            _ => None,
        })
    })
}

/// Whether `e` reads anything of an enclosing query (a column, or an aggregate of that level).
fn references_outside(e: &Expr) -> bool {
    let mut found = false;
    let _ = rewrite_expr(e.clone(), 0, &mut |x, k| {
        found |= match x {
            Expr::Outer { depth, .. } => *depth > k,
            Expr::AggCall(a) => a.levels_up > k,
            _ => false,
        };
        Ok(None)
    });
    found
}

/// An EXISTS subquery's plan without the parts that cannot change whether it returns a row:
/// its select list, ORDER BY, DISTINCT, a positive constant LIMIT and a GROUP BY without
/// aggregates (Postgres' `simplify_EXISTS_query`), so their volatile functions do not run.
/// Kept when the query aggregates, has window functions, set-returning functions in its
/// select list, HAVING, OFFSET or a locking clause, or when a dropped expression references an
/// enclosing query (which the enclosing query's grouping checks still need to see).
pub(crate) fn simplify_exists(rel: Rel) -> Rel {
    if !exists_simplifiable(&rel) {
        return rel;
    }
    fn strip(rel: Rel) -> Rel {
        match rel {
            Rel::Limit { input, .. } | Rel::Distinct { input, .. } | Rel::Sort { input, .. } => strip(*input),
            Rel::Project { input, .. } if is_layer(&input) => strip(*input),
            Rel::Project { input, .. } => match *input {
                Rel::Aggregate { input, .. } => Rel::Project { input, exprs: Vec::new() },
                other => Rel::Project { input: Box::new(other), exprs: Vec::new() },
            },
            other => other,
        }
    }
    strip(rel)
}

/// The nodes a SELECT puts over its select list's input.
fn is_layer(rel: &Rel) -> bool {
    matches!(rel, Rel::Limit { .. } | Rel::Distinct { .. } | Rel::Sort { .. } | Rel::Project { .. })
}

fn exists_simplifiable(rel: &Rel) -> bool {
    match rel {
        Rel::Limit { input, limit, offset: None, .. } => {
            let positive = match limit {
                None => true,
                Some(Expr::Const(v)) => v.is_null() || v.as_i64().is_some_and(|n| n > 0),
                Some(_) => false,
            };
            positive && exists_simplifiable(input)
        }
        Rel::Distinct { input, on } => on.iter().flatten().all(|e| !references_outside(e)) && exists_simplifiable(input),
        Rel::Sort { input, keys } => keys.iter().all(|k| !references_outside(&k.expr)) && exists_simplifiable(input),
        Rel::Project { input, exprs } => {
            exprs.iter().all(|e| !references_outside(e))
                && if is_layer(input) { exists_simplifiable(input) } else { base_simplifiable(input) }
        }
        _ => false,
    }
}

/// The input of the select list: the query's FROM and WHERE, or a GROUP BY without
/// aggregates over them.
fn base_simplifiable(rel: &Rel) -> bool {
    match rel {
        Rel::Aggregate { aggs, sets: None, group, .. } => aggs.is_empty() && group.iter().all(|g| !references_outside(g)),
        Rel::Aggregate { .. } | Rel::Window { .. } | Rel::ProjectSet { .. } | Rel::LockRows { .. } | Rel::SetOp { .. } => false,
        // HAVING.
        Rel::Filter { input, .. } => !matches!(**input, Rel::Aggregate { .. }),
        _ => true,
    }
}
