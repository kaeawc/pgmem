//! A table's row type (and its array type) used by other objects: columns of other tables,
//! domains, attributes of composite types. They block DROP TABLE (CASCADE drops them), block
//! ALTER TABLE actions that would rewrite the table (`find_composite_type_dependencies`), and
//! follow ADD / DROP COLUMN in the values stored with the row type.

use crate::catalog_views::rowtype_oid_of;
use crate::typeddl::{Dependent, dependents};
use pgmem_catalog::{Catalog, Oid, QualName, TableDef, TypeDefKind};
use pgmem_ir as ir;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};

/// `cannot alter table "a" because column "b.v" uses its row type` (0A000) for the ALTER
/// TABLE actions Postgres refuses while the row type is stored elsewhere: ALTER COLUMN TYPE,
/// and ADD COLUMN of a column that needs filling in (a non-null default, identity,
/// generated, or a domain with constraints).
pub(crate) fn check_alter(cat: &Catalog, table: &TableDef, action: &ir::AlterTableAction) -> SqlResult<()> {
    let refused = match action {
        ir::AlterTableAction::AlterType { column, .. } => table.column_index(column).is_some(),
        ir::AlterTableAction::AddColumn { column, if_not_exists } => {
            !(*if_not_exists && table.column_index(&column.name).is_some()) && needs_rewrite(cat, column)
        }
        _ => false,
    };
    if !refused {
        return Ok(());
    }
    match first_user(cat, rowtype_oid_of(table.oid)) {
        Some((user, column)) => Err(SqlError::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("cannot alter table \"{}\" because column \"{user}.{column}\" uses its row type", table.name),
        )),
        None => Ok(()),
    }
}

/// True if adding `column` fills it in for existing rows (Postgres rewrites the table).
fn needs_rewrite(cat: &Catalog, column: &ir::ColumnDef) -> bool {
    use ir::ColumnConstraintKind as K;
    let filled = column.constraints.iter().any(|c| match &c.kind {
        K::Default(e) => !matches!(e, ir::Expr::Literal(ir::Literal::Null)),
        K::Generated(_) | K::Identity { .. } => true,
        _ => false,
    });
    let serial = matches!(column.ty.name.as_str(), "serial" | "serial4" | "bigserial" | "serial8" | "smallserial" | "serial2")
        && column.ty.array_dims == 0;
    filled || serial || crate::bind::resolve_type_name(cat, &column.ty).is_ok_and(|(ty, _)| domain_has_constraints(cat, &ty))
}

fn domain_has_constraints(cat: &Catalog, ty: &Type) -> bool {
    let Type::Domain(oid) = ty else { return false };
    match cat.type_by_oid(*oid).map(|t| &t.kind) {
        Some(TypeDefKind::Domain { base, not_null, checks, .. }) => *not_null || !checks.is_empty() || domain_has_constraints(cat, base),
        _ => false,
    }
}

/// The first table column (table name, column name) storing type `oid`, through its array
/// type, domains and composite types, in `pg_depend` order.
fn first_user(cat: &Catalog, oid: Oid) -> Option<(String, String)> {
    dependents(cat, oid).into_iter().find_map(|(dep, _)| match dep {
        Dependent::Column { table, column, .. } => Some((table.name, column)),
        Dependent::Domain { oid, .. } | Dependent::Attribute { oid, .. } => first_user(cat, oid),
        Dependent::Cast { .. } => None,
    })
}

/// DETAIL lines for DROP of relation `rel`: the objects using its row type, those of tables in
/// `dropping` (dropped by the same statement) aside.
pub(crate) fn drop_lines(cat: &Catalog, rel: &TableDef, dropping: &[QualName]) -> Vec<String> {
    let mut lines = Vec::new();
    crate::typeddl::dependent_lines(cat, rowtype_oid_of(rel.oid), &rel.name, dropping, &mut lines);
    lines
}

/// How ADD / DROP COLUMN changes the values of a row type.
#[derive(Clone, Copy)]
pub(crate) enum FieldChange {
    /// A column was added: values get a NULL field at the end.
    Append,
    /// The column at this position among the live columns was dropped.
    Remove(usize),
}

/// Rewrites the values of row type `rowtype` stored in other tables' columns (directly, in
/// arrays, domains or other composite values) after `change`.
pub(crate) fn rewrite_stored(txn: &mut pgmem_storage::Txn, rowtype: Oid, change: FieldChange) {
    let users: Vec<(Oid, Vec<(usize, Type)>)> = txn
        .catalog()
        .tables()
        .map(|t| {
            let slots = t
                .live_columns()
                .filter(|(_, c)| contains(txn.catalog(), &c.ty, rowtype, 0))
                .map(|(slot, c)| (slot, c.ty.clone()))
                .collect::<Vec<_>>();
            (t.oid, slots)
        })
        .filter(|(_, slots)| !slots.is_empty())
        .collect();
    for (table, slots) in users {
        let cat = txn.catalog().clone();
        txn.rewrite(table, |row| {
            for (slot, ty) in &slots {
                if let Some(v) = row.get_mut(*slot) {
                    rewrite_value(&cat, ty, v, rowtype, change, 0);
                }
            }
        });
    }
}

/// Nesting deeper than this is not followed (composite types cannot contain themselves).
const MAX_DEPTH: usize = 32;

/// True if values of `ty` can hold values of row type `rowtype`.
fn contains(cat: &Catalog, ty: &Type, rowtype: Oid, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match ty {
        Type::Composite(o) if *o == rowtype => true,
        Type::Composite(o) => crate::usertypes::composite_fields(*o).iter().any(|f| contains(cat, &f.ty, rowtype, depth + 1)),
        Type::Array(e) => contains(cat, e, rowtype, depth + 1),
        Type::Domain(o) => crate::usertypes::domain_parent(cat, *o).is_some_and(|(base, _)| contains(cat, &base, rowtype, depth + 1)),
        _ => false,
    }
}

fn rewrite_value(cat: &Catalog, ty: &Type, v: &mut Value, rowtype: Oid, change: FieldChange, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match (ty, v) {
        (_, Value::Null) => {}
        (Type::Composite(o), Value::Record(_, fields)) if *o == rowtype => match change {
            FieldChange::Append => fields.push(Value::Null),
            FieldChange::Remove(i) if i < fields.len() => {
                fields.remove(i);
            }
            FieldChange::Remove(_) => {}
        },
        (Type::Composite(o), Value::Record(_, fields)) => {
            for (f, v) in crate::usertypes::composite_fields(*o).iter().zip(fields.iter_mut()) {
                rewrite_value(cat, &f.ty, v, rowtype, change, depth + 1);
            }
        }
        (Type::Array(e), Value::Array(items) | Value::BoundedArray(_, items)) => {
            for v in items {
                rewrite_value(cat, e, v, rowtype, change, depth + 1);
            }
        }
        (Type::Domain(o), v) => {
            if let Some((base, _)) = crate::usertypes::domain_parent(cat, *o) {
                rewrite_value(cat, &base, v, rowtype, change, depth + 1);
            }
        }
        _ => {}
    }
}
