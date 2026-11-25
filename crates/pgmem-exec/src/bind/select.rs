//! Query binding: WITH, SELECT, VALUES, set operations, FROM, ORDER BY, LIMIT.

use super::expr::{common_of, contains_agg, contains_window, output_name};
use super::{Binder, Clause, CteBinding, Ctx, Level, Scope, ScopeCol, feature};
use crate::cast::CoercionContext;
use crate::plan::{AggCall, CtePlan, Expr, JoinKind, OutCol, Rel, SetOpKind, SortKey, WinCall};
use pgmem_catalog::Relation;
use pgmem_ir as ir;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};

/// A bound query: its plan and result columns.
#[derive(Clone, Debug)]
pub struct BoundQuery {
    pub rel: Rel,
    pub columns: Vec<OutCol>,
}

impl Binder<'_> {
    pub fn bind_query(&self, q: &ir::Query, ctx: &Ctx) -> SqlResult<BoundQuery> {
        let mut ctx = Ctx { levels: ctx.levels.clone(), ctes: ctx.ctes.clone(), clause: Clause::Other, windows: Vec::new() };
        let mut cte_plans = Vec::new();
        if let Some(with) = &q.with {
            for (i, cte) in with.ctes.iter().enumerate() {
                if with.ctes[..i].iter().any(|c| c.name == cte.name) {
                    return Err(SqlError::new(code::DUPLICATE_ALIAS, format!("WITH query name \"{}\" specified more than once", cte.name)));
                }
                let plan = self.bind_cte(cte, with.recursive, &ctx)?;
                cte_plans.push(plan.0);
                ctx.ctes.push(plan.1);
            }
        }
        if let Some(l) = q.locking.first()
            && !matches!(q.body, ir::SetExpr::Select(_))
        {
            return Err(lock_misuse(l, "UNION/INTERSECT/EXCEPT"));
        }
        let mut lock_targets = 0;
        let mut bound = match &q.body {
            ir::SetExpr::Select(s) => {
                let (b, n) = self.bind_select_locking(s, &q.order_by, &q.locking, &ctx)?;
                lock_targets = n;
                b
            }
            body => {
                let mut b = self.bind_set_expr(body, &ctx)?;
                if !q.order_by.is_empty() {
                    b = self.order_by_outputs(b, &q.order_by, matches!(body, ir::SetExpr::SetOp { .. }), &ctx)?;
                }
                b
            }
        };
        if q.limit.is_some() || q.offset.is_some() {
            let lim_ctx = ctx.with_clause(Clause::Other);
            let as_int8 = |e: &ir::Expr, what: &str| -> SqlResult<Expr> {
                let (x, t) = self.bind_expr(e, &lim_ctx)?;
                if contains_column(&x) {
                    return Err(SqlError::new(code::INVALID_COLUMN_REFERENCE, format!("argument of {what} must not contain variables")));
                }
                super::coerce(self, x, &t, &Type::Int8, CoercionContext::Assignment)
            };
            let limit = q.limit.as_ref().map(|l| as_int8(l, "LIMIT")).transpose()?;
            let offset = q.offset.as_ref().map(|o| as_int8(o, "OFFSET")).transpose()?;
            if lock_targets > 0 {
                bound.rel = lock_rows(bound.rel, &q.locking, lock_targets, bound.columns.len(), limit, offset);
                lock_targets = 0;
            } else {
                bound.rel = if q.with_ties {
                    limit_with_ties(bound.rel, limit, offset)
                } else {
                    Rel::Limit { input: Box::new(bound.rel), limit, offset, ties: None }
                };
            }
        }
        if lock_targets > 0 {
            bound.rel = lock_rows(bound.rel, &q.locking, lock_targets, bound.columns.len(), None, None);
        }
        if !cte_plans.is_empty() {
            bound.rel = Rel::With { ctes: cte_plans, body: Box::new(bound.rel) };
        }
        Ok(bound)
    }

    pub(crate) fn bind_cte(&self, cte: &ir::Cte, recursive: bool, ctx: &Ctx) -> SqlResult<(CtePlan, CteBinding)> {
        let id = self.fresh_id();
        let self_ref = recursive && query_references(&cte.query, &cte.name);
        if self_ref {
            // WITH RECURSIVE name AS (non_recursive UNION [ALL] recursive)
            let ir::SetExpr::SetOp { op: ir::SetOperator::Union, all, left, right } = &cte.query.body else {
                return Err(SqlError::new(
                    code::INVALID_RECURSION,
                    format!("recursive query \"{}\" does not have the form non-recursive-term UNION [ALL] recursive-term", cte.name),
                ));
            };
            if in_set(left, &cte.name) {
                return Err(SqlError::new(
                    code::INVALID_RECURSION,
                    format!("recursive reference to query \"{}\" must not appear within its non-recursive term", cte.name),
                ));
            }
            let base_level = Level::new(Scope::default());
            let base = self.bind_set_expr(left, &ctx.from_item(&[&base_level]))?;
            let mut columns: Vec<(String, Type)> = base.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
            rename_columns(&mut columns, &cte.columns, &cte.name)?;
            let step_level = Level::new(Scope::default());
            let mut inner_ctx = ctx.from_item(&[&step_level]);
            let binding = CteBinding { name: cte.name.clone(), id, columns: columns.clone(), no_returning: false };
            inner_ctx.ctes.push(binding.clone());
            let step = self.bind_set_expr(right, &inner_ctx)?;
            if has_aggregate(&step.rel) {
                return Err(SqlError::new(
                    code::INVALID_RECURSION,
                    "aggregate functions are not allowed in a recursive query's recursive term",
                ));
            }
            if step.columns.len() != columns.len() {
                return Err(SqlError::syntax("each UNION query must have the same number of columns"));
            }
            for (n, (c, s)) in columns.iter().zip(&step.columns).enumerate() {
                let common = common_of(&[c.1.clone(), s.ty.clone()], "UNION")?;
                if common != c.1 {
                    return Err(SqlError::datatype_mismatch(format!(
                        "recursive query \"{}\" column {} has type {} in non-recursive term but type {} overall",
                        cte.name,
                        n + 1,
                        c.1.name(),
                        common.name()
                    ))
                    .with_hint("Cast the output of the non-recursive term to the correct type."));
                }
            }
            let step_rel = coerce_columns(
                self,
                step.rel,
                &step.columns.iter().map(|c| c.ty.clone()).collect::<Vec<_>>(),
                &columns.iter().map(|c| c.1.clone()).collect::<Vec<_>>(),
            )?;
            return Ok((CtePlan { id, plan: base.rel, recursive: Some((step_rel, *all)) }, binding));
        }
        if let ir::SetExpr::Dml(_) = cte.query.body {
            return Err(dml_not_top_level());
        }
        let level = Level::new(Scope::default());
        let bound = self.bind_query(&cte.query, &ctx.from_item(&[&level]))?;
        let mut columns: Vec<(String, Type)> = bound.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
        rename_columns(&mut columns, &cte.columns, &cte.name)?;
        Ok((CtePlan { id, plan: bound.rel, recursive: None }, CteBinding { name: cte.name.clone(), id, columns, no_returning: false }))
    }

    /// Binds a set-expression operand (SELECT, VALUES, nested set op, parenthesized query).
    pub fn bind_set_expr(&self, body: &ir::SetExpr, ctx: &Ctx) -> SqlResult<BoundQuery> {
        match body {
            ir::SetExpr::Select(s) => self.bind_select(s, &[], ctx),
            ir::SetExpr::Query(q) => self.bind_query(q, ctx),
            ir::SetExpr::Dml(_) => Err(dml_not_top_level()),
            ir::SetExpr::Values(rows) => self.bind_values(rows, ctx),
            ir::SetExpr::SetOp { op, all, left, right } => {
                let octx = ctx.with_clause(Clause::SetOperand);
                let l = self.bind_set_expr(left, &octx)?;
                let r = self.bind_set_expr(right, &octx)?;
                let name = match op {
                    ir::SetOperator::Union => "UNION",
                    ir::SetOperator::Intersect => "INTERSECT",
                    ir::SetOperator::Except => "EXCEPT",
                };
                if l.columns.len() != r.columns.len() {
                    return Err(SqlError::syntax(format!("each {name} query must have the same number of columns")));
                }
                let mut types = Vec::new();
                for (a, b) in l.columns.iter().zip(&r.columns) {
                    types.push(common_of(&[a.ty.clone(), b.ty.clone()], name)?);
                }
                let lrel = coerce_columns(self, l.rel, &l.columns.iter().map(|c| c.ty.clone()).collect::<Vec<_>>(), &types)?;
                let rrel = coerce_columns(self, r.rel, &r.columns.iter().map(|c| c.ty.clone()).collect::<Vec<_>>(), &types)?;
                let columns = l.columns.iter().zip(&types).map(|(c, t)| OutCol::new(c.name.clone(), t.clone())).collect();
                let kind = match op {
                    ir::SetOperator::Union => SetOpKind::Union,
                    ir::SetOperator::Intersect => SetOpKind::Intersect,
                    ir::SetOperator::Except => SetOpKind::Except,
                };
                Ok(BoundQuery { rel: Rel::SetOp { op: kind, all: *all, left: Box::new(lrel), right: Box::new(rrel) }, columns })
            }
        }
    }

    fn bind_values(&self, rows: &[Vec<ir::Expr>], ctx: &Ctx) -> SqlResult<BoundQuery> {
        let width = rows.first().map_or(0, Vec::len);
        let vctx = ctx.with_clause(Clause::Values);
        let mut bound_rows = Vec::with_capacity(rows.len());
        for row in rows {
            if row.len() != width {
                return Err(SqlError::syntax("VALUES lists must all be the same length"));
            }
            bound_rows.push(row.iter().map(|e| self.bind_expr(e, &vctx)).collect::<SqlResult<Vec<_>>>()?);
        }
        let mut types = Vec::with_capacity(width);
        for col in 0..width {
            let col_types: Vec<Type> = bound_rows.iter().map(|r| r[col].1.clone()).collect();
            types.push(common_of(&col_types, "VALUES")?);
        }
        let out_rows = bound_rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .zip(&types)
                    .map(|((e, et), t)| super::coerce(self, e, &et, t, CoercionContext::Implicit))
                    .collect::<SqlResult<Vec<_>>>()
            })
            .collect::<SqlResult<Vec<_>>>()?;
        let columns = types.iter().enumerate().map(|(i, t)| OutCol::new(format!("column{}", i + 1), t.clone())).collect();
        Ok(BoundQuery { rel: Rel::Values { rows: out_rows }, columns })
    }

    /// ORDER BY over a set operation / VALUES: only output columns (by name or position).
    fn order_by_outputs(&self, b: BoundQuery, order_by: &[ir::OrderByExpr], set_op: bool, ctx: &Ctx) -> SqlResult<BoundQuery> {
        let scope = Scope::from_cols(b.columns.iter().map(|c| ScopeCol::new(None, c.name.clone(), c.ty.clone())).collect());
        let level = Level::sharing(scope, ctx.current());
        let octx = ctx.nested_same_level(&level).with_clause(Clause::OrderBy);
        let mut keys = Vec::new();
        for o in order_by {
            let expr = match ordinal(&o.expr) {
                Some(n) if n >= 1 && n <= b.columns.len() => Expr::Col(n - 1),
                Some(n) => {
                    return Err(SqlError::new(code::INVALID_COLUMN_REFERENCE, format!("ORDER BY position {n} is not in select list")));
                }
                None if set_op && !matches!(&o.expr, ir::Expr::Column(p) if p.len() == 1) => {
                    return Err(SqlError::new(code::FEATURE_NOT_SUPPORTED, "invalid UNION/INTERSECT/EXCEPT ORDER BY clause")
                        .with_detail("Only result column names can be used, not expressions or functions.")
                        .with_hint("Add the expression/function to every SELECT, or move the UNION into a FROM clause."));
                }
                None => self.bind_expr(&o.expr, &octx)?.0,
            };
            keys.push(SortKey { expr, desc: o.desc, nulls_first: o.nulls_first.unwrap_or(o.desc) });
        }
        Ok(BoundQuery { rel: Rel::Sort { input: Box::new(b.rel), keys }, columns: b.columns })
    }

    fn bind_select(&self, s: &ir::Select, order_by: &[ir::OrderByExpr], ctx: &Ctx) -> SqlResult<BoundQuery> {
        Ok(self.bind_select_locking(s, order_by, &[], ctx)?.0)
    }

    /// A SELECT, with its FOR UPDATE / SHARE clauses: the rows of each locked table's
    /// `(tableoid, ctid)` follow the visible columns (their number is returned).
    fn bind_select_locking(
        &self,
        s: &ir::Select,
        order_by: &[ir::OrderByExpr],
        locking: &[ir::LockClause],
        ctx: &Ctx,
    ) -> SqlResult<(BoundQuery, usize)> {
        // FROM
        let (mut rel, scope) = self.bind_from(&s.from, ctx)?;
        let lock_junk = lock_targets(&scope, locking)?;
        if let Some(l) = locking.first() {
            let what = if !matches!(s.distinct, ir::Distinct::All) {
                Some("DISTINCT clause")
            } else if !s.group_by.is_empty() {
                Some("GROUP BY clause")
            } else if s.having.is_some() {
                Some("HAVING clause")
            } else {
                None
            };
            if let Some(what) = what {
                return Err(lock_misuse(l, what));
            }
        }
        let level = Level::sharing(scope, ctx.current());
        let mut sctx = ctx.nested_same_level(&level);
        sctx.windows = s.windows.clone();
        // WHERE
        if let Some(w) = &s.selection {
            let pred = self.bind_predicate(w, "WHERE", &sctx.with_clause(Clause::Where))?;
            rel = Rel::Filter { input: Box::new(rel), pred };
        }
        // Select list.
        let list_ctx = sctx.with_clause(Clause::SelectList);
        let mut exprs = Vec::new();
        let mut columns: Vec<OutCol> = Vec::new();
        for item in &s.projection {
            match item {
                ir::SelectItem::Wildcard => {
                    if level.scope.star.is_empty() && s.from.is_empty() {
                        return Err(SqlError::syntax("SELECT * with no tables specified is not valid"));
                    }
                    for &i in &level.scope.star {
                        let c = &level.scope.cols[i];
                        exprs.push(Expr::Col(i));
                        columns.push(OutCol {
                            name: c.name.clone(),
                            ty: c.ty.clone(),
                            table_oid: c.table_oid,
                            attnum: c.attnum,
                            typmod: c.typmod,
                        });
                    }
                }
                ir::SelectItem::QualifiedWildcard(q) => {
                    let qual = q.base();
                    if !level.scope.has_qualifier(qual) {
                        return Err(SqlError::new(code::UNDEFINED_TABLE, format!("missing FROM-clause entry for table \"{qual}\"")));
                    }
                    for i in level.scope.qualified_star(qual) {
                        let c = &level.scope.cols[i];
                        exprs.push(Expr::Col(i));
                        columns.push(OutCol {
                            name: c.name.clone(),
                            ty: c.ty.clone(),
                            table_oid: c.table_oid,
                            attnum: c.attnum,
                            typmod: c.typmod,
                        });
                    }
                }
                ir::SelectItem::FieldStar(expr) => {
                    let (e, t) = self.bind_expr(expr, &list_ctx)?;
                    for (fe, name, ty) in super::expr::expand_fields(e, &t)? {
                        exprs.push(fe);
                        columns.push(OutCol::new(name, ty));
                    }
                }
                ir::SelectItem::Expr { expr, alias } => {
                    let (e, t) = self.bind_expr(expr, &list_ctx)?;
                    // An untyped literal in the select list becomes text, as in Postgres (a set
                    // operation operand leaves it to the set operation).
                    let (e, t) = if t == Type::Unknown && ctx.clause != Clause::SetOperand {
                        (super::coerce(self, e, &t, &Type::Text, CoercionContext::Implicit)?, Type::Text)
                    } else {
                        (e, t)
                    };
                    let mut col = OutCol::new(alias.clone().unwrap_or_else(|| output_name(expr)), t);
                    if let Expr::Col(i) = &e {
                        let c = &level.scope.cols[*i];
                        col.table_oid = c.table_oid;
                        col.attnum = c.attnum;
                        col.typmod = c.typmod;
                    }
                    exprs.push(e);
                    columns.push(col);
                }
            }
        }
        // GROUP BY
        let gctx = sctx.with_clause(Clause::GroupBy);
        let mut group = Vec::new();
        for g in &s.group_by {
            let e = match ordinal(g) {
                Some(n) if n >= 1 && n <= exprs.len() => exprs[n - 1].clone(),
                Some(n) => {
                    return Err(SqlError::new(code::INVALID_COLUMN_REFERENCE, format!("GROUP BY position {n} is not in select list")));
                }
                None => match self.bind_expr(g, &gctx) {
                    Ok((e, _)) => e,
                    // A bare name that isn't an input column may name an output column.
                    Err(err) if err.code == code::UNDEFINED_COLUMN => {
                        match bare_name(g).and_then(|n| columns.iter().position(|c| c.name == n)) {
                            Some(i) => exprs[i].clone(),
                            None => return Err(err),
                        }
                    }
                    Err(err) => return Err(err),
                },
            };
            if contains_agg(&e) {
                return Err(SqlError::new(code::GROUPING_ERROR, "aggregate functions are not allowed in GROUP BY"));
            }
            if contains_window(&e) {
                return Err(SqlError::new(code::WINDOWING_ERROR, "window functions are not allowed in GROUP BY"));
            }
            group.push(e);
        }
        // HAVING
        let having = s.having.as_ref().map(|h| self.bind_predicate(h, "HAVING", &sctx.with_clause(Clause::Having))).transpose()?;
        // ORDER BY (bound against the input scope, or naming output columns).
        let octx = sctx.with_clause(Clause::OrderBy);
        let mut order_exprs: Vec<(Expr, bool, bool)> = Vec::new();
        for o in order_by {
            let e = match ordinal(&o.expr) {
                Some(n) if n >= 1 && n <= exprs.len() => OrderTarget::Output(n - 1),
                Some(n) => {
                    return Err(SqlError::new(code::INVALID_COLUMN_REFERENCE, format!("ORDER BY position {n} is not in select list")));
                }
                None => match bare_name(&o.expr)
                    .map(|n| columns.iter().enumerate().filter(|(_, c)| c.name == n).map(|(i, _)| i).collect::<Vec<_>>())
                {
                    Some(matches) if matches.len() == 1 => OrderTarget::Output(matches[0]),
                    Some(matches) if matches.len() > 1 && !matches.iter().all(|&m| exprs[m] == exprs[matches[0]]) => {
                        return Err(SqlError::new(
                            code::AMBIGUOUS_COLUMN,
                            format!("ORDER BY \"{}\" is ambiguous", bare_name(&o.expr).unwrap_or_default()),
                        ));
                    }
                    Some(matches) if !matches.is_empty() => OrderTarget::Output(matches[0]),
                    _ => OrderTarget::Expr(self.bind_expr(&o.expr, &octx)?.0),
                },
            };
            let e = match e {
                OrderTarget::Output(i) => exprs[i].clone(),
                OrderTarget::Expr(e) => e,
            };
            order_exprs.push((e, o.desc, o.nulls_first.unwrap_or(o.desc)));
        }
        // DISTINCT ON expressions.
        let distinct_on: Option<Vec<Expr>> = match &s.distinct {
            ir::Distinct::On(items) => Some(
                items
                    .iter()
                    .map(|i| match ordinal(i) {
                        Some(n) if n >= 1 && n <= exprs.len() => Ok(exprs[n - 1].clone()),
                        _ => match bare_name(i).and_then(|n| columns.iter().position(|c| c.name == n)) {
                            Some(p) => Ok(exprs[p].clone()),
                            None => Ok(self.bind_expr(i, &octx)?.0),
                        },
                    })
                    .collect::<SqlResult<Vec<_>>>()?,
            ),
            _ => None,
        };

        // Aggregation.
        let mut all_aggs: Vec<AggCall> = Vec::new();
        let mut collect = |e: &Expr| collect_aggs(e, &mut all_aggs);
        exprs.iter().for_each(&mut collect);
        order_exprs.iter().for_each(|(e, _, _)| collect(e));
        if let Some(h) = &having {
            collect(h);
        }
        if let Some(d) = &distinct_on {
            d.iter().for_each(&mut collect);
        }
        let grouped = !group.is_empty() || !all_aggs.is_empty() || having.is_some();
        if grouped && let Some(l) = locking.first() {
            return Err(lock_misuse(l, "aggregate functions"));
        }
        if grouped {
            let sets = s.grouping_sets.clone();
            let pk_groups = if sets.is_some() { Vec::new() } else { functional_dependencies(&group, &level.scope, self.catalog) };
            let mut grouping = Grouping { group: group.clone(), aggs: all_aggs, scope: &level.scope, pk_tables: pk_groups, sets };
            exprs = exprs.into_iter().map(|e| grouping.rewrite(e)).collect::<SqlResult<Vec<_>>>()?;
            order_exprs = order_exprs.into_iter().map(|(e, d, n)| Ok((grouping.rewrite(e)?, d, n))).collect::<SqlResult<Vec<_>>>()?;
            let having = having.map(|h| grouping.rewrite(h)).transpose()?;
            let distinct_rw =
                distinct_on.clone().map(|d| d.into_iter().map(|e| grouping.rewrite(e)).collect::<SqlResult<Vec<_>>>()).transpose()?;
            rel = Rel::Aggregate {
                input: Box::new(rel),
                group: grouping.group.clone(),
                aggs: grouping.aggs.clone(),
                sets: grouping.sets.clone(),
            };
            // GROUPING() reads the set index, which follows the aggregates.
            let set_col = grouping.group.len() + grouping.aggs.len();
            let fix = |e: Expr| {
                e.transform(&mut |x| {
                    Ok(match x {
                        Expr::Grouping { col: usize::MAX, values, args } => Expr::Grouping { args, col: set_col, values },
                        other => other,
                    })
                })
            };
            exprs = exprs.into_iter().map(&fix).collect::<SqlResult<Vec<_>>>()?;
            order_exprs = order_exprs.into_iter().map(|(e, d, n)| Ok((fix(e)?, d, n))).collect::<SqlResult<Vec<_>>>()?;
            let having = having.map(&fix).transpose()?;
            let distinct_rw = distinct_rw.map(|d| d.into_iter().map(&fix).collect::<SqlResult<Vec<_>>>()).transpose()?;
            if let Some(h) = having {
                rel = Rel::Filter { input: Box::new(rel), pred: h };
            }
            return Ok((
                self.finish_select(
                    rel,
                    exprs,
                    columns,
                    order_exprs,
                    distinct_rw,
                    &s.distinct,
                    set_col + usize::from(grouping.sets.is_some()),
                    Vec::new(),
                )?,
                0,
            ));
        }
        let width = level.scope.width();
        let targets = lock_junk.len() / 2;
        Ok((self.finish_select(rel, exprs, columns, order_exprs, distinct_on, &s.distinct, width, lock_junk)?, targets))
    }

    /// Window functions, projection, ORDER BY, DISTINCT and junk-column removal.
    #[allow(clippy::too_many_arguments)]
    fn finish_select(
        &self,
        mut rel: Rel,
        mut exprs: Vec<Expr>,
        columns: Vec<OutCol>,
        mut order_exprs: Vec<(Expr, bool, bool)>,
        mut distinct_on: Option<Vec<Expr>>,
        distinct: &ir::Distinct,
        input_width: usize,
        lock_junk: Vec<Expr>,
    ) -> SqlResult<BoundQuery> {
        if !lock_junk.is_empty() && exprs.iter().chain(order_exprs.iter().map(|(e, _, _)| e)).any(|e| matches!(e, Expr::WindowCall(_))) {
            return Err(SqlError::feature_not_supported("FOR UPDATE is not allowed with window functions"));
        }
        // Window functions over the (possibly aggregated) input.
        let mut wins: Vec<WinCall> = Vec::new();
        exprs.iter().for_each(|e| collect_windows(e, &mut wins));
        order_exprs.iter().for_each(|(e, _, _)| collect_windows(e, &mut wins));
        if !wins.is_empty() {
            let replace = |e: Expr| -> SqlResult<Expr> {
                e.transform(&mut |x| {
                    Ok(match x {
                        Expr::WindowCall(w) => Expr::Col(input_width + wins.iter().position(|c| *c == *w).unwrap_or(0)),
                        other => other,
                    })
                })
            };
            exprs = exprs.into_iter().map(&replace).collect::<SqlResult<Vec<_>>>()?;
            order_exprs = order_exprs.into_iter().map(|(e, d, n)| Ok((replace(e)?, d, n))).collect::<SqlResult<Vec<_>>>()?;
            distinct_on = distinct_on.map(|d| d.into_iter().map(&replace).collect::<SqlResult<Vec<_>>>()).transpose()?;
            rel = Rel::Window { input: Box::new(rel), calls: wins.clone() };
        }
        // Set-returning functions in the select list, innermost first: each level is a
        // ProjectSet appending one column per function.
        let mut width = input_width + wins.len();
        loop {
            let mut calls: Vec<Expr> = Vec::new();
            for e in exprs.iter().chain(order_exprs.iter().map(|(e, _, _)| e)) {
                collect_innermost_srfs(e, &mut calls);
            }
            if calls.is_empty() {
                break;
            }
            let base = width;
            let replace = |e: Expr| -> SqlResult<Expr> {
                e.transform(&mut |x| {
                    Ok(match x {
                        call @ Expr::SrfCall { .. } => match calls.iter().position(|c| *c == call) {
                            Some(i) => Expr::Col(base + i),
                            None => call,
                        },
                        other => other,
                    })
                })
            };
            exprs = exprs.into_iter().map(&replace).collect::<SqlResult<Vec<_>>>()?;
            order_exprs = order_exprs.into_iter().map(|(e, d, n)| Ok((replace(e)?, d, n))).collect::<SqlResult<Vec<_>>>()?;
            distinct_on = distinct_on.map(|d| d.into_iter().map(&replace).collect::<SqlResult<Vec<_>>>()).transpose()?;
            width += calls.len();
            let srfs = calls
                .into_iter()
                .filter_map(|c| match c {
                    Expr::SrfCall { srf, args, cols } => Some((srf, args, cols)),
                    _ => None,
                })
                .collect();
            rel = Rel::ProjectSet { input: Box::new(rel), srfs };
        }
        let visible = exprs.len();
        let is_distinct = !matches!(distinct, ir::Distinct::All);
        // Sort keys refer to projected columns; expressions not in the list become junk.
        let mut keys = Vec::new();
        for (e, desc, nulls_first) in order_exprs {
            let pos = match exprs.iter().position(|x| *x == e) {
                Some(p) => p,
                None => {
                    if is_distinct && distinct_on.is_none() {
                        return Err(SqlError::new(
                            code::INVALID_COLUMN_REFERENCE,
                            "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                        ));
                    }
                    exprs.push(e);
                    exprs.len() - 1
                }
            };
            keys.push(SortKey { expr: Expr::Col(pos), desc, nulls_first });
        }
        let distinct_cols = distinct_on.map(|items| {
            items
                .into_iter()
                .map(|e| {
                    let pos = match exprs.iter().position(|x| *x == e) {
                        Some(p) => p,
                        None => {
                            exprs.push(e);
                            exprs.len() - 1
                        }
                    };
                    Expr::Col(pos)
                })
                .collect::<Vec<_>>()
        });
        if let Some(on) = &distinct_cols {
            // DISTINCT ON keys must lead the ORDER BY.
            for (k, d) in keys.iter().zip(on) {
                if k.expr != *d && on.iter().all(|o| *o != k.expr) {
                    return Err(SqlError::new(
                        code::INVALID_COLUMN_REFERENCE,
                        "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
                    ));
                }
            }
        }
        // Locked tables' row identities stay after the visible columns (for LockRows).
        let lock_cols: Vec<usize> = lock_junk
            .into_iter()
            .map(|e| {
                exprs.push(e);
                exprs.len() - 1
            })
            .collect();
        rel = Rel::Project { input: Box::new(rel), exprs: exprs.clone() };
        if !keys.is_empty() {
            rel = Rel::Sort { input: Box::new(rel), keys };
        }
        match (distinct, distinct_cols) {
            (_, Some(on)) => rel = Rel::Distinct { input: Box::new(rel), on: Some(on) },
            (ir::Distinct::Distinct, None) => {
                rel = if exprs.len() > visible {
                    Rel::Distinct { input: Box::new(rel), on: Some((0..visible).map(Expr::Col).collect()) }
                } else {
                    Rel::Distinct { input: Box::new(rel), on: None }
                };
            }
            _ => {}
        }
        if exprs.len() > visible || !lock_cols.is_empty() {
            let keep = (0..visible).chain(lock_cols.iter().copied()).map(Expr::Col).collect();
            rel = Rel::Project { input: Box::new(rel), exprs: keep };
        }
        Ok(BoundQuery { rel, columns })
    }

    /// Binds a FROM list into a plan (cross joins between items) and its scope.
    pub fn bind_from(&self, items: &[ir::TableRef], ctx: &Ctx) -> SqlResult<(Rel, Scope)> {
        if items.is_empty() {
            return Ok((Rel::Values { rows: vec![vec![]] }, Scope::default()));
        }
        let mut acc: Option<(Rel, Scope)> = None;
        for item in items {
            let left_scope = acc.as_ref().map(|(_, s)| s.clone()).unwrap_or_default();
            let (rel, scope, lateral) = self.bind_table_ref(item, ctx, &left_scope)?;
            acc = Some(match acc {
                None => (rel, scope),
                Some((lrel, lscope)) => {
                    check_duplicate_aliases(&lscope, &scope)?;
                    let (lw, rw) = (lscope.width(), scope.width());
                    let joined = join_scopes(lscope, scope, &[], JoinKind::Inner);
                    (
                        Rel::Join {
                            kind: JoinKind::Inner,
                            left: Box::new(lrel),
                            right: Box::new(rel),
                            on: None,
                            lateral,
                            left_width: lw,
                            right_width: rw,
                        },
                        joined,
                    )
                }
            });
        }
        acc.ok_or_else(|| SqlError::internal("empty FROM"))
    }

    /// Binds one FROM item. `left` is the scope of items to its left (visible to LATERAL
    /// items as the immediately enclosing level). Returns whether the item is lateral.
    fn bind_table_ref(&self, item: &ir::TableRef, ctx: &Ctx, left: &Scope) -> SqlResult<(Rel, Scope, bool)> {
        match item {
            ir::TableRef::Table { name, alias } => {
                let (rel, scope) = self.bind_relation(name, alias.as_ref(), ctx)?;
                Ok((rel, scope, false))
            }
            ir::TableRef::Subquery { query, alias, lateral } => {
                let left_level = Level::new(left.clone());
                let level = Level::new(Scope::default());
                let lateral = *lateral && !left.cols.is_empty();
                let bound = if lateral {
                    self.bind_query(query, &ctx.from_item(&[&left_level, &level]))?
                } else {
                    self.bind_query(query, &ctx.from_item(&[&level])).map_err(|e| sibling_reference(e, left))?
                };
                let alias_name = alias.as_ref().map(|a| a.name.clone());
                let mut cols: Vec<ScopeCol> =
                    bound.columns.iter().map(|c| ScopeCol::new(alias_name.clone(), c.name.clone(), c.ty.clone())).collect();
                if let Some(a) = alias {
                    apply_column_aliases(&mut cols, &a.columns, &a.name)?;
                }
                // A lateral item that references anything outside itself is re-evaluated per
                // left row (which also keeps outer-reference depths consistent).
                let correlated = lateral && level.correlated.get();
                Ok((bound.rel, Scope::from_cols(cols), correlated))
            }
            ir::TableRef::Function { call, alias, with_ordinality, .. } => {
                self.bind_function_scan(call, alias.as_ref(), *with_ordinality, ctx, left)
            }
            ir::TableRef::Join { left: l, right: r, kind, constraint, using_alias, alias } => {
                let (rel, scope) = self.bind_join(l, r, *kind, constraint, using_alias.as_deref(), ctx, left)?;
                match alias {
                    None => Ok((rel, scope, false)),
                    // An aliased join exposes only its output columns, under the alias.
                    Some(a) => {
                        let exprs: Vec<Expr> = scope.star.iter().map(|&i| Expr::Col(i)).collect();
                        let mut cols: Vec<ScopeCol> = scope
                            .star
                            .iter()
                            .map(|&i| {
                                let c = &scope.cols[i];
                                let mut n = ScopeCol::new(Some(a.name.clone()), c.name.clone(), c.ty.clone());
                                n.typmod = c.typmod;
                                n.table_oid = c.table_oid;
                                n.attnum = c.attnum;
                                n
                            })
                            .collect();
                        apply_column_aliases(&mut cols, &a.columns, &a.name)?;
                        Ok((Rel::Project { input: Box::new(rel), exprs }, Scope::from_cols(cols), false))
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_join(
        &self,
        l: &ir::TableRef,
        r: &ir::TableRef,
        kind: ir::JoinKind,
        constraint: &ir::JoinConstraint,
        using_alias: Option<&str>,
        ctx: &Ctx,
        left: &Scope,
    ) -> SqlResult<(Rel, Scope)> {
        let (lrel, lscope, _) = self.bind_table_ref(l, ctx, left)?;
        let (rrel, rscope, lateral) = self.bind_table_ref(r, ctx, &lscope)?;
        check_duplicate_aliases(&lscope, &rscope)?;
        let kind = match kind {
            ir::JoinKind::Inner | ir::JoinKind::Cross => JoinKind::Inner,
            ir::JoinKind::Left => JoinKind::Left,
            ir::JoinKind::Right => JoinKind::Right,
            ir::JoinKind::Full => JoinKind::Full,
        };
        if lateral && matches!(kind, JoinKind::Right | JoinKind::Full) {
            return Err(SqlError::syntax("invalid reference to FROM-clause entry for table")
                .with_hint("The combining JOIN type must be INNER or LEFT for a LATERAL reference."));
        }
        let (lw, rw) = (lscope.width(), rscope.width());
        let using: Vec<String> = match constraint {
            ir::JoinConstraint::Using(cols) => cols.clone(),
            ir::JoinConstraint::Natural => {
                let lnames: Vec<&String> = lscope.star.iter().map(|i| &lscope.cols[*i].name).collect();
                rscope.star.iter().map(|i| rscope.cols[*i].name.clone()).filter(|n| lnames.contains(&n)).collect()
            }
            _ => Vec::new(),
        };
        // USING columns: (left index, right index, merged type).
        let mut pairs = Vec::new();
        for name in &using {
            let li = find_col(&lscope, name).ok_or_else(|| {
                SqlError::new(code::UNDEFINED_COLUMN, format!("column \"{name}\" specified in USING clause does not exist in left table"))
            })?;
            let ri = find_col(&rscope, name).ok_or_else(|| {
                SqlError::new(code::UNDEFINED_COLUMN, format!("column \"{name}\" specified in USING clause does not exist in right table"))
            })?;
            let (lt, rt) = (lscope.cols[li].ty.clone(), rscope.cols[ri].ty.clone());
            let common = super::expr::common_of(&[lt.clone(), rt.clone()], "JOIN/USING")?;
            pairs.push((li, ri, lt, rt, common));
        }
        let mut scope = join_scopes(lscope.clone(), rscope.clone(), &using, kind);
        let on = match constraint {
            ir::JoinConstraint::On(e) => {
                let level = Level::sharing(scope.clone(), ctx.current());
                let jctx = ctx.nested_same_level(&level).with_clause(Clause::JoinOn);
                Some(self.bind_predicate(e, "JOIN/ON", &jctx)?)
            }
            ir::JoinConstraint::Using(_) | ir::JoinConstraint::Natural => {
                let mut conds = Vec::new();
                for (li, ri, lt, rt, _) in &pairs {
                    conds.push(self.apply_binary("=", Expr::Col(*li), lt.clone(), Expr::Col(lw + ri), rt.clone())?.0);
                }
                if conds.is_empty() { None } else { Some(Expr::And(conds)) }
            }
            ir::JoinConstraint::None => {
                if matches!(kind, JoinKind::Left | JoinKind::Right | JoinKind::Full) {
                    return Err(SqlError::syntax("syntax error at or near \"WHERE\"").with_detail("OUTER JOIN requires a join condition"));
                }
                None
            }
        };
        let mut rel = Rel::Join { kind, left: Box::new(lrel), right: Box::new(rrel), on, lateral, left_width: lw, right_width: rw };
        // The merged USING column is its own expression when it is not just one side's column:
        // COALESCE(l, r) for FULL JOIN, a cast when the types differ, or a USING alias names it.
        let merge = !pairs.is_empty()
            && (kind == JoinKind::Full || using_alias.is_some() || pairs.iter().any(|(_, _, lt, rt, c)| lt != c || rt != c));
        if merge {
            let mut exprs: Vec<Expr> = (0..lw + rw).map(Expr::Col).collect();
            let mut star = Vec::new();
            for (n, (li, ri, lt, rt, common)) in pairs.iter().enumerate() {
                let lcol = super::coerce(self, Expr::Col(*li), lt, common, CoercionContext::Implicit)?;
                let rcol = super::coerce(self, Expr::Col(lw + ri), rt, common, CoercionContext::Implicit)?;
                exprs.push(match kind {
                    JoinKind::Full => Expr::Coalesce(vec![lcol, rcol]),
                    JoinKind::Right => rcol,
                    _ => lcol,
                });
                scope.cols[*li].hidden_unqualified = true;
                scope.cols[lw + ri].hidden_unqualified = true;
                let mut col = ScopeCol::new(using_alias.map(str::to_owned), using[n].clone(), common.clone());
                let src = if kind == JoinKind::Right { &scope.cols[lw + ri] } else { &scope.cols[*li] };
                if lt == rt {
                    col.typmod = src.typmod;
                }
                scope.cols.push(col);
                star.push(exprs.len() - 1);
            }
            let merged: Vec<usize> = pairs.iter().flat_map(|(li, ri, ..)| [*li, lw + ri]).collect();
            star.extend(scope.star.iter().copied().filter(|i| !merged.contains(i)));
            scope.star = star;
            rel = Rel::Project { input: Box::new(rel), exprs };
        }
        Ok((rel, scope))
    }

    fn bind_function_scan(
        &self,
        call: &ir::FunctionCall,
        alias: Option<&ir::TableAlias>,
        ordinality: bool,
        ctx: &Ctx,
        left: &Scope,
    ) -> SqlResult<(Rel, Scope, bool)> {
        let name = call.name.base();
        let left_level = Level::new(left.clone());
        let own_level = Level::new(Scope::default());
        let fctx = if left.cols.is_empty() { ctx.from_item(&[&own_level]) } else { ctx.from_item(&[&left_level, &own_level]) }
            .with_clause(Clause::FromFunction);
        let bound = call.args.iter().map(|a| self.bind_expr(a, &fctx)).collect::<SqlResult<Vec<_>>>()?;
        let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
        if let Some(kind) = crate::funcs::json::record_function(name) {
            let correlated = !left.cols.is_empty() && own_level.correlated.get();
            let (rel, scope) = self.bind_json_record_scan(kind, name, bound, alias, ordinality)?;
            return Ok((rel, scope, correlated));
        }
        let (out_cols, targets, srf): (Vec<(String, Type)>, Vec<Type>, crate::plan::SrfFn) =
            if let Some((outs, targets, f)) = crate::funcs::array::resolve_srf(name, &types)? {
                let names: Vec<String> =
                    if outs.len() == 1 { vec![name.to_owned()] } else { (0..outs.len()).map(|_| name.to_owned()).collect() };
                (names.into_iter().zip(outs).collect(), targets, f)
            } else if let Some((cols, f)) = crate::funcs::json::resolve_srf(name, &types) {
                (cols.into_iter().map(|(n, t)| (n.to_owned(), t)).collect(), vec![crate::funcs::json::srf_arg_type(name)], f)
            } else if let Some((cols, targets, f)) = crate::funcs::json::resolve_path_srf(name, &types) {
                (cols.into_iter().map(|(n, t)| (n.to_owned(), t)).collect(), targets, f)
            } else {
                // A scalar function in FROM yields one row with one column.
                let (e, t) = self.bind_expr(&ir::Expr::Function(Box::new(call.clone())), &fctx)?;
                let alias_name = alias.map_or_else(|| name.to_owned(), |a| a.name.clone());
                let mut cols = vec![ScopeCol::new(Some(alias_name.clone()), name.to_owned(), t)];
                if let Some(a) = alias {
                    apply_column_aliases(&mut cols, &a.columns, &a.name)?;
                    if a.columns.is_empty() {
                        cols[0].name = a.name.clone();
                    }
                }
                let correlated = !left.cols.is_empty() && own_level.correlated.get();
                return Ok((Rel::Values { rows: vec![vec![e]] }, Scope::from_cols(cols), correlated));
            };
        let args = bound
            .into_iter()
            .zip(&targets)
            .map(|((e, et), t)| super::coerce(self, e, &et, t, CoercionContext::Implicit))
            .collect::<SqlResult<Vec<_>>>()?;
        let alias_name = alias.map_or_else(|| name.to_owned(), |a| a.name.clone());
        let mut cols: Vec<ScopeCol> = out_cols.into_iter().map(|(n, t)| ScopeCol::new(Some(alias_name.clone()), n, t)).collect();
        if ordinality {
            cols.push(ScopeCol::new(Some(alias_name.clone()), "ordinality", Type::Int8));
        }
        if let Some(a) = alias {
            // For single-column functions the alias also names the column.
            if a.columns.is_empty() && cols.len() == 1 + usize::from(ordinality) {
                cols[0].name = a.name.clone();
            }
            apply_column_aliases(&mut cols, &a.columns, &a.name)?;
        }
        let correlated = !left.cols.is_empty() && own_level.correlated.get();
        Ok((Rel::FuncScan { srf: crate::plan::Srf { name: "srf", f: srf }, args, ordinality }, Scope::from_cols(cols), correlated))
    }

    /// `json(b)_populate_record(set)` / `json(b)_to_record(set)`: output columns come from the
    /// base argument's composite type or from the column definition list.
    fn bind_json_record_scan(
        &self,
        kind: crate::funcs::json::RecordFn,
        name: &str,
        mut bound: Vec<(Expr, Type)>,
        alias: Option<&ir::TableAlias>,
        ordinality: bool,
    ) -> SqlResult<(Rel, Scope)> {
        use crate::funcs::json::RecordFn;
        let doc_type = if name.starts_with("jsonb") { Type::Jsonb } else { Type::Json };
        let arg_types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
        let wrong = || crate::funcs::no_function(name, &arg_types);
        let (fields, doc, base) = match kind {
            RecordFn::Populate { .. } => {
                if bound.len() != 2 {
                    return Err(wrong());
                }
                let (doc, dt) = bound.pop().ok_or_else(wrong)?;
                let (base, bt) = bound.pop().ok_or_else(wrong)?;
                let Type::Composite(oid) = bt else {
                    return Err(SqlError::datatype_mismatch(format!("first argument of {name} must be a row type")));
                };
                (crate::usertypes::composite_fields(oid), super::coerce(self, doc, &dt, &doc_type, CoercionContext::Implicit)?, Some(base))
            }
            RecordFn::ToRecord { .. } | RecordFn::JsonTable => {
                let expected = if kind == RecordFn::JsonTable { 5 } else { 1 };
                if bound.len() != expected {
                    return Err(wrong());
                }
                let defs = alias
                    .filter(|a| !a.column_types.is_empty())
                    .ok_or_else(|| SqlError::syntax("a column definition list is required for functions returning \"record\""))?;
                let mut fields = Vec::new();
                for (n, t) in defs.columns.iter().zip(&defs.column_types) {
                    let (ty, typmod) = super::resolve_type_name(self.catalog, t)?;
                    fields.push(pgmem_types::usertype::Field { name: n.clone(), ty, typmod });
                }
                if kind == RecordFn::JsonTable {
                    // (ctx, path, vars, spec, on_error) + the descriptor.
                    let targets = [Type::Jsonb, Type::JsonPath, Type::Jsonb, Type::Text, Type::Int4];
                    let mut args = Vec::new();
                    for ((e, t), target) in std::mem::take(&mut bound).into_iter().zip(targets) {
                        args.push(super::coerce(self, e, &t, &target, CoercionContext::Explicit)?);
                    }
                    let desc = pgmem_types::usertype::bless(fields.clone());
                    args.push(Expr::Const(Value::Record(desc, Vec::new())));
                    return self.record_scan_result(kind, name, fields, args, alias, ordinality);
                }
                let (doc, dt) = bound.pop().ok_or_else(wrong)?;
                (fields, super::coerce(self, doc, &dt, &doc_type, CoercionContext::Implicit)?, None)
            }
        };
        let desc = pgmem_types::usertype::bless(fields.clone());
        let mut args = vec![doc, Expr::Const(Value::Record(desc, Vec::new()))];
        args.extend(base);
        self.record_scan_result(kind, name, fields, args, alias, ordinality)
    }

    fn record_scan_result(
        &self,
        kind: crate::funcs::json::RecordFn,
        name: &str,
        fields: Vec<pgmem_types::usertype::Field>,
        args: Vec<Expr>,
        alias: Option<&ir::TableAlias>,
        ordinality: bool,
    ) -> SqlResult<(Rel, Scope)> {
        let qualifier = alias.map_or_else(|| name.to_owned(), |a| a.name.clone());
        let mut cols: Vec<ScopeCol> = fields
            .iter()
            .map(|f| {
                let mut c = ScopeCol::new(Some(qualifier.clone()), f.name.clone(), f.ty.clone());
                c.typmod = f.typmod;
                c
            })
            .collect();
        if ordinality {
            cols.push(ScopeCol::new(Some(qualifier.clone()), "ordinality", Type::Int8));
        }
        if let Some(a) = alias
            && a.column_types.is_empty()
        {
            apply_column_aliases(&mut cols, &a.columns, &a.name)?;
        }
        let srf = crate::plan::Srf { name: "srf", f: crate::funcs::json::record_srf(kind) };
        Ok((Rel::FuncScan { srf, args, ordinality }, Scope::from_cols(cols)))
    }

    /// A named relation in FROM: CTE, table, view, or virtual catalog table.
    fn bind_relation(&self, name: &ir::ObjectName, alias: Option<&ir::TableAlias>, ctx: &Ctx) -> SqlResult<(Rel, Scope)> {
        let qualifier = alias.map_or_else(|| name.base().to_owned(), |a| a.name.clone());
        if name.0.len() == 1
            && let Some(cte) = ctx.ctes.iter().rev().find(|c| c.name == name.base())
        {
            if cte.no_returning {
                return Err(SqlError::feature_not_supported(format!("WITH query \"{}\" does not have a RETURNING clause", cte.name)));
            }
            let mut cols: Vec<ScopeCol> =
                cte.columns.iter().map(|(n, t)| ScopeCol::new(Some(qualifier.clone()), n.clone(), t.clone())).collect();
            if let Some(a) = alias {
                apply_column_aliases(&mut cols, &a.columns, &a.name)?;
            }
            return Ok((Rel::CteScan { id: cte.id }, Scope::from_cols(cols)));
        }
        if let Some(schema) = name.schema()
            && (schema == "pg_catalog" || schema == "information_schema")
        {
            return self.bind_virtual(schema, name.base(), &qualifier, alias);
        }
        let base = name.base();
        // The search path puts pg_catalog's (virtual) relations first unless it is listed later.
        enum Found {
            System,
            User(Relation),
        }
        let found = self
            .catalog
            .resolve(name, |schema, n| {
                if schema == pgmem_catalog::PG_CATALOG {
                    crate::catalog_views::lookup("pg_catalog", n).map(|_| Found::System)
                } else {
                    self.catalog.relation(schema, n).map(Found::User)
                }
            })
            .or_else(|e| if e.code == code::INVALID_SCHEMA_NAME { Ok(None) } else { Err(e) })?;
        let rel = match found {
            Some(Found::System) => return self.bind_virtual("pg_catalog", base, &qualifier, alias),
            Some(Found::User(r)) => Some(r),
            None => None,
        };
        match rel {
            Some(Relation::Table(def)) => {
                let mut cols = Vec::new();
                let mut slots = Vec::new();
                let mut missing = Vec::new();
                for (i, c) in def.columns.iter().enumerate() {
                    if c.dropped {
                        continue;
                    }
                    let mut sc = ScopeCol::new(Some(qualifier.clone()), c.name.clone(), c.ty.clone());
                    sc.table_oid = def.oid;
                    sc.attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
                    sc.typmod = c.typmod;
                    cols.push(sc);
                    slots.push(i);
                    missing.push(c.missing.clone());
                }
                cols.extend(super::system_scope_cols(&qualifier));
                if let Some(a) = alias {
                    apply_column_aliases(&mut cols, &a.columns, &a.name)?;
                }
                Ok((Rel::Scan { table: def.oid, slots, missing, system: true }, Scope::from_cols(cols)))
            }
            Some(Relation::View(view)) => {
                let top = Level::new(Scope::default());
                let vctx = Ctx::new(&top);
                let bound = pgmem_types::context::with_search_path(&view.search_path, || self.bind_query(&view.query, &vctx))?;
                let mut cols: Vec<ScopeCol> = bound
                    .columns
                    .iter()
                    .zip(&view.columns)
                    .map(|(c, n)| ScopeCol::new(Some(qualifier.clone()), n.clone(), c.ty.clone()))
                    .collect();
                if let Some(a) = alias {
                    apply_column_aliases(&mut cols, &a.columns, &a.name)?;
                }
                Ok((bound.rel, Scope::from_cols(cols)))
            }
            Some(Relation::Sequence(seq)) => {
                // SELECT * FROM seq: last_value, log_cnt, is_called.
                let cols = vec![
                    ScopeCol::new(Some(qualifier.clone()), "last_value", Type::Int8),
                    ScopeCol::new(Some(qualifier.clone()), "log_cnt", Type::Int8),
                    ScopeCol::new(Some(qualifier), "is_called", Type::Bool),
                ];
                let _ = seq;
                Err(feature("selecting from a sequence")).or(Ok((Rel::Values { rows: vec![] }, Scope::from_cols(cols))))
            }
            Some(Relation::Index { .. }) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is an index"))),
            None => Err(SqlError::undefined_table(&name.to_string())),
        }
    }

    fn bind_virtual(&self, schema: &str, name: &str, qualifier: &str, alias: Option<&ir::TableAlias>) -> SqlResult<(Rel, Scope)> {
        let view = crate::catalog_views::lookup(schema, name).ok_or_else(|| SqlError::undefined_table(&format!("{schema}.{name}")))?;
        let mut cols: Vec<ScopeCol> = view.columns.into_iter().map(|(n, t)| ScopeCol::new(Some(qualifier.to_owned()), n, t)).collect();
        if let Some(a) = alias {
            apply_column_aliases(&mut cols, &a.columns, &a.name)?;
        }
        Ok((Rel::Virtual { rows: crate::plan::VirtualRows(view.rows) }, Scope::from_cols(cols)))
    }
}

enum OrderTarget {
    Output(usize),
    Expr(Expr),
}

fn ordinal(e: &ir::Expr) -> Option<usize> {
    match e {
        ir::Expr::Literal(ir::Literal::Integer(s)) => s.parse().ok(),
        _ => None,
    }
}

fn bare_name(e: &ir::Expr) -> Option<String> {
    match e {
        ir::Expr::Column(parts) if parts.len() == 1 => Some(parts[0].clone()),
        _ => None,
    }
}

fn contains_column(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if matches!(x, Expr::Col(_)) {
            found = true;
        }
    });
    found
}

/// Places a WITH TIES limit right above the query's sort (below the projection that drops
/// hidden ORDER BY columns), comparing rows on the sort keys.
fn limit_with_ties(rel: Rel, limit: Option<Expr>, offset: Option<Expr>) -> Rel {
    fn keys_of(rel: &Rel) -> Option<Vec<SortKey>> {
        match rel {
            Rel::Sort { keys, .. } => Some(keys.clone()),
            Rel::Distinct { input, .. } => keys_of(input),
            _ => None,
        }
    }
    match rel {
        Rel::Project { input, exprs } if keys_of(&input).is_some() && exprs.iter().all(|e| matches!(e, Expr::Col(_))) => {
            let ties = keys_of(&input);
            Rel::Project { input: Box::new(Rel::Limit { input, limit, offset, ties }), exprs }
        }
        rel => {
            let ties = keys_of(&rel);
            Rel::Limit { input: Box::new(rel), limit, offset, ties }
        }
    }
}

/// Select-list SRF calls whose arguments contain no other SRF call (deduplicated).
fn collect_innermost_srfs(e: &Expr, out: &mut Vec<Expr>) {
    e.walk(&mut |x| {
        if let Expr::SrfCall { args, .. } = x {
            let mut nested = false;
            for a in args {
                a.walk(&mut |y| nested |= matches!(y, Expr::SrfCall { .. }));
            }
            if !nested && !out.contains(x) {
                out.push(x.clone());
            }
        }
    });
}

/// A non-LATERAL subquery naming a FROM item to its left: Postgres says the entry exists but
/// can't be referenced from there.
fn sibling_reference(e: SqlError, left: &Scope) -> SqlError {
    let named = (e.code == code::UNDEFINED_TABLE).then(|| e.message.clone()).and_then(|m| {
        m.strip_prefix("missing FROM-clause entry for table \"")
            .and_then(|r| r.strip_suffix('"'))
            .filter(|q| left.has_qualifier(q))
            .map(str::to_owned)
    });
    match named {
        Some(q) => SqlError::new(code::UNDEFINED_TABLE, format!("invalid reference to FROM-clause entry for table \"{q}\""))
            .with_hint(format!("There is an entry for table \"{q}\", but it cannot be referenced from this part of the query.")),
        None => e,
    }
}

fn find_col(scope: &Scope, name: &str) -> Option<usize> {
    scope.cols.iter().position(|c| !c.dropped && !c.hidden_unqualified && c.name == name)
}

fn check_duplicate_aliases(l: &Scope, r: &Scope) -> SqlResult<()> {
    let lq: std::collections::HashSet<&str> = l.cols.iter().filter_map(|c| c.qualifier.as_deref()).collect();
    if let Some(dup) = r.cols.iter().filter_map(|c| c.qualifier.as_deref()).find(|q| lq.contains(q)) {
        return Err(SqlError::new(code::DUPLICATE_ALIAS, format!("table name \"{dup}\" specified more than once")));
    }
    Ok(())
}

/// Combines two scopes for a join. USING columns appear once in `*` (first), and unqualified
/// references resolve to the left copy (the right copy for RIGHT JOIN).
fn join_scopes(l: Scope, r: Scope, using: &[String], kind: JoinKind) -> Scope {
    let lw = l.width();
    let mut cols = l.cols.clone();
    cols.extend(r.cols.iter().cloned());
    let mut star = Vec::new();
    let mut merged_l = Vec::new();
    let mut merged_r = Vec::new();
    for name in using {
        let li = find_col(&l, name);
        let ri = find_col(&r, name);
        if let (Some(li), Some(ri)) = (li, ri) {
            let (keep, hide) = if kind == JoinKind::Right { (lw + ri, li) } else { (li, lw + ri) };
            cols[hide].hidden_unqualified = true;
            star.push(keep);
            merged_l.push(li);
            merged_r.push(lw + ri);
        }
    }
    star.extend(l.star.iter().copied().filter(|i| !merged_l.contains(i)));
    star.extend(r.star.iter().map(|i| i + lw).filter(|i| !merged_r.contains(i)));
    Scope { cols, star }
}

fn apply_column_aliases(cols: &mut [ScopeCol], aliases: &[String], table: &str) -> SqlResult<()> {
    let live: Vec<usize> = cols.iter().enumerate().filter(|(_, c)| !c.dropped && !c.system).map(|(i, _)| i).collect();
    if aliases.len() > live.len() {
        return Err(SqlError::new(
            code::INVALID_COLUMN_REFERENCE,
            format!("table \"{table}\" has {} columns available but {} columns specified", live.len(), aliases.len()),
        ));
    }
    for (a, &i) in aliases.iter().zip(&live) {
        cols[i].name = a.clone();
    }
    for c in cols.iter_mut() {
        c.qualifier = Some(table.to_owned());
    }
    Ok(())
}

pub(crate) fn rename_columns(columns: &mut [(String, Type)], names: &[String], cte: &str) -> SqlResult<()> {
    if names.len() > columns.len() {
        return Err(SqlError::new(
            code::INVALID_COLUMN_REFERENCE,
            format!("WITH query \"{cte}\" has {} columns available but {} columns specified", columns.len(), names.len()),
        ));
    }
    for (c, n) in columns.iter_mut().zip(names) {
        c.0 = n.clone();
    }
    Ok(())
}

/// Wraps a plan in a projection that casts columns to the target types where they differ.
fn coerce_columns(b: &Binder, rel: Rel, from: &[Type], to: &[Type]) -> SqlResult<Rel> {
    if from == to {
        return Ok(rel);
    }
    // VALUES rows can be coerced in place (so literals are parsed into the right type).
    if let Rel::Values { rows } = rel {
        let rows = rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .zip(from.iter().zip(to))
                    .map(|(e, (f, t))| super::coerce(b, e, f, t, CoercionContext::Implicit))
                    .collect::<SqlResult<Vec<_>>>()
            })
            .collect::<SqlResult<Vec<_>>>()?;
        return Ok(Rel::Values { rows });
    }
    let exprs = from
        .iter()
        .zip(to)
        .enumerate()
        .map(|(i, (f, t))| super::coerce(b, Expr::Col(i), f, t, CoercionContext::Implicit))
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(Rel::Project { input: Box::new(rel), exprs })
}

fn collect_aggs(e: &Expr, out: &mut Vec<AggCall>) {
    e.walk(&mut |x| match x {
        Expr::AggCall(a) if !out.contains(a) => out.push((**a).clone()),
        // A window call's arguments may hold the query's aggregates.
        Expr::WindowCall(w) => {
            let agg_args = w.agg.iter().flat_map(|a| a.args.iter());
            for inner in w.args.iter().chain(&w.partition_by).chain(w.order_by.iter().map(|k| &k.expr)).chain(agg_args) {
                collect_aggs(inner, out);
            }
        }
        _ => {}
    });
}

/// Whether `e` holds a window function call.
pub(crate) fn select_contains_window(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| found |= matches!(x, Expr::WindowCall(_)));
    found
}

fn collect_windows(e: &Expr, out: &mut Vec<WinCall>) {
    e.walk(&mut |x| {
        if let Expr::WindowCall(w) = x
            && !out.contains(w)
        {
            out.push((**w).clone());
        }
    });
}

/// Tables whose primary key is fully covered by plain-column GROUP BY keys: their other
/// columns are functionally dependent and may appear ungrouped (Postgres' rule).
fn functional_dependencies(group: &[Expr], scope: &Scope, catalog: &pgmem_catalog::Catalog) -> Vec<pgmem_catalog::Oid> {
    let grouped_cols: Vec<usize> = group.iter().filter_map(|g| if let Expr::Col(i) = g { Some(*i) } else { None }).collect();
    let mut out = Vec::new();
    for t in catalog.tables() {
        let Some(pk) = t.primary_key() else { continue };
        // Every qualifier instance of this table in scope is considered separately.
        let quals: std::collections::BTreeSet<Option<String>> =
            scope.cols.iter().filter(|c| c.table_oid == t.oid).map(|c| c.qualifier.clone()).collect();
        for q in quals {
            let covered = pk.iter().all(|pkcol| {
                scope.cols.iter().enumerate().any(|(i, c)| {
                    c.table_oid == t.oid
                        && c.qualifier == q
                        && usize::from(c.attnum.unsigned_abs()) == pkcol + 1
                        && grouped_cols.contains(&i)
                })
            });
            if covered {
                out.push(t.oid);
            }
        }
    }
    out
}

/// Rewrites grouped-query expressions to reference the Aggregate's output.
struct Grouping<'s> {
    group: Vec<Expr>,
    aggs: Vec<AggCall>,
    scope: &'s Scope,
    pk_tables: Vec<pgmem_catalog::Oid>,
    sets: Option<Vec<Vec<usize>>>,
}

