//! Expression evaluation.

use crate::cast::cast_value;
use crate::plan::{Expr, Row, SubqueryKind};
use crate::run::run;
use crate::session::SessionEnv;
use pgmem_catalog::Oid;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, Value, code};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Per-statement mutable state shared by every evaluation in the statement.
#[derive(Default)]
pub struct StatementState {
    /// Materialized CTEs (and recursive working tables) by id.
    pub ctes: RefCell<HashMap<usize, Rc<Vec<Row>>>>,
    /// Results of uncorrelated subqueries, computed once per statement.
    pub subqueries: RefCell<HashMap<usize, Rc<Vec<Row>>>>,
    /// `currval` values set by `nextval` during this statement (merged into the session).
    pub currval: RefCell<HashMap<Oid, i64>>,
}

/// Everything an expression needs at run time.
pub struct EvalCtx<'a> {
    pub txn: &'a Txn,
    pub params: &'a [Value],
    pub env: &'a SessionEnv,
    pub state: &'a StatementState,
    /// Rows of enclosing queries, innermost last.
    pub outer: Vec<Row>,
}

impl<'a> EvalCtx<'a> {
    pub fn new(txn: &'a Txn, params: &'a [Value], env: &'a SessionEnv, state: &'a StatementState) -> EvalCtx<'a> {
        EvalCtx { txn, params, env, state, outer: Vec::new() }
    }

    /// A context for a subquery evaluated on behalf of `row`.
    pub fn child(&self, row: &[Value]) -> EvalCtx<'a> {
        let mut outer = self.outer.clone();
        outer.push(row.to_vec());
        EvalCtx { txn: self.txn, params: self.params, env: self.env, state: self.state, outer }
    }
}

