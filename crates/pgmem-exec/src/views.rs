//! Automatically updatable views (Postgres' `view_query_is_auto_updatable`): INSERT, UPDATE
//! and DELETE on a simple view become the same statement on its base table, restricted by the
//! view's WHERE and, with WITH CHECK OPTION, checked against it.

use pgmem_catalog::{Catalog, Relation, ViewDef};
use pgmem_ir as ir;
use pgmem_types::SqlResult;

const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "bool_and",
    "bool_or",
    "every",
    "string_agg",
    "array_agg",
    "json_agg",
    "jsonb_agg",
    "json_object_agg",
    "jsonb_object_agg",
    "stddev",
    "stddev_samp",
    "stddev_pop",
    "variance",
    "var_samp",
    "var_pop",
    "bit_and",
    "bit_or",
    "bit_xor",
    "corr",
    "covar_pop",
    "covar_samp",
    "regr_count",
    "regr_avgx",
    "regr_avgy",
    "regr_slope",
    "regr_intercept",
    "regr_r2",
    "regr_sxx",
    "regr_syy",
    "regr_sxy",
    "percentile_cont",
    "percentile_disc",
    "mode",
];

/// A simple view resolved to the table it writes.
#[derive(Clone, Debug)]
pub struct Updatable {
    /// The base table, schema-qualified.
    pub base: ir::ObjectName,
    /// The WHERE clauses of this view and the views below it, with the FROM qualifier each is
    /// written against.
    pub filters: Vec<(ir::Expr, String)>,
    /// WITH CHECK OPTION predicates: (view name, predicate, qualifier).
    pub checks: Vec<(String, ir::Expr, String)>,
    /// Per view column, in order: (view column name, the base column it is, if any).
    pub columns: Vec<(String, Option<String>)>,
}

fn contains_call(e: &ir::Expr, window: bool) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let ir::Expr::Function(f) = x {
            let is_window = f.over.is_some();
            if (window && is_window) || (!window && !is_window && AGGREGATES.contains(&f.name.base())) {
                found = true;
            }
        }
    });
    found
}

/// Why the view is not automatically updatable (Postgres' DETAIL), or None.
pub fn not_updatable_reason(v: &ViewDef) -> Option<&'static str> {
    let q = &v.query;
    let ir::SetExpr::Select(sel) = &q.body else {
        return Some("Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.");
    };
    if q.with.is_some() {
        return Some("Views containing WITH are not automatically updatable.");
    }
    if sel.distinct != ir::Distinct::All {
        return Some("Views containing DISTINCT are not automatically updatable.");
    }
    if !sel.group_by.is_empty() {
        return Some("Views containing GROUP BY are not automatically updatable.");
    }
    if sel.having.is_some() {
        return Some("Views containing HAVING are not automatically updatable.");
    }
    if q.limit.is_some() || q.offset.is_some() {
        return Some("Views containing LIMIT or OFFSET are not automatically updatable.");
    }
    let exprs = || {
        sel.projection.iter().filter_map(|p| match p {
            ir::SelectItem::Expr { expr, .. } => Some(expr),
            _ => None,
        })
    };
    if exprs().any(|e| contains_call(e, false)) {
        return Some("Views that return aggregate functions are not automatically updatable.");
    }
    if exprs().any(|e| contains_call(e, true)) {
        return Some("Views that return window functions are not automatically updatable.");
    }
    if !matches!(sel.from.as_slice(), [ir::TableRef::Table { .. }]) {
        return Some("Views that do not select from a single table or view are not automatically updatable.");
    }
    None
}

/// Resolves `v` to its base table, or the reason it cannot be written.
pub fn resolve(catalog: &Catalog, v: &ViewDef) -> SqlResult<Result<Updatable, &'static str>> {
    if let Some(reason) = not_updatable_reason(v) {
        return Ok(Err(reason));
    }
    let ir::SetExpr::Select(sel) = &v.query.body else { return Ok(Err("")) };
    let [ir::TableRef::Table { name, alias, .. }] = sel.from.as_slice() else { return Ok(Err("")) };
    let qualifier = alias.as_ref().map_or_else(|| name.base().to_owned(), |a| a.name.clone());
    let rel = pgmem_types::context::with_search_path(&v.search_path, || catalog.find_relation(name))?;
    // The columns below this view, by name, and how to write them.
    let (base, below_columns, mut filters, mut checks): (ir::ObjectName, Vec<(String, Option<String>)>, _, _) = match rel {
        Some(Relation::Table(t)) => {
            let cols = t.live_columns().map(|(_, c)| (c.name.clone(), Some(c.name.clone()))).collect();
            (ir::ObjectName(vec![t.schema.clone(), t.name.clone()]), cols, Vec::new(), Vec::new())
        }
        Some(Relation::View(inner)) => match resolve(catalog, &inner)? {
            Ok(u) => (u.base, u.columns, u.filters, u.checks),
            Err(reason) => return Ok(Err(reason)),
        },
        _ => return Ok(Err("Views that do not select from a single table or view are not automatically updatable.")),
    };
    let mut columns = Vec::new();
    for item in &sel.projection {
        match item {
            ir::SelectItem::Wildcard | ir::SelectItem::QualifiedWildcard(_) => columns.extend(below_columns.iter().cloned()),
            ir::SelectItem::Expr { expr: ir::Expr::Column(parts), .. } => {
                let source = parts.last().cloned().unwrap_or_default();
                let target = below_columns.iter().find(|(n, _)| *n == source).and_then(|(_, b)| b.clone());
                columns.push((source, target));
            }
            ir::SelectItem::Expr { .. } | ir::SelectItem::FieldStar(_) => columns.push((String::new(), None)),
        }
    }
    // The view's own column names (a column list or aliases rename them).
    for (i, out) in v.columns.iter().enumerate() {
        if let Some(c) = columns.get_mut(i) {
            c.0.clone_from(out);
        }
    }
    if let Some(w) = &sel.selection {
        filters.push((w.clone(), qualifier.clone()));
        if v.check_option {
            checks.push((v.name.clone(), w.clone(), qualifier.clone()));
        }
    }
    // WITH CASCADED CHECK OPTION checks the views below too.
    if v.check_option {
        for (w, q) in filters.iter().take(filters.len().saturating_sub(usize::from(sel.selection.is_some()))) {
            checks.push((v.name.clone(), w.clone(), q.clone()));
        }
    }
    Ok(Ok(Updatable { base, filters, checks, columns }))
}