impl Grouping<'_> {
    fn rewrite(&mut self, e: Expr) -> SqlResult<Expr> {
        if let Some(i) = self.group.iter().position(|g| *g == e) {
            return Ok(Expr::Col(i));
        }
        match e {
            Expr::Grouping { args, .. } => {
                // Bit n (from the right, for the last argument) is set when that argument is
                // not grouped by the row's set.
                let mut idx = Vec::with_capacity(args.len());
                for a in &args {
                    idx.push(self.group.iter().position(|g| g == a).ok_or_else(|| {
                        SqlError::new(
                            code::GROUPING_ERROR,
                            "arguments to GROUPING must be grouping expressions of the associated query level",
                        )
                    })?);
                }
                let sets = self.sets.clone().unwrap_or_else(|| vec![(0..self.group.len()).collect()]);
                let values = sets.iter().map(|set| idx.iter().fold(0i32, |acc, i| (acc << 1) | i32::from(!set.contains(i)))).collect();
                Ok(Expr::Grouping { args: Vec::new(), col: usize::MAX, values })
            }
            Expr::AggCall(a) => {
                let pos = match self.aggs.iter().position(|x| *x == *a) {
                    Some(p) => p,
                    None => {
                        self.aggs.push(*a);
                        self.aggs.len() - 1
                    }
                };
                Ok(Expr::Col(self.group.len() + pos))
            }
            Expr::Col(i) => {
                let c = &self.scope.cols[i];
                if c.table_oid != 0 && self.pk_tables.contains(&c.table_oid) {
                    // Functionally dependent on a grouped primary key: carry it as an extra key.
                    self.group.push(Expr::Col(i));
                    return Ok(Expr::Col(self.group.len() - 1));
                }
                let qual = c.qualifier.as_deref().map(|q| format!("{q}.")).unwrap_or_default();
                Err(SqlError::new(
                    code::GROUPING_ERROR,
                    format!("column \"{qual}{}\" must appear in the GROUP BY clause or be used in an aggregate function", c.name),
                ))
            }
            Expr::Subquery { correlated: true, .. } => Err(feature("correlated subqueries in the select list of a grouped query")),
            Expr::WindowCall(mut w) => {
                w.args = w.args.into_iter().map(|a| self.rewrite(a)).collect::<SqlResult<Vec<_>>>()?;
                w.partition_by = w.partition_by.into_iter().map(|a| self.rewrite(a)).collect::<SqlResult<Vec<_>>>()?;
                w.order_by =
                    w.order_by.into_iter().map(|k| Ok(SortKey { expr: self.rewrite(k.expr)?, ..k })).collect::<SqlResult<Vec<_>>>()?;
                if let Some(agg) = w.agg.take() {
                    let mut agg = agg;
                    agg.args = agg.args.into_iter().map(|a| self.rewrite(a)).collect::<SqlResult<Vec<_>>>()?;
                    w.agg = Some(agg);
                }
                Ok(Expr::WindowCall(w))
            }
            other => map_children(other, &mut |c| self.rewrite(c)),
        }
    }
}

