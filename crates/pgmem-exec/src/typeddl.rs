//! CREATE / ALTER / DROP for enum, composite and domain types (Postgres' `typecmds.c` and
//! `pg_enum.c`), with the dependency checks of `DROP TYPE` (`dependency.c`).

use crate::bind::{Binder, Clause, Ctx, Level, Scope, resolve_type_name};
use crate::ddl::{alter_table_action, padded_rows};
use crate::eval::{EvalCtx, StatementState, eval};
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, DomainCheck, Oid, QualName, Relation, TypeDef, TypeDefKind, type_mentions};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::usertype::{EnumLabel, Field};
use pgmem_types::{SqlError, SqlResult, Type, Value, code};

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

fn notice(code: &'static str, message: String) -> Notice {
    Notice { severity: "NOTICE", code, message }
}

fn not_found(name: &str) -> SqlError {
    SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{name}\" does not exist"))
}

/// The longest enum label (NAMEDATALEN - 1 bytes).
const MAX_LABEL: usize = 63;

pub fn create_type(ct: &ir::CreateType, txn: &mut Txn, binder: &Binder) -> SqlResult<QueryResult> {
    let schema = binder.catalog.creation_schema(&ct.name)?;
    let name = ct.name.base().to_owned();
    let cat = txn.catalog_mut();
    let kind = match &ct.kind {
        ir::CreateTypeKind::Enum(labels) => {
            for l in labels {
                if l.len() > MAX_LABEL {
                    return Err(SqlError::new(code::INVALID_NAME, format!("invalid enum label \"{l}\""))
                        .with_detail(format!("Labels must be {MAX_LABEL} bytes or less.")));
                }
            }
            TypeDefKind::Enum { labels: Vec::new() }
        }
        ir::CreateTypeKind::Composite(attrs) => {
            let mut fields: Vec<Field> = Vec::with_capacity(attrs.len());
            for (n, t) in attrs {
                if fields.iter().any(|f| &f.name == n) {
                    return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{n}\" specified more than once")));
                }
                let (ty, typmod) = resolve_type_name(binder.catalog, t)?;
                fields.push(Field { name: n.clone(), ty, typmod });
            }
            TypeDefKind::Composite { fields, relid: 0 }
        }
    };
    // Array type OID first, as AssignTypeArrayOid does.
    let array_oid = cat.alloc_oid();
    let oid = cat.alloc_oid();
    let kind = match (kind, &ct.kind) {
        (TypeDefKind::Enum { .. }, ir::CreateTypeKind::Enum(labels)) => {
            let mut out: Vec<EnumLabel> = Vec::with_capacity(labels.len());
            for (i, l) in labels.iter().enumerate() {
                if out.iter().any(|x| &x.label == l) {
                    return Err(SqlError::new(
                        code::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"pg_enum_typid_label_index\"",
                    )
                    .with_detail(format!("Key (enumtypid, enumlabel)=({oid}, {l}) already exists."))
                    .with_constraint("pg_enum_typid_label_index"));
                }
                #[allow(clippy::cast_precision_loss)]
                out.push(EnumLabel { oid: cat.alloc_oid(), sort: (i + 1) as f32, label: l.clone() });
            }
            TypeDefKind::Enum { labels: out }
        }
        (TypeDefKind::Composite { fields, .. }, _) => TypeDefKind::Composite { fields, relid: cat.alloc_oid() },
        (other, _) => other,
    };
    cat.create_type(TypeDef { oid, array_oid, schema, name, kind })?;
    Ok(tag("CREATE TYPE"))
}

