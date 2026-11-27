//! The bridge between catalog type definitions and the I/O-level [`Registry`] that the
//! session installs for each statement: user types plus the row types of tables.

use crate::catalog_views::rowtype_oid_of;
use pgmem_catalog::{Catalog, TypeDef, TypeDefKind};
use pgmem_types::usertype::{Field, Registry, UserKind, UserType};
use pgmem_types::{SqlError, SqlResult, Type, code};
use std::collections::HashSet;
use std::sync::Arc;

/// The user types visible in `catalog`, including every table's composite row type, with the
/// enum labels the running transaction may not use yet.
pub fn registry(catalog: &Catalog, uncommitted_enum_values: &HashSet<u32>) -> Arc<Registry> {
    let user = catalog.types().map(|t| Arc::new(t.user_type()));
    let rows = catalog.tables().map(|t| {
        let fields = t.live_columns().map(|(_, c)| Field { name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod }).collect();
        Arc::new(UserType {
            oid: rowtype_oid_of(t.oid),
            array_oid: 0,
            name: t.name.clone(),
            schema: t.schema.clone(),
            kind: UserKind::Composite { fields, relid: t.oid },
        })
    });
    Arc::new(Registry::new(user.chain(rows)).with_uncommitted_enum_values(uncommitted_enum_values.iter().copied()))
}

/// A user type (or a table's row type) by possibly qualified name, through the search path.
pub fn lookup_type(catalog: &Catalog, name: &pgmem_ir::ObjectName) -> SqlResult<Option<Type>> {
    catalog.resolve(name, |schema, n| type_in(catalog, schema, n))
}

fn type_in(catalog: &Catalog, schema: &str, name: &str) -> Option<Type> {
    if let Some(t) = catalog.type_by_name(schema, name) {
        return Some(t.as_type());
    }
    // `_name` is a user type's array type.
    if let Some(elem) = name.strip_prefix('_').and_then(|n| catalog.type_by_name(schema, n)) {
        return Some(Type::array_of(elem.as_type()));
    }
    catalog.table(schema, name).map(|t| Type::Composite(rowtype_oid_of(t.oid)))
}

/// A user type definition by name, or 42704.
pub fn type_def(catalog: &Catalog, name: &pgmem_ir::ObjectName) -> SqlResult<Arc<TypeDef>> {
    catalog.lookup_type(name)?.ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{}\" does not exist", name.base())))
}

/// The fields of a composite type: a `CREATE TYPE ... AS (...)` type or a table row type.
pub fn composite_fields(oid: u32) -> Vec<Field> {
    pgmem_types::usertype::lookup(oid).map(|t| t.fields().to_vec()).unwrap_or_default()
}

/// A domain's immediate base type and typmod (which may itself be a domain).
pub fn domain_parent(catalog: &Catalog, oid: u32) -> Option<(Type, i32)> {
    match &catalog.type_by_oid(oid)?.kind {
        TypeDefKind::Domain { base, typmod, .. } => Some((base.clone(), *typmod)),
        _ => None,
    }
}
