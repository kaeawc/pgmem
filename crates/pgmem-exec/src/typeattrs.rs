//! `ALTER TYPE ... {ADD | DROP | ALTER | RENAME} ATTRIBUTE` on composite types
//! (`AlterCompositeType` in `tablecmds.c`, which runs them as `ALTER TABLE` commands on the
//! type's relation, in passes: drops, then type changes, then additions).

use crate::bind::{Binder, resolve_type_name};
use crate::eval::StatementState;
use crate::fielddeps;
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, TypeDef, TypeDefKind};
use pgmem_ir::{self as ir, TypeAttributeAction as Action};
use pgmem_storage::Txn;
use pgmem_types::usertype::Field;
use pgmem_types::{SqlError, SqlResult, Type, code};

/// The composite type `name` for an attribute action: Postgres resolves it as a relation, so
/// an enum or domain is "no such relation" and a table or view "not a composite type".
pub(crate) fn composite(cat: &Catalog, name: &ir::ObjectName) -> SqlResult<TypeDef> {
    if let Some(t) = cat.lookup_type(name)?
        && matches!(t.kind, TypeDefKind::Composite { .. })
    {
        return Ok((*t).clone());
    }
    if cat.lookup_relation(name)?.is_some() {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a composite type", name.base())));
    }
    Err(SqlError::undefined_table(&name.to_string()))
}

fn fields_of(def: &TypeDef) -> &[Field] {
    match &def.kind {
        TypeDefKind::Composite { fields, .. } => fields,
        _ => &[],
    }
}

fn missing_column(attr: &str, ty: &str) -> SqlError {
    SqlError::undefined_column(&format!("\"{attr}\" of relation \"{ty}\""))
}

/// Replaces the type's field list (and the stored definition).
fn put_fields(txn: &mut Txn, def: &mut TypeDef, fields: Vec<Field>) {
    if let TypeDefKind::Composite { fields: f, .. } = &mut def.kind {
        *f = fields;
    }
    let key = pgmem_catalog::QualName::new(&def.schema, &def.name);
    txn.catalog_mut().replace_type(&key, def.clone());
}

/// An attribute's type, typmod and collation.
struct Resolved {
    ty: Type,
    typmod: i32,
    collation: Option<String>,
}

/// Resolves the type and `COLLATE` of an attribute being added or retyped; a composite type
/// cannot hold itself.
fn attribute_type(cat: &Catalog, def: &TypeDef, ty: &ir::TypeName, collation: Option<&str>) -> SqlResult<Resolved> {
    let (t, typmod) = resolve_type_name(cat, ty)?;
    let collation = collation.map(|c| crate::bind::ddl::column_collation(&t, c)).transpose()?.flatten();
    if crate::rowtype_deps::contains_rowtype(cat, &t, def.oid) {
        return Err(SqlError::new(
            code::INVALID_TABLE_DEFINITION,
            format!("composite type {} cannot be made a member of itself", def.name),
        ));
    }
    Ok(Resolved { ty: t, typmod, collation })
}

pub(crate) fn alter_attributes(
    name: &ir::ObjectName,
    actions: &[Action],
    txn: &mut Txn,
    binder: &Binder,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let def = composite(txn.catalog(), name)?;
    let ty_name = def.name.clone();
    let mut notices: Vec<Notice> = Vec::new();

    // Parse analysis and the prepare phase (all before anything changes): the types and
    // collations resolve and the attributes to retype exist.
    let mut resolved: Vec<Option<Resolved>> = Vec::with_capacity(actions.len());
    for a in actions {
        resolved.push(match a {
            Action::Add { ty, collation, .. } => Some(attribute_type(binder.catalog, &def, ty, collation.as_deref())?),
            Action::AlterType { name: attr, ty, collation } => {
                if !fields_of(&def).iter().any(|f| f.name == *attr) {
                    return Err(missing_column(attr, &ty_name));
                }
                Some(attribute_type(binder.catalog, &def, ty, collation.as_deref())?)
            }
            Action::Drop { .. } => None,
        });
    }
    // A type change would have to rewrite the values stored in other tables' columns, which
    // Postgres does not do for composite types.
    if actions.iter().any(|a| matches!(a, Action::AlterType { .. }))
        && let Some((table, column)) = crate::rowtype_deps::first_user(binder.catalog, def.oid)
    {
        return Err(SqlError::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("cannot alter type \"{ty_name}\" because column \"{table}.{column}\" uses it"),
        ));
    }

    // Execution: Postgres' passes.
    for a in actions {
        let Action::Drop { name: attr, if_exists, cascade } = a else { continue };
        drop_attribute(txn, name, attr, DropOptions { if_exists: *if_exists, cascade: *cascade }, &mut notices, env, state)?;
    }
    let mut retyped: Vec<&str> = Vec::new();
    for (a, r) in actions.iter().zip(&resolved) {
        let (Action::AlterType { name: attr, .. }, Some(r)) = (a, r) else { continue };
        let mut def = composite(txn.catalog(), name)?;
        let mut fields = fields_of(&def).to_vec();
        let Some(f) = fields.iter_mut().find(|f| f.name == *attr) else { return Err(missing_column(attr, &ty_name)) };
        if retyped.contains(&attr.as_str()) {
            return Err(SqlError::new(code::FEATURE_NOT_SUPPORTED, format!("cannot alter type of column \"{attr}\" twice")));
        }
        retyped.push(attr);
        *f = Field { name: attr.clone(), ty: r.ty.clone(), typmod: r.typmod, collation: r.collation.clone() };
        put_fields(txn, &mut def, fields);
    }
    for (a, r) in actions.iter().zip(&resolved) {
        let (Action::Add { name: attr, .. }, Some(r)) = (a, r) else { continue };
        let mut def = composite(txn.catalog(), name)?;
        let mut fields = fields_of(&def).to_vec();
        if fields.iter().any(|f| f.name == *attr) {
            return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{attr}\" of relation \"{ty_name}\" already exists")));
        }
        fields.push(Field { name: attr.clone(), ty: r.ty.clone(), typmod: r.typmod, collation: r.collation.clone() });
        put_fields(txn, &mut def, fields);
        // Stored values of the type get a NULL for the new attribute.
        crate::rowtype_deps::rewrite_stored(txn, def.oid, crate::rowtype_deps::FieldChange::Append);
    }
    Ok(QueryResult { tag: "ALTER TYPE".into(), notices, ..Default::default() })
}

