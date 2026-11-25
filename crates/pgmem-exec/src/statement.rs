//! Running a query or DML statement inside a transaction, including statements whose WITH
//! holds data-modifying queries (`WITH d AS (DELETE ... RETURNING *) INSERT ... SELECT * FROM d`).
//!
//! As in Postgres, every WITH query runs exactly once, in order, and every part of the
//! statement reads the snapshot taken at its start: the main query does not see the rows the
//! WITH queries changed, only what they RETURNed.

use crate::bind::{self, Binder, CteBinding, Ctx};
use crate::eval::{EvalCtx, StatementState};
use crate::run::run;
use crate::session::{QueryResult, SessionEnv};
use pgmem_ir::{self as ir, Statement};
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, Value, code};
use std::rc::Rc;

/// The top-level WITH that has to run before the statement: any WITH of a DML statement, or a
/// query's WITH that contains a data-modifying query.
pub(crate) fn leading_with(stmt: &Statement) -> Option<&ir::With> {
    match stmt {
        Statement::Query(q) => q.with.as_ref().filter(|w| w.ctes.iter().any(|c| matches!(c.query.body, ir::SetExpr::Dml(_)))),
        Statement::Insert(i) => i.with.as_ref(),
        Statement::Update(u) => u.with.as_ref(),
        Statement::Delete(d) => d.with.as_ref(),
        Statement::Merge(m) => m.with.as_ref(),
        _ => None,
    }
}

fn without_with(stmt: &Statement) -> Statement {
    let mut s = stmt.clone();
    match &mut s {
        Statement::Query(q) => q.with = None,
        Statement::Insert(i) => i.with = None,
        Statement::Update(u) => u.with = None,
        Statement::Delete(d) => d.with = None,
        Statement::Merge(m) => m.with = None,
        _ => {}
    }
    s
}

/// Runs the WITH queries of `stmt` (see [`leading_with`]), then its body.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_with(
    stmt: &Statement,
    with: &ir::With,
    binder: &Binder,
    ctx: &Ctx,
    txn: &mut Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    txn.freeze_reads();
    let result = run_with(stmt, with, binder, ctx, txn, params, env, state);
    txn.thaw_reads();
    result
}

#[allow(clippy::too_many_arguments)]
fn run_with(
    stmt: &Statement,
    with: &ir::With,
    binder: &Binder,
    ctx: &Ctx,
    txn: &mut Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let mut ctx = Ctx { levels: ctx.levels.clone(), ctes: ctx.ctes.clone(), clause: ctx.clause, windows: Vec::new() };
    for (i, cte) in with.ctes.iter().enumerate() {
        if with.ctes[..i].iter().any(|c| c.name == cte.name) {
            return Err(SqlError::new(code::DUPLICATE_ALIAS, format!("WITH query name \"{}\" specified more than once", cte.name)));
        }
        let binding = match &cte.query.body {
            ir::SetExpr::Dml(inner) => {
                if leading_with(inner).is_some() {
                    return Err(bind::dml_not_top_level());
                }
                let id = binder.fresh_id();
                let r = exec_statement(inner, binder, &ctx, txn, params, env, state)?;
                let mut columns: Vec<_> = r.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
                if r.returns_rows {
                    bind::rename_columns(&mut columns, &cte.columns, &cte.name)?;
                }
                state.ctes.borrow_mut().insert(id, Rc::new(r.rows));
                CteBinding { name: cte.name.clone(), id, columns, no_returning: !r.returns_rows }
            }
            _ => {
                let (plan, binding) = binder.bind_cte(cte, with.recursive, &ctx)?;
                let rows = crate::run::materialize_cte(&plan, &EvalCtx::new(txn, params, env, state))?;
                state.ctes.borrow_mut().insert(plan.id, Rc::new(rows));
                binding
            }
        };
        ctx.ctes.push(binding);
    }
    exec_statement(&without_with(stmt), binder, &ctx, txn, params, env, state)
}

