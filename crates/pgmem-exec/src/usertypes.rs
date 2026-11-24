//! The bridge between catalog type definitions and the I/O-level [`Registry`] that the
//! session installs for each statement: user types plus the row types of tables.

use crate::catalog_views::rowtype_oid_of;
use pgmem_catalog::{Catalog, TypeDef, TypeDefKind};
use pgmem_types::usertype::{Field, Registry, UserKind, UserType};
use pgmem_types::{SqlError, SqlResult, Type, code};
use std::sync::Arc;

/// The user types visible in `catalog`, including every table's composite row type.
pub fn registry(catalog: &Catalog) -> Arc<Registry> {
    let user = catalog.types().map(|t| Arc::new(t.user_type()));
    let rows = catalog.tables().map(|t| {
        let fields = t.live_columns().map(|(_, c)| Field { name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod }).collect();
        Arc::new(UserType {
            oid: rowtype_oid_of(t.oid),
            array_oid: 0,
            name: t.name.clone(),
            kind: UserKind::Composite { fields, relid: t.oid },
        })
    });
    Arc::new(Registry::new(user.chain(rows)))
}

/// A user type (or a table's row type) by name.
pub fn lookup_type(catalog: &Catalog, name: &str) -> Option<Type> {
    if let Some(t) = catalog.type_by_name(name) {
        return Some(t.as_type());
    }
    // `_name` is a user type's array type.
    if let Some(elem) = name.strip_prefix('_').and_then(|n| catalog.type_by_name(n)) {
        return Some(Type::array_of(elem.as_type()));
    }
    catalog.table(name).map(|t| Type::Composite(rowtype_oid_of(t.oid)))
}

/// A user type definition by name, or 42704.
pub fn type_def<'c>(catalog: &'c Catalog, name: &str) -> SqlResult<&'c Arc<TypeDef>> {
    catalog.type_by_name(name).ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{name}\" does not exist")))
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
