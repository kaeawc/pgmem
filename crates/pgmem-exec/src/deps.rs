//! What depends on a relation (Postgres' `pg_depend` for the cases pgmem models): views on
//! the relations they read, column defaults on the sequences they draw from.

use pgmem_catalog::{Catalog, Oid, QualName, ViewDef};
use pgmem_ir as ir;
use std::sync::Arc;

/// The OIDs of the relations `view` reads.
fn view_reads(catalog: &Catalog, view: &ViewDef) -> Vec<Oid> {
    let names = crate::views::referenced_relations(&view.query);
    pgmem_types::context::with_search_path(&view.search_path, || {
        names.iter().filter_map(|n| catalog.find_relation(n).ok().flatten().map(|r| r.oid())).collect()
    })
}

/// Views reading relation `oid` directly.
pub fn dependent_views(catalog: &Catalog, oid: Oid) -> Vec<Arc<ViewDef>> {
    catalog.views().filter(|v| v.oid != oid && view_reads(catalog, v).contains(&oid)).cloned().collect()
}

/// Every view depending on `oid`, directly or through other views (dependents first in
/// drop order: the deepest last).
pub fn dependent_views_transitive(catalog: &Catalog, oid: Oid) -> Vec<Arc<ViewDef>> {
    let mut out: Vec<Arc<ViewDef>> = Vec::new();
    let mut frontier = vec![oid];
    while let Some(o) = frontier.pop() {
        for v in dependent_views(catalog, o) {
            if !out.iter().any(|x| x.oid == v.oid) {
                frontier.push(v.oid);
                out.push(v);
            }
        }
    }
    out
}

/// Views reading column `column` of table `oid` directly (a `*` reads every column).
pub fn views_using_column(catalog: &Catalog, oid: Oid, column: &str) -> Vec<Arc<ViewDef>> {
    dependent_views(catalog, oid).into_iter().filter(|v| query_mentions(&v.query, column)).collect()
}

fn query_mentions(q: &ir::Query, column: &str) -> bool {
    let refs = crate::views::references(q);
    refs.wildcard || refs.columns.iter().any(|c| c == column)
}

/// The sequence a `nextval('name')` / `currval` / `setval` call in `e` names.
fn sequences_in(e: &ir::Expr) -> Vec<ir::ObjectName> {
    let mut out = Vec::new();
    e.walk(&mut |x| {
        if let ir::Expr::Function(f) = x
            && matches!(f.name.base(), "nextval" | "currval" | "setval")
        {
            let arg = match f.args.first() {
                Some(ir::Expr::Cast { expr, .. }) => expr.as_ref(),
                Some(other) => other,
                None => return,
            };
            if let ir::Expr::Literal(ir::Literal::String(s)) = arg {
                out.push(ir::ObjectName(s.split('.').map(|p| p.trim_matches('"').to_owned()).collect()));
            }
        }
    });
    out
}

/// Columns whose DEFAULT draws from sequence `oid`: (table, column slot).
pub fn defaults_using_sequence(catalog: &Catalog, oid: Oid) -> Vec<(QualName, usize)> {
    let mut out = Vec::new();
    for t in catalog.tables() {
        for (i, c) in t.live_columns() {
            let Some(d) = &c.default else { continue };
            let uses = pgmem_types::context::with_search_path(&[t.schema.clone(), "public".into()], || {
                sequences_in(d).iter().any(|n| catalog.find_relation(n).ok().flatten().is_some_and(|r| r.oid() == oid))
            });
            if uses {
                out.push((t.qualified(), i));
            }
        }
    }
    out
}