pub fn create_domain(cd: &ir::CreateDomain, txn: &mut Txn, binder: &Binder) -> SqlResult<QueryResult> {
    let schema = binder.catalog.creation_schema(&cd.name)?;
    let name = cd.name.base().to_owned();
    let (base, typmod) = resolve_type_name(binder.catalog, &cd.ty)?;
    if matches!(base, Type::Record | Type::Unknown | Type::Void) {
        return Err(SqlError::datatype_mismatch(format!("\"{}\" is not a valid base type for a domain", base.name())));
    }
    if let Some(d) = &cd.default {
        validate_default(binder, d, &base, typmod)?;
    }
    let mut checks: Vec<DomainCheck> = Vec::new();
    for (cname, expr) in &cd.checks {
        binder.bind_domain_check(expr, &base)?;
        let cname = match cname {
            Some(n) => {
                if checks.iter().any(|c| &c.name == n) {
                    return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("constraint \"{n}\" for domain \"{name}\" already exists")));
                }
                n.clone()
            }
            None => choose_check_name(&name, &checks),
        };
        checks.push(DomainCheck { oid: 0, name: cname, expr: expr.clone(), validated: true });
    }
    let cat = txn.catalog_mut();
    let array_oid = cat.alloc_oid();
    let oid = cat.alloc_oid();
    for c in &mut checks {
        c.oid = cat.alloc_oid();
    }
    let kind = TypeDefKind::Domain { base, typmod, not_null: cd.not_null, default: cd.default.clone(), checks };
    cat.create_type(TypeDef { oid, array_oid, schema, name, kind })?;
    Ok(tag("CREATE DOMAIN"))
}

/// `ChooseConstraintName(domain, NULL, "check", ...)`: `d_check`, `d_check1`, ...
fn choose_check_name(domain: &str, taken: &[DomainCheck]) -> String {
    pgmem_catalog::choose_object_name(domain, &[], "check", |n| !taken.iter().any(|c| c.name == n))
}

/// A domain DEFAULT must convert to the base type (it is checked against the domain's
/// constraints when used).
fn validate_default(binder: &Binder, d: &ir::Expr, base: &Type, typmod: i32) -> SqlResult<()> {
    let empty = Level::new(Scope::default());
    let ctx = Ctx::new(&empty).with_clause(Clause::Default);
    let (e, t) = binder.bind_expr(d, &ctx)?;
    crate::bind::coerce_assignment(binder, e, &t, base, typmod, "default")?;
    Ok(())
}

fn user_type(cat: &Catalog, name: &ir::ObjectName) -> SqlResult<TypeDef> {
    let base = name.base();
    if let Some(t) = cat.lookup_type(name)? {
        return Ok((*t).clone());
    }
    if matches!(cat.lookup_relation(name)?, Some(Relation::Table(_))) {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{base} is a table's row type")).with_hint("Use ALTER TABLE instead."));
    }
    Err(not_found(base))
}

/// Columns of tables declared with `oid` itself (not arrays or other types over it).
fn direct_columns(cat: &Catalog, oid: Oid) -> Vec<(QualName, usize)> {
    let mut out = Vec::new();
    for t in cat.tables() {
        for (i, c) in t.live_columns() {
            if c.ty == Type::Domain(oid) || c.ty == Type::Enum(oid) || c.ty == Type::Composite(oid) {
                out.push((t.qualified(), i));
            }
        }
    }
    out
}

fn rename_type(cat: &mut Catalog, mut def: TypeDef, new: &str) -> SqlResult<()> {
    let schema = def.schema.clone();
    if cat.type_by_name(&schema, new).is_some() || cat.table(&schema, new).is_some() || cat.view(&schema, new).is_some() {
        return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("type \"{new}\" already exists")));
    }
    let old = std::mem::replace(&mut def.name, new.to_owned());
    cat.replace_type(&QualName::new(&schema, &old), def);
    Ok(())
}

