//! Objects reading one attribute of a composite type, as `(column).attribute`: they depend on
//! that attribute, so `ALTER TYPE ... DROP ATTRIBUTE` needs CASCADE for them and takes them
//! along with it. Detected for columns declared with the composite type itself (not through
//! arrays, domains or nested composites).

use crate::eval::StatementState;
use crate::session::SessionEnv;
use pgmem_catalog::{Catalog, ConstraintKind, Oid, QualName, TypeDefKind};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlResult, Type};

/// An object depending on an attribute of a composite type.
pub(crate) enum FieldUser {
    /// A CHECK constraint of a table.
    Check { table: QualName, name: String, oid: Oid },
    /// An index with an expression or predicate reading the attribute.
    Index { table: QualName, name: String, oid: Oid },
    /// A generated column computed from the attribute.
    Generated { table: QualName, column: String, table_oid: Oid },
    /// A view selecting it.
    View { view: QualName, oid: Oid },
    /// A CHECK constraint of a domain over the composite type.
    DomainCheck { domain: QualName, name: String, oid: Oid },
}

impl FieldUser {
    /// How `pg_depend` reporting names the object.
    pub(crate) fn describe(&self) -> String {
        match self {
            FieldUser::Check { table, name, .. } => format!("constraint {name} on table {}", table.name),
            FieldUser::Index { name, .. } => format!("index {name}"),
            FieldUser::Generated { table, column, .. } => format!("column {column} of table {}", table.name),
            FieldUser::View { view, .. } => format!("view {}", view.name),
            FieldUser::DomainCheck { name, .. } => format!("constraint {name}"),
        }
    }

    /// Creation order, the order the dependencies were recorded in.
    fn order(&self) -> Oid {
        match self {
            FieldUser::Check { oid, .. }
            | FieldUser::Index { oid, .. }
            | FieldUser::View { oid, .. }
            | FieldUser::DomainCheck { oid, .. } => *oid,
            FieldUser::Generated { table_oid, .. } => *table_oid,
        }
    }
}

/// Whether `e` reads `field` of one of `columns` (`(c).field`); `*` stands for every field.
fn reads(e: &ir::Expr, columns: &[&str], field: &str) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if let ir::Expr::FieldAccess { expr, field: f } = x
            && let ir::Expr::Column(parts) = &**expr
            && parts.last().is_some_and(|c| columns.contains(&c.as_str()))
            && (f == field || f == "*")
        {
            found = true;
        }
    });
    found
}

/// The objects reading attribute `field` of composite type `ty`.
pub(crate) fn users(cat: &Catalog, ty: Oid, field: &str) -> Vec<FieldUser> {
    let mut out = Vec::new();
    for t in cat.tables() {
        let columns: Vec<&str> = t.live_columns().filter(|(_, c)| c.ty == Type::Composite(ty)).map(|(_, c)| c.name.as_str()).collect();
        if columns.is_empty() {
            continue;
        }
        for c in &t.constraints {
            if matches!(&c.kind, ConstraintKind::Check { expr } if reads(expr, &columns, field)) {
                out.push(FieldUser::Check { table: t.qualified(), name: c.name.clone(), oid: c.oid });
            }
        }
        for i in &t.indexes {
            if i.exprs.iter().chain(i.predicate.iter()).any(|e| reads(e, &columns, field)) {
                out.push(FieldUser::Index { table: t.qualified(), name: i.name.clone(), oid: i.oid });
            }
        }
        for (_, c) in t.live_columns() {
            if c.generated.as_ref().is_some_and(|g| reads(g, &columns, field)) {
                out.push(FieldUser::Generated { table: t.qualified(), column: c.name.clone(), table_oid: t.oid });
            }
        }
        for v in crate::deps::dependent_views(cat, t.oid) {
            let refs = crate::views::references(&v.query);
            if refs.fields.iter().any(|(c, f)| columns.contains(&c.as_str()) && (f == field || f == "*"))
                && !out.iter().any(|u| matches!(u, FieldUser::View { oid, .. } if *oid == v.oid))
            {
                out.push(FieldUser::View { view: QualName::new(&v.schema, &v.name), oid: v.oid });
            }
        }
    }
    for d in cat.types() {
        if let TypeDefKind::Domain { base: Type::Composite(o), checks, .. } = &d.kind
            && *o == ty
        {
            for c in checks.iter().filter(|c| reads(&c.expr, &["value"], field)) {
                out.push(FieldUser::DomainCheck { domain: QualName::new(&d.schema, &d.name), name: c.name.clone(), oid: c.oid });
            }
        }
    }
    out.sort_by_key(FieldUser::order);
    out
}

