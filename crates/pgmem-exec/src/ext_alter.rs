//! `ALTER EXTENSION`: UPDATE, SET SCHEMA, ADD / DROP of member objects, and the checks that
//! keep members from being dropped without their extension.

use crate::bind::Binder;
use crate::eval::StatementState;
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, ExtMember, ExtMemberClass, Oid, QualName, Relation, TypeDefKind};
use pgmem_ir::{self as ir, AlterExtensionAction, ExtensionMember, ObjectType};
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};

/// `plpgsql` is implicit: not in the catalog, with a fixed OID.
const PLPGSQL_OID: Oid = 13_558;

const VERSIONS: &str = include_str!("catalog_views/pg_extension_versions.tsv");

/// The versions of an extension Postgres 18 ships, oldest first, with whether it is relocatable.
fn versions(name: &str) -> Vec<(&'static str, bool)> {
    let mut v: Vec<_> = VERSIONS
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            (f.next()? == name).then_some((f.next()?, f.next()? == "t"))
        })
        .collect();
    v.sort_by_key(|(ver, _)| version_key(ver));
    v
}

fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// `extrelocatable`.
pub(crate) fn relocatable(name: &str) -> bool {
    versions(name).last().is_some_and(|(_, r)| *r)
}

fn notice_result(message: String) -> QueryResult {
    QueryResult {
        tag: "ALTER EXTENSION".into(),
        notices: vec![Notice { severity: "NOTICE", code: "00000", message }],
        ..Default::default()
    }
}

fn done() -> QueryResult {
    QueryResult { tag: "ALTER EXTENSION".into(), ..Default::default() }
}

/// The extension's `(oid, schema, version)`.
fn find(txn: &Txn, name: &str) -> SqlResult<(Oid, String, String)> {
    if name == "plpgsql" {
        return Ok((PLPGSQL_OID, "pg_catalog".into(), "1.0".into()));
    }
    txn.catalog()
        .extension(name)
        .map(|e| (e.oid, e.schema.clone(), e.version.clone()))
        .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("extension \"{name}\" does not exist")))
}

fn extension_name(cat: &Catalog, oid: Oid) -> String {
    if oid == PLPGSQL_OID {
        return "plpgsql".into();
    }
    cat.extensions().find(|e| e.oid == oid).map(|e| e.name.clone()).unwrap_or_default()
}

pub(crate) fn alter_extension(name: &str, action: &AlterExtensionAction, txn: &mut Txn) -> SqlResult<QueryResult> {
    let (oid, schema, version) = find(txn, name)?;
    match action {
        AlterExtensionAction::Update { version: target } => update(name, &version, target.as_deref(), txn),
        AlterExtensionAction::SetSchema(target) => set_schema(name, oid, &schema, target, txn),
        AlterExtensionAction::Member { add, member } => alter_member(name, oid, *add, member, txn),
    }
}

fn update(name: &str, current: &str, target: Option<&str>, txn: &mut Txn) -> SqlResult<QueryResult> {
    let default = crate::catalog_views::available_extension(name).map_or(current, |(_, v)| v);
    let target = target.unwrap_or(default);
    if target == current {
        return Ok(notice_result(format!("version \"{target}\" of extension \"{name}\" is already installed")));
    }
    let known = versions(name);
    let reachable = known.iter().any(|(v, _)| *v == target) && version_key(target) > version_key(current);
    if !reachable {
        return Err(SqlError::new(
            code::INVALID_PARAMETER_VALUE,
            format!("extension \"{name}\" has no update path from version \"{current}\" to version \"{target}\""),
        ));
    }
    if let Some(mut def) = txn.catalog().extension(name).cloned() {
        def.version = target.to_owned();
        txn.catalog_mut().put_extension(def);
    }
    Ok(done())
}

/// Where a member lives: its schema, for the SET SCHEMA check.
fn member_schema(cat: &Catalog, m: &ExtMember) -> Option<String> {
    match m.class {
        ExtMemberClass::Relation => cat.relation_by_oid(m.oid).map(|r| r.schema().to_owned()),
        ExtMemberClass::Function => cat.function_by_oid(m.oid).map(|f| f.schema.clone()),
        ExtMemberClass::Type => cat.type_by_oid(m.oid).map(|t| t.schema.clone()),
        ExtMemberClass::Schema => None,
    }
}