pub fn alter_type(at: &ir::AlterType, txn: &mut Txn, binder: &Binder) -> SqlResult<QueryResult> {
    let mut def = user_type(binder.catalog, &at.name)?;
    let name = def.name.clone();
    let mut notices = Vec::new();
    let not_enum = || SqlError::new(code::WRONG_OBJECT_TYPE, format!("{name} is not an enum"));
    let not_composite = || SqlError::new(code::WRONG_OBJECT_TYPE, format!("{name} is not a composite type"));
    let cat = txn.catalog_mut();
    match &at.action {
        ir::AlterTypeAction::AddValue { value, if_not_exists, neighbor } => {
            let TypeDefKind::Enum { labels } = &mut def.kind else { return Err(not_enum()) };
            if labels.iter().any(|l| &l.label == value) {
                if *if_not_exists {
                    notices.push(notice(code::DUPLICATE_OBJECT, format!("enum label \"{value}\" already exists, skipping")));
                    return Ok(QueryResult { tag: "ALTER TYPE".into(), notices, ..Default::default() });
                }
                return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("enum label \"{value}\" already exists")));
            }
            if value.len() > MAX_LABEL {
                return Err(SqlError::new(code::INVALID_NAME, format!("invalid enum label \"{value}\""))
                    .with_detail(format!("Labels must be {MAX_LABEL} bytes or less.")));
            }
            let pos = match neighbor {
                None => labels.len(),
                Some((before, n)) => {
                    let i = labels
                        .iter()
                        .position(|l| &l.label == n)
                        .ok_or_else(|| SqlError::new(code::INVALID_PARAMETER_VALUE, format!("\"{n}\" is not an existing enum label")))?;
                    if *before { i } else { i + 1 }
                }
            };
            insert_label(labels, pos, value.clone(), cat.alloc_oid());
        }
        ir::AlterTypeAction::RenameValue { from, to } => {
            let TypeDefKind::Enum { labels } = &mut def.kind else { return Err(not_enum()) };
            if labels.iter().any(|l| &l.label == to) && from != to {
                return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("enum label \"{to}\" already exists")));
            }
            let l = labels
                .iter_mut()
                .find(|l| &l.label == from)
                .ok_or_else(|| SqlError::new(code::INVALID_PARAMETER_VALUE, format!("\"{from}\" is not an existing enum label")))?;
            l.label.clone_from(to);
        }
        ir::AlterTypeAction::Rename(new) => {
            rename_type(cat, def, new)?;
            return Ok(tag("ALTER TYPE"));
        }
        ir::AlterTypeAction::AddAttribute { name: attr, ty } => {
            let (t, typmod) = resolve_type_name(binder.catalog, ty)?;
            let TypeDefKind::Composite { fields, .. } = &mut def.kind else { return Err(not_composite()) };
            if fields.iter().any(|f| &f.name == attr) {
                return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{attr}\" of relation \"{name}\" already exists")));
            }
            fields.push(Field { name: attr.clone(), ty: t, typmod });
        }
        ir::AlterTypeAction::DropAttribute { name: attr, if_exists } => {
            let in_use = !direct_columns(binder.catalog, def.oid).is_empty();
            let TypeDefKind::Composite { fields, .. } = &mut def.kind else { return Err(not_composite()) };
            let Some(i) = fields.iter().position(|f| &f.name == attr) else {
                if *if_exists {
                    notices.push(notice("00000", format!("column \"{attr}\" of relation \"{name}\" does not exist, skipping")));
                    return Ok(QueryResult { tag: "ALTER TYPE".into(), notices, ..Default::default() });
                }
                return Err(SqlError::undefined_column(&format!("\"{attr}\" of relation \"{name}\"")));
            };
            if in_use {
                return Err(SqlError::feature_not_supported(format!(
                    "dropping an attribute of type {name} while table columns use it is not supported by pgmem"
                )));
            }
            fields.remove(i);
        }
        ir::AlterTypeAction::RenameAttribute { from, to } => {
            let TypeDefKind::Composite { fields, .. } = &mut def.kind else { return Err(not_composite()) };
            if fields.iter().any(|f| &f.name == to) {
                return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{to}\" of relation \"{name}\" already exists")));
            }
            let f = fields
                .iter_mut()
                .find(|f| &f.name == from)
                .ok_or_else(|| SqlError::undefined_column(&format!("\"{from}\" of relation \"{name}\"")))?;
            f.name.clone_from(to);
        }
        ir::AlterTypeAction::NoOp => return Ok(tag("ALTER TYPE")),
    }
    let key = QualName::new(&def.schema, &name);
    cat.replace_type(&key, def);
    Ok(QueryResult { tag: "ALTER TYPE".into(), notices, ..Default::default() })
}

/// `AddEnumLabel`: the new label sorts between its neighbors (the midpoint of their sort
/// orders), after the last label as `last + 1`; when the midpoint is not representable the
/// labels are renumbered `1, 2, ...` first, as `RenumberEnumType` does.
fn insert_label(labels: &mut Vec<EnumLabel>, pos: usize, label: String, oid: Oid) {
    let sort_at = |labels: &[EnumLabel]| -> Option<f32> {
        match (pos.checked_sub(1).and_then(|p| labels.get(p)), labels.get(pos)) {
            (None, None) => Some(1.0),
            (Some(prev), None) => Some(prev.sort + 1.0),
            (None, Some(next)) => Some(next.sort - 1.0),
            (Some(prev), Some(next)) => {
                let mid = (prev.sort + next.sort) / 2.0;
                (mid > prev.sort && mid < next.sort).then_some(mid)
            }
        }
    };
    let sort = sort_at(labels).unwrap_or_else(|| {
        for (i, l) in labels.iter_mut().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            {
                l.sort = (i + 1) as f32;
            }
        }
        sort_at(labels).unwrap_or(0.0)
    });
    labels.insert(pos, EnumLabel { oid, sort, label });
}