/// Drops one of the objects `users` found (`DROP ... CASCADE` of it); returns the descriptions
/// of the further objects that went with it.
pub(crate) fn drop_user(txn: &mut Txn, user: FieldUser, env: &SessionEnv, state: &StatementState) -> SqlResult<Vec<String>> {
    let mut also = Vec::new();
    match user {
        FieldUser::Check { table, name, .. } => {
            let action = ir::AlterTableAction::DropConstraint { name, if_exists: true, cascade: true };
            crate::ddl::alter_table_action(txn, &table, &action, env, state)?;
        }
        FieldUser::Generated { table, column, .. } => {
            let action = ir::AlterTableAction::DropColumn { name: column, if_exists: true, cascade: true };
            crate::ddl::alter_table_action(txn, &table, &action, env, state)?;
        }
        FieldUser::Index { table, name, .. } => {
            let d = ir::Drop {
                object_type: ir::ObjectType::Index,
                names: vec![ir::ObjectName(vec![table.schema, name])],
                if_exists: true,
                cascade: true,
            };
            crate::ddl::drop_objects(&d, txn, env, state)?;
        }
        FieldUser::View { view, oid } => {
            // Views over the view go with it.
            let others = crate::deps::dependent_views_transitive(txn.catalog(), oid);
            for v in others {
                also.push(format!("view {}", v.name));
                txn.catalog_mut().drop_view(&QualName::new(&v.schema, &v.name));
            }
            txn.catalog_mut().drop_view(&view);
        }
        FieldUser::DomainCheck { domain, oid, .. } => {
            if let Some(t) = txn.catalog().type_by_name(&domain.schema, &domain.name).cloned() {
                let mut t = (*t).clone();
                if let TypeDefKind::Domain { checks, .. } = &mut t.kind {
                    checks.retain(|c| c.oid != oid);
                }
                txn.catalog_mut().replace_type(&domain, t);
                txn.catalog_mut().drop_comments(oid);
            }
        }
    }
    Ok(also)
}

/// Keeps the expressions reading attribute `from` of composite type `ty` pointing at it
/// after the rename to `to` (Postgres stores attribute numbers).
pub(crate) fn rename(txn: &mut Txn, ty: Oid, from: &str, to: &str) {
    let rename_in = |e: &mut ir::Expr, columns: &[String]| {
        e.walk_mut(&mut |x| {
            if let ir::Expr::FieldAccess { expr, field } = x
                && let ir::Expr::Column(parts) = &**expr
                && parts.last().is_some_and(|c| columns.contains(c))
                && field == from
            {
                to.clone_into(field);
            }
        });
    };
    let tables: Vec<_> = txn.catalog().tables().cloned().collect();
    for t in tables {
        let columns: Vec<String> = t.live_columns().filter(|(_, c)| c.ty == Type::Composite(ty)).map(|(_, c)| c.name.clone()).collect();
        if columns.is_empty() {
            continue;
        }
        let mut def = (*t).clone();
        for c in &mut def.columns {
            if let Some(g) = &mut c.generated {
                rename_in(g, &columns);
            }
        }
        for con in &mut def.constraints {
            if let ConstraintKind::Check { expr } = &mut con.kind {
                rename_in(expr, &columns);
            }
        }
        for i in &mut def.indexes {
            i.exprs.iter_mut().chain(i.predicate.iter_mut()).for_each(|e| rename_in(e, &columns));
        }
        let key = def.qualified();
        txn.catalog_mut().replace_table(&key, def);
    }
    let domains: Vec<_> = txn
        .catalog()
        .types()
        .filter(|d| matches!(&d.kind, TypeDefKind::Domain { base: Type::Composite(o), .. } if *o == ty))
        .cloned()
        .collect();
    for d in domains {
        let mut def = (*d).clone();
        if let TypeDefKind::Domain { checks, .. } = &mut def.kind {
            for c in checks {
                rename_in(&mut c.expr, &["value".to_owned()]);
            }
        }
        let key = QualName::new(&def.schema, &def.name);
        txn.catalog_mut().replace_type(&key, def);
    }
}
