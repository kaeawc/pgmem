//! `pg_policy` and the `pg_policies` view: row-level security policies.

use super::{boolean, ch, name, oid, rows_of, snapshot, txt};
use crate::eval::EvalCtx;
use crate::plan::Row;
use pgmem_catalog::PolicyDef;
use pgmem_types::{SqlResult, Value};

pub(super) const PG_POLICY: &str = "oid:oid, polname:name, polrelid:oid, polcmd:\"char\", polpermissive:bool, polroles:oid[], \
    polqual:pg_node_tree, polwithcheck:pg_node_tree";
pub(super) const PG_POLICIES: &str = "schemaname:name, tablename:name, policyname:name, permissive:text, roles:name[], cmd:text, \
    qual:text, with_check:text";

/// The policies of the transaction's tables, in OID order, with their table and deparsed
/// USING / WITH CHECK expressions (`pg_get_expr(polqual, polrelid)`).
fn policies<'a>(ctx: &'a EvalCtx<'a>) -> Vec<(PolicyDef, &'a pgmem_catalog::TableDef, Option<String>, Option<String>)> {
    let m = snapshot(ctx);
    let catalog = ctx.txn.catalog();
    let tables = m.tables();
    catalog
        .policies()
        .filter_map(|p| {
            let t = tables.iter().find(|t| t.oid == p.table)?;
            let text = |e: &Option<pgmem_ir::Expr>| e.as_ref().and_then(|e| crate::deparse::deparse_predicate(catalog, t, e).ok());
            Some(((**p).clone(), *t, text(&p.using), text(&p.with_check)))
        })
        .collect()
}

pub(super) fn pg_policy(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let all = policies(ctx);
    let catalog = ctx.txn.catalog();
    Ok(rows_of(PG_POLICY, |s| {
        all.into_iter()
            .map(|(p, _, qual, check)| {
                let roles = if p.roles.is_empty() {
                    vec![Value::Oid(0)]
                } else {
                    p.roles.iter().map(|r| Value::Oid(crate::rls::role_oid(catalog, &ctx.env.user, r))).collect()
                };
                s.row(vec![
                    ("oid", oid(p.oid)),
                    ("polname", name(&p.name)),
                    ("polrelid", oid(p.table)),
                    ("polcmd", ch(p.command)),
                    ("polpermissive", boolean(p.permissive)),
                    ("polroles", Value::Array(roles)),
                    ("polqual", qual.map_or(Value::Null, txt)),
                    ("polwithcheck", check.map_or(Value::Null, txt)),
                ])
            })
            .collect()
    }))
}

pub(super) fn pg_policies(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let all = policies(ctx);
    Ok(rows_of(PG_POLICIES, |s| {
        all.into_iter()
            .map(|(p, t, qual, check)| {
                let mut roles: Vec<String> = if p.roles.is_empty() { vec!["public".to_owned()] } else { p.roles.clone() };
                if !p.roles.is_empty() {
                    roles.sort();
                    roles.dedup();
                }
                s.row(vec![
                    ("schemaname", name(&t.schema)),
                    ("tablename", name(&t.name)),
                    ("policyname", name(&p.name)),
                    ("permissive", txt(if p.permissive { "PERMISSIVE" } else { "RESTRICTIVE" })),
                    ("roles", Value::Array(roles.iter().map(|r| name(r)).collect())),
                    ("cmd", txt(crate::rls::command_name(p.command))),
                    ("qual", qual.map_or(Value::Null, txt)),
                    ("with_check", check.map_or(Value::Null, txt)),
                ])
            })
            .collect()
    }))
}