pub fn alter_domain(
    ad: &ir::AlterDomain,
    txn: &mut Txn,
    binder: &Binder,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let base_name = ad.name.base().to_owned();
    let mut def = binder.catalog.lookup_type(&ad.name)?.map(|t| (*t).clone()).ok_or_else(|| not_found(&base_name))?;
    let name = def.name.clone();
    let oid = def.oid;
    if !matches!(def.kind, TypeDefKind::Domain { .. }) {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{name} is not a domain")));
    }
    match &ad.action {
        ir::AlterDomainAction::Rename(new) => {
            rename_type(txn.catalog_mut(), def, new)?;
            return Ok(tag("ALTER DOMAIN"));
        }
        ir::AlterDomainAction::NoOp => return Ok(tag("ALTER DOMAIN")),
        _ => {}
    }
    let TypeDefKind::Domain { base, not_null, default, checks, .. } = &mut def.kind else { return Ok(tag("ALTER DOMAIN")) };
    let base = base.clone();
    let mut notices = Vec::new();
    let missing = |c: &str| SqlError::new(code::UNDEFINED_OBJECT, format!("constraint \"{c}\" of domain \"{name}\" does not exist"));
    match &ad.action {
        ir::AlterDomainAction::SetDefault(e) => {
            validate_default(binder, e, &base, -1)?;
            *default = Some(e.clone());
        }
        ir::AlterDomainAction::DropDefault => *default = None,
        ir::AlterDomainAction::SetNotNull => {
            for (table, slot, v) in domain_values(txn, oid) {
                if v.is_null() {
                    return Err(column_error(
                        code::NOT_NULL_VIOLATION,
                        &table.name,
                        &txn_column(txn, &table, slot),
                        "contains null values",
                    ));
                }
            }
            *not_null = true;
        }
        ir::AlterDomainAction::DropNotNull => *not_null = false,
        ir::AlterDomainAction::AddCheck { name: cname, expr, not_valid } => {
            let bound = binder.bind_domain_check(expr, &base)?;
            let cname = match cname {
                Some(n) if checks.iter().any(|c| &c.name == n) => {
                    return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("constraint \"{n}\" for domain \"{name}\" already exists")));
                }
                Some(n) => n.clone(),
                None => choose_check_name(&name, checks),
            };
            if !not_valid {
                validate_check(txn, oid, &bound, env, state)?;
            }
            let coid = txn.catalog_mut().alloc_oid();
            checks.push(DomainCheck { oid: coid, name: cname, expr: expr.clone(), validated: !not_valid });
        }
        ir::AlterDomainAction::ValidateConstraint(c) => {
            let check = checks.iter_mut().find(|x| &x.name == c).ok_or_else(|| missing(c))?;
            let bound = binder.bind_domain_check(&check.expr, &base)?;
            validate_check(txn, oid, &bound, env, state)?;
            check.validated = true;
        }
        ir::AlterDomainAction::DropConstraint { name: c, if_exists } => {
            if *not_null && *c == pgmem_catalog::default_constraint_name(&name, &[], "not_null") {
                *not_null = false;
            } else if let Some(i) = checks.iter().position(|x| &x.name == c) {
                checks.remove(i);
            } else if *if_exists {
                notices.push(notice("00000", format!("constraint \"{c}\" of domain \"{name}\" does not exist, skipping")));
            } else {
                return Err(missing(c));
            }
        }
        ir::AlterDomainAction::RenameConstraint { from, to } => {
            if checks.iter().any(|x| &x.name == to) {
                return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("constraint \"{to}\" for domain \"{name}\" already exists")));
            }
            let check = checks.iter_mut().find(|x| &x.name == from).ok_or_else(|| missing(from))?;
            check.name.clone_from(to);
        }
        ir::AlterDomainAction::Rename(_) | ir::AlterDomainAction::NoOp => {}
    }
    let key = QualName::new(&def.schema, &name);
    txn.catalog_mut().replace_type(&key, def);
    Ok(QueryResult { tag: "ALTER DOMAIN".into(), notices, ..Default::default() })
}