/// The result columns of a statement with a leading WITH, binding without running it.
pub(crate) fn describe_with(stmt: &Statement, binder: &Binder, ctx: &Ctx) -> SqlResult<Option<Vec<crate::plan::OutCol>>> {
    let Some(with) = leading_with(stmt) else { return Ok(None) };
    let mut ctx = Ctx { levels: ctx.levels.clone(), ctes: ctx.ctes.clone(), clause: ctx.clause, windows: Vec::new() };
    for cte in &with.ctes {
        let binding = match &cte.query.body {
            ir::SetExpr::Dml(inner) => {
                let cols = match inner.as_ref() {
                    _ if leading_with(inner).is_some() => return Err(bind::dml_not_top_level()),
                    Statement::Insert(i) => bind::bind_insert(binder, i, &ctx)?.returning_cols,
                    Statement::Update(u) => bind::bind_update(binder, u, &ctx)?.returning_cols,
                    Statement::Delete(d) => bind::bind_delete(binder, d, &ctx)?.returning_cols,
                    Statement::Merge(m) => bind::merge::bind_merge(binder, m, &ctx)?.returning_cols,
                    _ => Vec::new(),
                };
                let mut columns: Vec<_> = cols.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
                if !columns.is_empty() {
                    bind::rename_columns(&mut columns, &cte.columns, &cte.name)?;
                }
                CteBinding { name: cte.name.clone(), id: binder.fresh_id(), no_returning: columns.is_empty(), columns }
            }
            _ => binder.bind_cte(cte, with.recursive, &ctx)?.1,
        };
        ctx.ctes.push(binding);
    }
    let cols = match without_with(stmt) {
        Statement::Query(q) => binder.bind_query(&q, &ctx)?.columns,
        Statement::Insert(i) => bind::bind_insert(binder, &i, &ctx)?.returning_cols,
        Statement::Update(u) => bind::bind_update(binder, &u, &ctx)?.returning_cols,
        Statement::Delete(d) => bind::bind_delete(binder, &d, &ctx)?.returning_cols,
        Statement::Merge(m) => bind::merge::bind_merge(binder, &m, &ctx)?.returning_cols,
        _ => Vec::new(),
    };
    Ok(match stmt {
        Statement::Query(_) => Some(cols),
        _ if cols.is_empty() => None,
        _ => Some(cols),
    })
}

/// Binds and runs a query or DML statement (anything else goes to DDL execution).
pub(crate) fn exec_statement(
    stmt: &Statement,
    binder: &Binder,
    ctx: &Ctx,
    txn: &mut Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    if let Some(with) = leading_with(stmt) {
        return exec_with(stmt, with, binder, ctx, txn, params, env, state);
    }
    if let Some(r) = crate::triggers::instead_of(stmt, txn, params, env, state)? {
        return Ok(r);
    }
    Ok(match stmt {
        Statement::Query(q) => {
            let bound = binder.bind_query(q, ctx)?;
            let ectx = EvalCtx::new(txn, params, env, state);
            let rows = run(&bound.rel, &ectx)?;
            QueryResult { tag: format!("SELECT {}", rows.len()), columns: bound.columns, returns_rows: true, rows, ..Default::default() }
        }
        Statement::Insert(i) if let Some(root) = crate::partition::partitioned_target(txn.catalog(), &i.table) => {
            crate::partition::exec_insert(i, &root, binder, ctx, txn, params, env, state)?
        }
        Statement::Update(u) if let Some(root) = crate::partition::partitioned_target(txn.catalog(), &u.table) => {
            crate::partition::exec_update(u, &root, binder, ctx, txn, params, env, state)?
        }
        Statement::Delete(d) if let Some(root) = crate::partition::partitioned_target(txn.catalog(), &d.table) => {
            crate::partition::exec_delete(d, &root, binder, ctx, txn, params, env, state)?
        }
        Statement::Insert(i) => {
            let plan = bind::bind_insert(binder, i, ctx)?;
            let r = crate::dml::exec_insert(&plan, txn, params, env, state)?;
            dml_result(format!("INSERT 0 {}", r.count), plan.returning_cols, r)
        }
        Statement::Update(u) => {
            let plan = bind::bind_update(binder, u, ctx)?;
            let r = crate::dml::exec_update(&plan, txn, params, env, state)?;
            dml_result(format!("UPDATE {}", r.count), plan.returning_cols, r)
        }
        Statement::Delete(d) => {
            let plan = bind::bind_delete(binder, d, ctx)?;
            let r = crate::dml::exec_delete(&plan, txn, params, env, state)?;
            dml_result(format!("DELETE {}", r.count), plan.returning_cols, r)
        }
        Statement::Merge(m) => {
            let plan = bind::merge::bind_merge(binder, m, ctx)?;
            let r = crate::dml::exec_merge(&plan, txn, params, env, state)?;
            dml_result(format!("MERGE {}", r.count), plan.returning_cols, r)
        }
        other => crate::ddl::execute_ddl(other, txn, binder, env, state)?,
    })
}

fn dml_result(tag: String, cols: Vec<crate::plan::OutCol>, r: crate::dml::DmlResult) -> QueryResult {
    let returns_rows = !cols.is_empty();
    QueryResult { tag, columns: cols, returns_rows, rows: r.rows, ..Default::default() }
}