/// Applies `f` to each direct child expression.
fn map_children(e: Expr, f: &mut dyn FnMut(Expr) -> SqlResult<Expr>) -> SqlResult<Expr> {
    let b = |x: Box<Expr>, f: &mut dyn FnMut(Expr) -> SqlResult<Expr>| -> SqlResult<Box<Expr>> { Ok(Box::new(f(*x)?)) };
    Ok(match e {
        leaf @ (Expr::Const(_) | Expr::Col(_) | Expr::Outer { .. } | Expr::Param(_) | Expr::AggCall(_) | Expr::WindowCall(_)) => leaf,
        Expr::Call { func, args } => Expr::Call { func, args: args.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()? },
        Expr::Cast { expr, to, typmod } => Expr::Cast { expr: b(expr, f)?, to, typmod },
        Expr::And(v) => Expr::And(v.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?),
        Expr::Or(v) => Expr::Or(v.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?),
        Expr::Not(x) => Expr::Not(b(x, f)?),
        Expr::IsNull { expr, negated } => Expr::IsNull { expr: b(expr, f)?, negated },
        Expr::IsDistinct { left, right, negated } => Expr::IsDistinct { left: b(left, f)?, right: b(right, f)?, negated },
        Expr::Case { operand, whens, else_result } => Expr::Case {
            operand: operand.map(|o| b(o, f)).transpose()?,
            whens: whens.into_iter().map(|(w, t)| Ok((f(w)?, f(t)?))).collect::<SqlResult<Vec<_>>>()?,
            else_result: else_result.map(|x| b(x, f)).transpose()?,
        },
        Expr::Coalesce(v) => Expr::Coalesce(v.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?),
        Expr::Grouping { args, col, values } => {
            Expr::Grouping { args: args.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?, col, values }
        }
        Expr::MinMax { args, greatest } => Expr::MinMax { args: args.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?, greatest },
        Expr::NullIf(x, y) => Expr::NullIf(b(x, f)?, b(y, f)?),
        Expr::InList { probe, list, cmp, negated } => {
            Expr::InList { probe: b(probe, f)?, list: list.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?, cmp, negated }
        }
        Expr::AnyAll { left, array, cmp, all } => Expr::AnyAll { left: b(left, f)?, array: b(array, f)?, cmp, all },
        Expr::Subquery { id, plan, kind, correlated } => {
            let kind = match kind {
                crate::plan::SubqueryKind::Quantified { probe, cmp, all } => {
                    crate::plan::SubqueryKind::Quantified { probe: b(probe, f)?, cmp, all }
                }
                k => k,
            };
            Expr::Subquery { id, plan, kind, correlated }
        }
        Expr::Array(v) => Expr::Array(v.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?),
        Expr::SrfCall { srf, args, cols } => {
            Expr::SrfCall { srf, args: args.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?, cols }
        }
        Expr::Subscript { expr, subs, slice } => Expr::Subscript {
            expr: b(expr, f)?,
            subs: subs
                .into_iter()
                .map(|s| {
                    Ok(crate::plan::SubscriptPart {
                        lower: s.lower.map(&mut *f).transpose()?,
                        upper: s.upper.map(&mut *f).transpose()?,
                        range: s.range,
                    })
                })
                .collect::<SqlResult<Vec<_>>>()?,
            slice,
        },
        Expr::Row { fields, types, type_oid } => {
            Expr::Row { fields: fields.into_iter().map(&mut *f).collect::<SqlResult<Vec<_>>>()?, types, type_oid }
        }
        Expr::Field { expr, idx } => Expr::Field { expr: b(expr, f)?, idx },
        Expr::FieldStore { expr, idx, value, type_oid, width } => {
            Expr::FieldStore { expr: b(expr, f)?, idx, value: b(value, f)?, type_oid, width }
        }
        Expr::CoerceToDomain(mut d) => {
            d.expr = f(d.expr)?;
            Expr::CoerceToDomain(d)
        }
    })
}