fn set_schema(name: &str, oid: Oid, current: &str, target: &str, txn: &mut Txn) -> SqlResult<QueryResult> {
    if !relocatable(name) || oid == PLPGSQL_OID {
        return Err(SqlError::feature_not_supported(format!("extension \"{name}\" does not support SET SCHEMA")));
    }
    if txn.catalog().schema(target).is_none() {
        return Err(pgmem_catalog::undefined_schema(target));
    }
    if target == current {
        return Ok(done());
    }
    let members = txn.catalog().members_of_extension(oid);
    for m in &members {
        let Some(schema) = member_schema(txn.catalog(), m) else { continue };
        if schema != current {
            let desc = describe(txn.catalog(), m);
            return Err(SqlError::feature_not_supported(format!("extension \"{name}\" does not support SET SCHEMA"))
                .with_detail(format!("{desc} is not in the extension's schema \"{current}\"")));
        }
    }
    // Tables first: they take their owned sequences along.
    let rank = |m: &ExtMember| if m.class == ExtMemberClass::Relation { 0 } else { 1 };
    let mut members = members;
    members.sort_by_key(rank);
    for m in members {
        move_member(&m, target, txn)?;
    }
    if let Some(mut def) = txn.catalog().extension(name).cloned() {
        def.schema = target.to_owned();
        txn.catalog_mut().put_extension(def);
    }
    Ok(done())
}

fn move_member(m: &ExtMember, target: &str, txn: &mut Txn) -> SqlResult<()> {
    match m.class {
        ExtMemberClass::Relation => {
            let Some(rel) = txn.catalog().relation_by_oid(m.oid) else { return Ok(()) };
            if rel.schema() == target {
                return Ok(());
            }
            let kind = match &rel {
                Relation::Table(t) if t.matview.is_some() => ObjectType::MaterializedView,
                Relation::Table(_) => ObjectType::Table,
                Relation::View(_) => ObjectType::View,
                _ => ObjectType::Sequence,
            };
            let name = ir::ObjectName(vec![rel.schema().to_owned(), rel.name().to_owned()]);
            crate::ddl::set_schema(kind, &name, false, target, txn).map(|_| ())
        }
        ExtMemberClass::Function => {
            if let Some(f) = txn.catalog().function_by_oid(m.oid).cloned() {
                let mut moved = (*f).clone();
                moved.schema = target.to_owned();
                txn.catalog_mut().put_function(moved);
            }
            Ok(())
        }
        ExtMemberClass::Type => {
            let Some(t) = txn.catalog().type_by_oid(m.oid).cloned() else { return Ok(()) };
            let name = ir::ObjectName(vec![t.schema.clone(), t.name.clone()]);
            crate::ddl::set_schema(ObjectType::Type, &name, false, target, txn).map(|_| ())
        }
        ExtMemberClass::Schema => Ok(()),
    }
}

/// True when an unqualified reference would find objects of `schema`.
fn on_search_path(cat: &Catalog, schema: &str) -> bool {
    cat.search_path().iter().any(|e| matches!(e, pgmem_catalog::PathEntry::Schema(s) if s == schema))
}

fn qualified(cat: &Catalog, schema: &str, name: &str) -> String {
    if on_search_path(cat, schema) { name.to_owned() } else { format!("{schema}.{name}") }
}

/// ruleutils-style object description (`table t1`, `function f(integer,text)`).
fn describe(cat: &Catalog, m: &ExtMember) -> String {
    match m.class {
        ExtMemberClass::Relation => match cat.relation_by_oid(m.oid) {
            Some(Relation::Table(t)) if t.matview.is_some() => {
                format!("materialized view {}", crate::ddl::display_name(cat, &t.qualified()))
            }
            Some(r) => format!("{} {}", r.kind_name(), crate::ddl::display_name(cat, &QualName::new(r.schema(), r.name()))),
            None => String::new(),
        },
        ExtMemberClass::Function => cat.function_by_oid(m.oid).map(|f| describe_function(cat, f)).unwrap_or_default(),
        ExtMemberClass::Type => cat.type_by_oid(m.oid).map(|t| format!("type {}", qualified(cat, &t.schema, &t.name))).unwrap_or_default(),
        ExtMemberClass::Schema => cat.schema_by_oid(m.oid).map(|s| format!("schema {}", s.name)).unwrap_or_default(),
    }
}