/// What a view's query reads: relations as written (resolve them with the view's search
/// path), column names, and whether it selects `*`.
#[derive(Default)]
pub struct Refs {
    pub relations: Vec<ir::ObjectName>,
    pub columns: Vec<String>,
    pub wildcard: bool,
}

/// The relations a view's query reads: FROM items, joins, subqueries and the bodies of its
/// WITH queries, minus the CTE names.
pub fn referenced_relations(q: &ir::Query) -> Vec<ir::ObjectName> {
    references(q).relations
}

pub fn references(q: &ir::Query) -> Refs {
    let mut out = Refs::default();
    collect_query(q, &[], &mut out);
    out
}

fn collect_query(q: &ir::Query, ctes: &[String], out: &mut Refs) {
    let mut scope: Vec<String> = ctes.to_vec();
    if let Some(w) = &q.with {
        for c in &w.ctes {
            scope.push(c.name.clone());
            collect_query(&c.query, &scope, out);
        }
    }
    collect_set(&q.body, &scope, out);
    for o in &q.order_by {
        collect_expr(&o.expr, &scope, out);
    }
}

fn collect_set(s: &ir::SetExpr, ctes: &[String], out: &mut Refs) {
    match s {
        ir::SetExpr::Select(sel) => {
            for t in &sel.from {
                collect_ref(t, ctes, out);
            }
            for p in &sel.projection {
                match p {
                    ir::SelectItem::Expr { expr, .. } | ir::SelectItem::FieldStar(expr) => collect_expr(expr, ctes, out),
                    ir::SelectItem::Wildcard | ir::SelectItem::QualifiedWildcard(_) => out.wildcard = true,
                }
            }
            for e in sel.selection.iter().chain(sel.having.iter()).chain(sel.group_by.iter()) {
                collect_expr(e, ctes, out);
            }
        }
        ir::SetExpr::Values(rows) => rows.iter().flatten().for_each(|e| collect_expr(e, ctes, out)),
        ir::SetExpr::SetOp { left, right, .. } => {
            collect_set(left, ctes, out);
            collect_set(right, ctes, out);
        }
        ir::SetExpr::Query(q) => collect_query(q, ctes, out),
        ir::SetExpr::Dml(_) => {}
    }
}

fn collect_ref(t: &ir::TableRef, ctes: &[String], out: &mut Refs) {
    match t {
        ir::TableRef::Table { name, .. } => {
            if !(name.0.len() == 1 && ctes.iter().any(|c| c == name.base())) && !out.relations.contains(name) {
                out.relations.push(name.clone());
            }
        }
        ir::TableRef::Subquery { query, .. } => collect_query(query, ctes, out),
        ir::TableRef::Join { left, right, constraint, .. } => {
            collect_ref(left, ctes, out);
            collect_ref(right, ctes, out);
            match constraint {
                ir::JoinConstraint::On(e) => collect_expr(e, ctes, out),
                ir::JoinConstraint::Using(cols) => out.columns.extend(cols.iter().cloned()),
                _ => {}
            }
        }
        ir::TableRef::Function { .. } => {}
    }
}

fn collect_expr(e: &ir::Expr, ctes: &[String], out: &mut Refs) {
    let mut queries: Vec<ir::Query> = Vec::new();
    e.walk(&mut |x| match x {
        ir::Expr::Subquery(q) | ir::Expr::ArraySubquery(q) => queries.push((**q).clone()),
        ir::Expr::Exists { query, .. } | ir::Expr::InSubquery { query, .. } => queries.push((**query).clone()),
        ir::Expr::Column(parts) => out.columns.extend(parts.last().cloned()),
        _ => {}
    });
    for q in &queries {
        collect_query(q, ctes, out);
    }
}