/// `IF EXISTS` and `CASCADE` of a `DROP ATTRIBUTE`.
#[derive(Clone, Copy)]
struct DropOptions {
    if_exists: bool,
    cascade: bool,
}

fn drop_attribute(
    txn: &mut Txn,
    name: &ir::ObjectName,
    attr: &str,
    DropOptions { if_exists, cascade }: DropOptions,
    notices: &mut Vec<Notice>,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<()> {
    let def = composite(txn.catalog(), name)?;
    let ty_name = def.name.as_str();
    if !fields_of(&def).iter().any(|f| f.name == attr) {
        if if_exists {
            notices.push(Notice {
                severity: "NOTICE",
                code: "00000",
                message: format!("column \"{attr}\" of relation \"{ty_name}\" does not exist, skipping"),
            });
            return Ok(());
        }
        return Err(missing_column(attr, ty_name));
    }
    // Expressions reading the attribute (CHECKs, indexes, generated columns, views) need CASCADE.
    let users = fielddeps::users(txn.catalog(), def.oid, attr);
    if !users.is_empty() {
        if !cascade {
            let lines: Vec<String> =
                users.iter().map(|u| format!("{} depends on column {attr} of composite type {ty_name}", u.describe())).collect();
            return Err(crate::ddl::dependents_error("column", &format!("{attr} of composite type {ty_name}"), lines.join("\n")));
        }
        let mut described = Vec::new();
        for u in users {
            described.push(u.describe());
            described.extend(fielddeps::drop_user(txn, u, env, state)?);
        }
        notices.push(crate::ddl::cascade_notice(&described));
    }
    let mut def = composite(txn.catalog(), name)?;
    let mut fields = fields_of(&def).to_vec();
    let Some(i) = fields.iter().position(|f| f.name == attr) else { return Ok(()) };
    fields.remove(i);
    put_fields(txn, &mut def, fields);
    if let TypeDefKind::Composite { relid, .. } = &def.kind {
        txn.catalog_mut().drop_attribute_comment(*relid, i32::try_from(i + 1).unwrap_or(i32::MAX));
    }
    crate::rowtype_deps::rewrite_stored(txn, def.oid, crate::rowtype_deps::FieldChange::Remove(i));
    Ok(())
}

/// `ALTER TYPE name RENAME ATTRIBUTE from TO to`.
pub(crate) fn rename_attribute(name: &ir::ObjectName, from: &str, to: &str, txn: &mut Txn) -> SqlResult<QueryResult> {
    let mut def = composite(txn.catalog(), name)?;
    let mut fields = fields_of(&def).to_vec();
    let Some(i) = fields.iter().position(|f| f.name == from) else {
        return Err(SqlError::undefined_column(&format!("\"{from}\"")));
    };
    if fields.iter().any(|f| f.name == to) {
        return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{to}\" of relation \"{}\" already exists", def.name)));
    }
    to.clone_into(&mut fields[i].name);
    fielddeps::rename(txn, def.oid, from, to);
    put_fields(txn, &mut def, fields);
    Ok(QueryResult { tag: "ALTER TYPE".into(), ..Default::default() })
}