fn describe_function(cat: &Catalog, f: &pgmem_catalog::FunctionDef) -> String {
    let types: Vec<String> = f.input_types().iter().map(pgmem_types::Type::name).collect();
    format!("function {}({})", qualified(cat, &f.schema, &f.name), types.join(","))
}

/// The object a member clause names: its class, OID and description.
fn resolve(cat: &Catalog, member: &ExtensionMember) -> SqlResult<ExtMember> {
    let make = |class, oid| ExtMember { ext: 0, class, oid };
    match member {
        ExtensionMember::Table(n) | ExtensionMember::View(n) | ExtensionMember::MaterializedView(n) | ExtensionMember::Sequence(n) => {
            let rel = cat.lookup_relation(n)?.ok_or_else(|| SqlError::undefined_table(&n.to_string()))?;
            let (ok, what) = match (member, &rel) {
                (ExtensionMember::Table(_), Relation::Table(t)) => (t.matview.is_none(), "a table"),
                (ExtensionMember::MaterializedView(_), Relation::Table(t)) => (t.matview.is_some(), "a materialized view"),
                (ExtensionMember::View(_), Relation::View(_)) => (true, "a view"),
                (ExtensionMember::Sequence(_), Relation::Sequence(_)) => (true, "a sequence"),
                (ExtensionMember::Table(_), _) => (false, "a table"),
                (ExtensionMember::View(_), _) => (false, "a view"),
                (ExtensionMember::MaterializedView(_), _) => (false, "a materialized view"),
                _ => (false, "a sequence"),
            };
            if !ok {
                return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not {what}", n.base())));
            }
            Ok(make(ExtMemberClass::Relation, rel.oid()))
        }
        ExtensionMember::Type(n) | ExtensionMember::Domain(n) => {
            let t = cat
                .lookup_type(n)?
                .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{}\" does not exist", n.base())))?;
            if matches!(member, ExtensionMember::Domain(_)) && !matches!(t.kind, TypeDefKind::Domain { .. }) {
                return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a domain", n.base())));
            }
            Ok(make(ExtMemberClass::Type, t.oid))
        }
        ExtensionMember::Schema(s) => {
            let schema = cat.schema(s).ok_or_else(|| pgmem_catalog::undefined_schema(s))?;
            Ok(make(ExtMemberClass::Schema, schema.oid))
        }
        ExtensionMember::Function { name, args, kind } => {
            let f = crate::functions::find_routine(cat, name, args.as_deref())?;
            let sig = crate::functions::signature(&f.name, &f.input_types());
            if *kind == ir::RoutineKind::Function && f.procedure {
                return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{sig} is not a function")));
            }
            if *kind == ir::RoutineKind::Procedure && !f.procedure {
                return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{sig} is not a procedure")));
            }
            Ok(make(ExtMemberClass::Function, f.oid))
        }
    }
}

fn alter_member(name: &str, ext: Oid, add: bool, member: &ExtensionMember, txn: &mut Txn) -> SqlResult<QueryResult> {
    let m = resolve(txn.catalog(), member)?;
    let desc = describe(txn.catalog(), &m);
    let owner = txn.catalog().extension_of(m.oid);
    if add {
        if let Some(other) = owner {
            let other = extension_name(txn.catalog(), other);
            return Err(SqlError::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("{desc} is already a member of extension \"{other}\""),
            ));
        }
        txn.catalog_mut().add_extension_member(ext, m.class, m.oid);
    } else {
        if owner != Some(ext) {
            return Err(SqlError::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("{desc} is not a member of extension \"{name}\"")));
        }
        txn.catalog_mut().remove_extension_member(m.oid);
    }
    Ok(done())
}

/// Fails like Postgres when a DROP names an extension member: the extension requires it.
pub(crate) fn check_member_drop(stmt: &ir::Statement, cat: &Catalog) -> SqlResult<()> {
    let found: Vec<ExtMember> = match stmt {
        ir::Statement::Drop(d) => d.names.iter().filter_map(|n| dropped_object(cat, d.object_type, n)).collect(),
        ir::Statement::DropFunction { items, .. } => items
            .iter()
            .filter_map(|(n, ts)| crate::functions::find_routine(cat, n, ts.as_deref()).ok())
            .map(|f| ExtMember { ext: 0, class: ExtMemberClass::Function, oid: f.oid })
            .collect(),
        _ => return Ok(()),
    };
    for m in found {
        if let Some(ext) = cat.extension_of(m.oid) {
            let ext = extension_name(cat, ext);
            return Err(SqlError::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop {} because extension {ext} requires it", describe(cat, &m)),
            )
            .with_hint(format!("You can drop extension {ext} instead.")));
        }
    }
    Ok(())
}