fn txn_column(txn: &Txn, table: &QualName, slot: usize) -> String {
    txn.catalog().table_q(table).and_then(|t| t.columns.get(slot)).map(|c| c.name.clone()).unwrap_or_default()
}

fn column_error(code: &'static str, table: &str, column: &str, what: &str) -> SqlError {
    SqlError::new(code, format!("column \"{column}\" of table \"{table}\" {what}")).with_table(table).with_column(column)
}

/// Every stored value of a column declared with domain `oid`: (table, slot, value).
fn domain_values(txn: &Txn, oid: Oid) -> Vec<(QualName, usize, Value)> {
    let mut out = Vec::new();
    for (table, slot) in direct_columns(txn.catalog(), oid) {
        let Some(def) = txn.catalog().table_q(&table).cloned() else { continue };
        for (_, row) in padded_rows(txn, &def) {
            out.push((table.clone(), slot, row[slot].clone()));
        }
    }
    out
}

fn validate_check(txn: &Txn, oid: Oid, check: &crate::plan::Expr, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let ctx = EvalCtx::new(txn, &[], env, state);
    for (table, slot, v) in domain_values(txn, oid) {
        if matches!(eval(check, std::slice::from_ref(&v), &ctx)?, Value::Bool(false)) {
            return Err(column_error(
                code::CHECK_VIOLATION,
                &table.name,
                &txn_column(txn, &table, slot),
                "contains values that violate the new constraint",
            ));
        }
    }
    Ok(())
}

/// Something that depends on a type and goes away with `DROP TYPE ... CASCADE`.
#[derive(Clone, Debug)]
enum Dependent {
    /// A table column; `via_array` when it is declared with the type's array type.
    Column { table: QualName, table_oid: Oid, slot: usize, column: String, via_array: bool },
    /// A domain over the type.
    Domain { name: QualName, oid: Oid },
    /// An attribute of a composite type.
    Attribute { ty: QualName, oid: Oid, field: String },
    /// A user-defined cast from or to the type.
    Cast { oid: Oid, label: String },
}

impl Dependent {
    fn describe(&self) -> String {
        match self {
            Dependent::Column { table, column, .. } => format!("column {column} of table {}", table.name),
            Dependent::Domain { name, .. } => format!("type {}", name.name),
            Dependent::Attribute { ty, field, .. } => format!("column {field} of composite type {}", ty.name),
            Dependent::Cast { label, .. } => label.clone(),
        }
    }

    /// Creation order (OIDs are allocated in order), the order `pg_depend` lists them in.
    fn order(&self) -> (Oid, usize) {
        match self {
            Dependent::Column { table_oid, slot, .. } => (*table_oid, *slot),
            Dependent::Domain { oid, .. } | Dependent::Attribute { oid, .. } | Dependent::Cast { oid, .. } => (*oid, 0),
        }
    }
}

/// The objects that depend on type `oid`: those reached through its array type first, then
/// the direct ones, each in creation order (`findDependentObjects`' order).
fn dependents(cat: &Catalog, oid: Oid) -> Vec<Dependent> {
    let mut via_array = Vec::new();
    let mut direct = Vec::new();
    let mentions = |t: &Type| -> Option<bool> {
        match t {
            Type::Array(e) if type_mentions(e, oid) => Some(true),
            t if type_mentions(t, oid) => Some(false),
            _ => None,
        }
    };
    for t in cat.tables() {
        for (slot, c) in t.live_columns() {
            if let Some(arr) = mentions(&c.ty) {
                let d = Dependent::Column { table: t.qualified(), table_oid: t.oid, slot, column: c.name.clone(), via_array: arr };
                if arr { via_array.push(d) } else { direct.push(d) }
            }
        }
    }
    for t in cat.types() {
        match &t.kind {
            TypeDefKind::Domain { base, .. } if mentions(base).is_some() => {
                direct.push(Dependent::Domain { name: QualName::new(&t.schema, &t.name), oid: t.oid })
            }
            TypeDefKind::Composite { fields, .. } => {
                for f in fields.iter().filter(|f| mentions(&f.ty).is_some()) {
                    direct.push(Dependent::Attribute { ty: QualName::new(&t.schema, &t.name), oid: t.oid, field: f.name.clone() });
                }
            }
            _ => {}
        }
    }
    for c in cat.casts().filter(|c| type_mentions(&c.source, oid) || type_mentions(&c.target, oid)) {
        direct.push(Dependent::Cast { oid: c.oid, label: crate::usercast::describe(c) });
    }
    via_array.sort_by_key(Dependent::order);
    direct.sort_by_key(Dependent::order);
    via_array.extend(direct);
    via_array
}