/// True if the plan aggregates at its own level (not inside a FROM subquery's plan).
fn has_aggregate(rel: &Rel) -> bool {
    match rel {
        Rel::Aggregate { .. } => true,
        Rel::Project { input, .. }
        | Rel::Filter { input, .. }
        | Rel::Sort { input, .. }
        | Rel::Limit { input, .. }
        | Rel::Distinct { input, .. }
        | Rel::Window { input, .. } => has_aggregate(input),
        _ => false,
    }
}

/// True if a query references `name` (a recursive CTE's self-reference) in a FROM clause.
fn query_references(q: &ir::Query, name: &str) -> bool {
    in_set(&q.body, name)
}

fn in_set(s: &ir::SetExpr, name: &str) -> bool {
    fn in_ref(t: &ir::TableRef, name: &str) -> bool {
        match t {
            ir::TableRef::Table { name: n, .. } => n.0.len() == 1 && n.base() == name,
            ir::TableRef::Subquery { query, .. } => query_references(query, name),
            ir::TableRef::Join { left, right, .. } => in_ref(left, name) || in_ref(right, name),
            ir::TableRef::Function { .. } => false,
        }
    }
    match s {
        ir::SetExpr::Select(sel) => sel.from.iter().any(|t| in_ref(t, name)),
        ir::SetExpr::Values(_) => false,
        ir::SetExpr::SetOp { left, right, .. } => in_set(left, name) || in_set(right, name),
        ir::SetExpr::Query(q) => query_references(q, name),
        ir::SetExpr::Dml(_) => false,
    }
}