pub fn eval(expr: &Expr, row: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    Ok(match expr {
        Expr::Const(v) => v.clone(),
        Expr::Col(i) => {
            row.get(*i).cloned().ok_or_else(|| SqlError::internal(format!("column index {i} out of range ({} columns)", row.len())))?
        }
        Expr::Outer { depth, idx } => {
            let level =
                ctx.outer.len().checked_sub(*depth).ok_or_else(|| SqlError::internal("outer reference without an enclosing row"))?;
            ctx.outer[level].get(*idx).cloned().ok_or_else(|| SqlError::internal("outer column index out of range"))?
        }
        Expr::Param(i) => ctx
            .params
            .get(*i)
            .cloned()
            .ok_or_else(|| SqlError::new(code::UNDEFINED_PARAMETER, format!("there is no parameter ${}", i + 1)))?,
        Expr::Call { func, args } => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                let v = eval(a, row, ctx)?;
                if func.strict && v.is_null() {
                    return Ok(Value::Null);
                }
                vals.push(v);
            }
            (func.f)(&vals, ctx)?
        }
        Expr::Cast { expr, to, typmod } => {
            let v = eval(expr, row, ctx)?;
            match to {
                pgmem_types::Type::Reg(kind) => crate::reg::to_reg(v, *kind, ctx.txn.catalog())?,
                _ => cast_value(v, to, *typmod)?,
            }
        }
        Expr::And(items) => {
            let mut saw_null = false;
            for e in items {
                match eval(e, row, ctx)? {
                    Value::Bool(false) => return Ok(Value::Bool(false)),
                    Value::Null => saw_null = true,
                    _ => {}
                }
            }
            if saw_null { Value::Null } else { Value::Bool(true) }
        }
        Expr::Or(items) => {
            let mut saw_null = false;
            for e in items {
                match eval(e, row, ctx)? {
                    Value::Bool(true) => return Ok(Value::Bool(true)),
                    Value::Null => saw_null = true,
                    _ => {}
                }
            }
            if saw_null { Value::Null } else { Value::Bool(false) }
        }
        Expr::Not(e) => match eval(e, row, ctx)? {
            Value::Bool(b) => Value::Bool(!b),
            _ => Value::Null,
        },
        Expr::IsNull { expr, negated } => {
            let v = eval(expr, row, ctx)?;
            // For row values Postgres checks every field; arrays and scalars are NULL or not.
            Value::Bool(v.is_null() != *negated)
        }
        Expr::IsDistinct { left, right, negated } => {
            let l = eval(left, row, ctx)?;
            let r = eval(right, row, ctx)?;
            let distinct = match (l.is_null(), r.is_null()) {
                (true, true) => false,
                (true, false) | (false, true) => true,
                (false, false) => !pgmem_types::compare_values(&l, &r)?.is_eq(),
            };
            Value::Bool(distinct != *negated)
        }
        Expr::Case { operand, whens, else_result } => {
            let op = operand.as_ref().map(|o| eval(o, row, ctx)).transpose()?;
            for (w, t) in whens {
                let hit = match &op {
                    // The simple form's WHEN is bound as `operand = value` already.
                    Some(_) | None => eval(w, row, ctx)?.is_true(),
                };
                if hit {
                    return eval(t, row, ctx);
                }
            }
            match else_result {
                Some(e) => eval(e, row, ctx)?,
                None => Value::Null,
            }
        }
        Expr::Coalesce(items) => {
            for e in items {
                let v = eval(e, row, ctx)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Value::Null
        }
        Expr::MinMax { args, greatest } => {
            let mut best: Option<Value> = None;
            for e in args {
                let v = eval(e, row, ctx)?;
                if v.is_null() {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let ord = pgmem_types::compare_values(&v, &b)?;
                        if (*greatest && ord.is_gt()) || (!*greatest && ord.is_lt()) { v } else { b }
                    }
                });
            }
            best.unwrap_or(Value::Null)
        }
        Expr::NullIf(a, b) => {
            let l = eval(a, row, ctx)?;
            let r = eval(b, row, ctx)?;
            if !l.is_null() && !r.is_null() && pgmem_types::compare_values(&l, &r)?.is_eq() { Value::Null } else { l }
        }
        Expr::InList { probe, list, cmp, negated } => {
            let p = eval(probe, row, ctx)?;
            if p.is_null() {
                return Ok(Value::Null);
            }
            let mut saw_null = false;
            for e in list {
                let v = eval(e, row, ctx)?;
                if v.is_null() {
                    saw_null = true;
                    continue;
                }
                if (cmp.f)(&[p.clone(), v], ctx)?.is_true() {
                    return Ok(Value::Bool(!*negated));
                }
            }
            if saw_null { Value::Null } else { Value::Bool(*negated) }
        }
        Expr::AnyAll { left, array, cmp, all } => {
            let l = eval(left, row, ctx)?;
            let arr = eval(array, row, ctx)?;
            let items = match arr {
                Value::Null => return Ok(Value::Null),
                Value::Array(items) => items,
                other => return Err(SqlError::internal(format!("ANY/ALL over non-array {}", other.kind()))),
            };
            quantified(&l, items.into_iter(), cmp, *all, ctx)?
        }
        Expr::Subquery { id, plan, kind, correlated } => {
            let rows = if *correlated {
                Rc::new(run(plan, &ctx.child(row))?)
            } else {
                let cached = ctx.state.subqueries.borrow().get(id).cloned();
                match cached {
                    Some(r) => r,
                    None => {
                        let r = Rc::new(run(plan, &ctx.child(row))?);
                        ctx.state.subqueries.borrow_mut().insert(*id, Rc::clone(&r));
                        r
                    }
                }
            };
            match kind {
                SubqueryKind::Exists => Value::Bool(!rows.is_empty()),
                SubqueryKind::Scalar => match rows.len() {
                    0 => Value::Null,
                    1 => rows[0].first().cloned().unwrap_or(Value::Null),
                    _ => {
                        return Err(SqlError::new(
                            code::CARDINALITY_VIOLATION,
                            "more than one row returned by a subquery used as an expression",
                        ));
                    }
                },
                SubqueryKind::Array => Value::Array(rows.iter().map(|r| r.first().cloned().unwrap_or(Value::Null)).collect()),
                SubqueryKind::Quantified { probe, cmp, all } => {
                    let p = eval(probe, row, ctx)?;
                    quantified(&p, rows.iter().map(|r| r.first().cloned().unwrap_or(Value::Null)), cmp, *all, ctx)?
                }
            }
        }
        Expr::Array(items) => Value::Array(items.iter().map(|e| eval(e, row, ctx)).collect::<SqlResult<Vec<_>>>()?),
        Expr::Subscript { expr, index, upper, slice } => {
            let arr = eval(expr, row, ctx)?;
            let Value::Array(items) = arr else { return Ok(Value::Null) };
            let idx = eval(index, row, ctx)?;
            if *slice {
                let lo = if idx.is_null() { 1 } else { idx.as_i64().unwrap_or(1) };
                let hi = match upper {
                    Some(u) => match eval(u, row, ctx)? {
                        Value::Null => i64::try_from(items.len()).unwrap_or(i64::MAX),
                        v => v.as_i64().unwrap_or(0),
                    },
                    None => i64::try_from(items.len()).unwrap_or(i64::MAX),
                };
                let lo = lo.max(1);
                let hi = hi.min(i64::try_from(items.len()).unwrap_or(i64::MAX));
                if lo > hi {
                    return Ok(Value::Array(Vec::new()));
                }
                let (lo, hi) = (usize::try_from(lo - 1).unwrap_or(0), usize::try_from(hi).unwrap_or(0));
                return Ok(Value::Array(items[lo..hi].to_vec()));
            }
            match idx.as_i64() {
                Some(i) if i >= 1 => items.get(usize::try_from(i - 1).unwrap_or(usize::MAX)).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            }
        }
        Expr::Row { fields, type_oid, .. } => {
            Value::Record(*type_oid, fields.iter().map(|e| eval(e, row, ctx)).collect::<SqlResult<Vec<_>>>()?)
        }
        Expr::Field { expr, idx } => match eval(expr, row, ctx)? {
            Value::Record(_, mut fields) if *idx < fields.len() => fields.swap_remove(*idx),
            Value::Null => Value::Null,
            other => return Err(SqlError::internal(format!("field selection from {}", other.kind()))),
        },
        Expr::FieldStore { expr, idx, value, type_oid, width } => {
            let mut fields = match eval(expr, row, ctx)? {
                Value::Record(_, fields) => fields,
                _ => vec![Value::Null; *width],
            };
            let v = eval(value, row, ctx)?;
            if let Some(slot) = fields.get_mut(*idx) {
                *slot = v;
            }
            Value::Record(*type_oid, fields)
        }
        Expr::CoerceToDomain(d) => {
            let v = eval(&d.expr, row, ctx)?;
            check_domain(d, &v, ctx)?;
            v
        }
        Expr::AggCall(_) | Expr::WindowCall(_) => return Err(SqlError::internal("unresolved aggregate or window call")),
    })
}