/// The object `DROP <kind> name` would remove, when it is of that kind.
fn dropped_object(cat: &Catalog, kind: ObjectType, name: &ir::ObjectName) -> Option<ExtMember> {
    let member = |class, oid| Some(ExtMember { ext: 0, class, oid });
    match kind {
        ObjectType::Table | ObjectType::View | ObjectType::MaterializedView | ObjectType::Sequence => {
            let rel = cat.find_relation(name).ok()??;
            let matches_kind = match (&kind, &rel) {
                (ObjectType::Table, Relation::Table(t)) => t.matview.is_none(),
                (ObjectType::MaterializedView, Relation::Table(t)) => t.matview.is_some(),
                (ObjectType::View, Relation::View(_)) | (ObjectType::Sequence, Relation::Sequence(_)) => true,
                _ => false,
            };
            matches_kind.then(|| ExtMember { ext: 0, class: ExtMemberClass::Relation, oid: rel.oid() })
        }
        ObjectType::Type | ObjectType::Domain => {
            let t = cat.lookup_type(name).ok()??;
            member(ExtMemberClass::Type, t.oid)
        }
        ObjectType::Schema => member(ExtMemberClass::Schema, cat.schema(name.base())?.oid),
        _ => None,
    }
}

/// DROP EXTENSION removes the extension's member objects with it.
pub(crate) fn drop_members(ext: Oid, cascade: bool, txn: &mut Txn, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let members = txn.catalog().members_of_extension(ext);
    txn.catalog_mut().clear_extension_members(ext);
    // Views before tables, then sequences, routines, types and schemas.
    let order = |m: &ExtMember| -> u8 {
        match (m.class, txn.catalog().relation_by_oid(m.oid)) {
            (ExtMemberClass::Relation, Some(Relation::View(_))) => 0,
            (ExtMemberClass::Relation, Some(Relation::Table(t))) if t.matview.is_some() => 0,
            (ExtMemberClass::Relation, Some(Relation::Table(_))) => 1,
            (ExtMemberClass::Relation, _) => 2,
            (ExtMemberClass::Function, _) => 3,
            (ExtMemberClass::Type, _) => 4,
            (ExtMemberClass::Schema, _) => 5,
        }
    };
    let mut members: Vec<(u8, ExtMember)> = members.into_iter().map(|m| (order(&m), m)).collect();
    members.sort_by_key(|(r, _)| *r);
    for (_, m) in members {
        drop_member(&m, cascade, txn, env, state)?;
    }
    Ok(())
}

fn drop_member(m: &ExtMember, cascade: bool, txn: &mut Txn, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let cat = txn.catalog();
    let (object_type, name) = match m.class {
        ExtMemberClass::Relation => {
            let Some(rel) = cat.relation_by_oid(m.oid) else { return Ok(()) };
            let kind = match &rel {
                Relation::Table(t) if t.matview.is_some() => ObjectType::MaterializedView,
                Relation::Table(_) => ObjectType::Table,
                Relation::View(_) => ObjectType::View,
                _ => ObjectType::Sequence,
            };
            (kind, ir::ObjectName(vec![rel.schema().to_owned(), rel.name().to_owned()]))
        }
        ExtMemberClass::Function => {
            txn.catalog_mut().drop_comments(m.oid);
            txn.catalog_mut().drop_function(m.oid);
            return Ok(());
        }
        ExtMemberClass::Type => {
            let Some(t) = cat.type_by_oid(m.oid) else { return Ok(()) };
            (ObjectType::Type, ir::ObjectName(vec![t.schema.clone(), t.name.clone()]))
        }
        ExtMemberClass::Schema => {
            let Some(s) = cat.schema_by_oid(m.oid) else { return Ok(()) };
            (ObjectType::Schema, ir::ObjectName(vec![s.name.clone()]))
        }
    };
    let snapshot = txn.catalog().clone();
    let binder = Binder::new(&snapshot, &[]);
    let stmt = ir::Statement::Drop(Box::new(ir::Drop { object_type, names: vec![name], if_exists: true, cascade, concurrently: false }));
    crate::ddl::execute_ddl(&stmt, txn, &binder, env, state).map(|_| ())
}