pub fn drop_types(d: &ir::Drop, txn: &mut Txn, env: &SessionEnv, state: &StatementState) -> SqlResult<QueryResult> {
    let domain_only = d.object_type == ir::ObjectType::Domain;
    let tag_name = if domain_only { "DROP DOMAIN" } else { "DROP TYPE" };
    let mut notices = Vec::new();
    for name in &d.names {
        let base = name.base().to_owned();
        let Some(def) = txn.catalog().lookup_type(name)? else {
            if !domain_only && matches!(txn.catalog().lookup_relation(name)?, Some(Relation::Table(_))) {
                return Err(SqlError::new(
                    code::DEPENDENT_OBJECTS_STILL_EXIST,
                    format!("cannot drop type {base} because table {base} requires it"),
                )
                .with_hint(format!("You can drop table {base} instead.")));
            }
            if d.if_exists {
                notices.push(notice("00000", format!("type \"{base}\" does not exist, skipping")));
                continue;
            }
            return Err(not_found(&base));
        };
        if domain_only && !matches!(def.kind, TypeDefKind::Domain { .. }) {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is not a domain")));
        }
        let mut dropped = Vec::new();
        drop_one(txn, &def, d.cascade, &mut dropped, env, state)?;
        match dropped.as_slice() {
            [] => {}
            [one] => notices.push(notice("00000", format!("drop cascades to {one}"))),
            many => notices.push(notice("00000", format!("drop cascades to {} other objects", many.len()))),
        }
    }
    Ok(QueryResult { tag: tag_name.into(), notices, ..Default::default() })
}

fn drop_one(
    txn: &mut Txn,
    def: &TypeDef,
    cascade: bool,
    dropped: &mut Vec<String>,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<()> {
    let deps = dependents(txn.catalog(), def.oid);
    if !deps.is_empty() && !cascade {
        let detail = deps
            .iter()
            .map(|dep| {
                let target = match dep {
                    Dependent::Column { via_array: true, .. } => format!("{}[]", def.name),
                    _ => def.name.clone(),
                };
                format!("{} depends on type {target}", dep.describe())
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(SqlError::new(
            code::DEPENDENT_OBJECTS_STILL_EXIST,
            format!("cannot drop type {} because other objects depend on it", def.name),
        )
        .with_detail(detail)
        .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
    }
    for dep in deps {
        dropped.push(dep.describe());
        match dep {
            Dependent::Column { table, column, .. } => {
                let action = ir::AlterTableAction::DropColumn { name: column, if_exists: true, cascade: true };
                alter_table_action(txn, &table, &action, env, state)?;
            }
            Dependent::Domain { name, .. } => {
                if let Some(inner) = txn.catalog().type_by_name(&name.schema, &name.name).cloned() {
                    drop_one(txn, &inner, true, dropped, env, state)?;
                }
            }
            Dependent::Cast { oid, .. } => {
                txn.catalog_mut().drop_cast(oid);
            }
            Dependent::Attribute { ty, field, .. } => {
                if let Some(t) = txn.catalog().type_by_name(&ty.schema, &ty.name).cloned() {
                    let mut t = (*t).clone();
                    if let TypeDefKind::Composite { fields, .. } = &mut t.kind {
                        fields.retain(|f| f.name != field);
                    }
                    txn.catalog_mut().replace_type(&ty, t);
                }
            }
        }
    }
    txn.catalog_mut().drop_type(&QualName::new(&def.schema, &def.name));
    txn.catalog_mut().drop_comments(def.oid);
    Ok(())
}