/// LockRows over a SELECT whose rows carry `targets` (tableoid, ctid) pairs after `visible`
/// columns; LIMIT / OFFSET count the rows that were locked (SKIP LOCKED ones do not count).
fn lock_rows(input: Rel, locking: &[ir::LockClause], targets: usize, visible: usize, limit: Option<Expr>, offset: Option<Expr>) -> Rel {
    use pgmem_storage::locks::RowLockMode;
    let strongest = locking.iter().map(|l| l.strength).max_by_key(|s| match s {
        ir::LockStrength::KeyShare => 0,
        ir::LockStrength::Share => 1,
        ir::LockStrength::NoKeyUpdate => 2,
        ir::LockStrength::Update => 3,
    });
    let mode = match strongest {
        Some(ir::LockStrength::KeyShare) => RowLockMode::KeyShare,
        Some(ir::LockStrength::Share) => RowLockMode::Share,
        Some(ir::LockStrength::NoKeyUpdate) => RowLockMode::NoKeyUpdate,
        _ => RowLockMode::Update,
    };
    let wait = if locking.iter().any(|l| l.nowait) {
        pgmem_storage::LockWait::NoWait
    } else if locking.iter().any(|l| l.skip_locked) {
        pgmem_storage::LockWait::SkipLocked
    } else {
        pgmem_storage::LockWait::Block
    };
    Rel::LockRows {
        input: Box::new(input),
        targets: (0..targets).map(|i| (visible + 2 * i, visible + 2 * i + 1)).collect(),
        mode,
        wait,
        limit,
        offset,
        visible,
    }
}