/// `x op ANY/ALL (items)` with three-valued logic.
fn quantified(l: &Value, items: impl Iterator<Item = Value>, cmp: &crate::plan::Func, all: bool, ctx: &EvalCtx) -> SqlResult<Value> {
    let mut saw_null = false;
    let mut any_item = false;
    for item in items {
        any_item = true;
        if l.is_null() || item.is_null() {
            saw_null = true;
            continue;
        }
        let hit = (cmp.f)(&[l.clone(), item], ctx)?.is_true();
        if all && !hit {
            return Ok(Value::Bool(false));
        }
        if !all && hit {
            return Ok(Value::Bool(true));
        }
    }
    if !any_item {
        return Ok(Value::Bool(all));
    }
    if saw_null { Ok(Value::Null) } else { Ok(Value::Bool(all)) }
}

/// Evaluates a predicate: true only for SQL TRUE.
/// `ExecEvalConstraintNotNull` / `ExecEvalConstraintCheck`: a CHECK that yields NULL passes.
fn check_domain(d: &crate::plan::DomainCoercion, v: &Value, ctx: &EvalCtx) -> SqlResult<()> {
    if d.not_null && v.is_null() {
        return Err(SqlError::new(code::NOT_NULL_VIOLATION, format!("domain {} does not allow null values", d.domain)));
    }
    let one = std::slice::from_ref(v);
    for (name, check) in &d.checks {
        if matches!(eval(check, one, ctx)?, Value::Bool(false)) {
            return Err(SqlError::new(
                code::CHECK_VIOLATION,
                format!("value for domain {} violates check constraint \"{name}\"", d.domain),
            )
            .with_constraint(name.clone()));
        }
    }
    Ok(())
}

pub fn eval_pred(expr: &Expr, row: &[Value], ctx: &EvalCtx) -> SqlResult<bool> {
    Ok(eval(expr, row, ctx)?.is_true())
}