fn lock_strength_name(l: &ir::LockClause) -> &'static str {
    match l.strength {
        ir::LockStrength::Update => "FOR UPDATE",
        ir::LockStrength::NoKeyUpdate => "FOR NO KEY UPDATE",
        ir::LockStrength::Share => "FOR SHARE",
        ir::LockStrength::KeyShare => "FOR KEY SHARE",
    }
}

fn lock_misuse(l: &ir::LockClause, what: &str) -> SqlError {
    SqlError::feature_not_supported(format!("{} is not allowed with {what}", lock_strength_name(l)))
}

/// The `tableoid` and `ctid` columns of each table a locking clause applies to (all tables
/// in FROM, or those named in OF), as pairs.
fn lock_targets(scope: &Scope, locking: &[ir::LockClause]) -> SqlResult<Vec<Expr>> {
    let Some(first) = locking.first() else { return Ok(Vec::new()) };
    let named: Vec<&str> = locking.iter().flat_map(|l| l.of.iter().map(|n| n.base())).collect();
    for n in &named {
        if !scope.cols.iter().any(|c| c.system && c.qualifier.as_deref() == Some(*n)) {
            return Err(SqlError::new(
                code::UNDEFINED_TABLE,
                format!("relation \"{n}\" in {} clause not found in FROM clause", lock_strength_name(first)),
            ));
        }
    }
    let mut out = Vec::new();
    for (i, c) in scope.cols.iter().enumerate() {
        if !(c.system && c.name == "ctid") {
            continue;
        }
        let q = c.qualifier.as_deref().unwrap_or("");
        if !named.is_empty() && !named.contains(&q) {
            continue;
        }
        let oid_col = scope.cols.iter().position(|x| x.system && x.name == "tableoid" && x.qualifier == c.qualifier);
        if let Some(o) = oid_col {
            out.push(Expr::Col(o));
            out.push(Expr::Col(i));
        }
    }
    Ok(out)
}

/// A data-modifying WITH query below the statement's own WITH.
pub(crate) fn dml_not_top_level() -> SqlError {
    SqlError::feature_not_supported("WITH clause containing a data-modifying statement must be at the top level")
}

#[allow(dead_code)]
fn unused(_: Value) {}
