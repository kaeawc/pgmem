//! The `reg*` OID alias types: name ↔ OID resolution against the catalog (`regclassin`,
//! `regtypein`, ... and their output functions).

use crate::funcs::string::quote_ident;
use pgmem_catalog::{Catalog, PG_CATALOG, PG_CATALOG_NAMESPACE_OID, PUBLIC_NAMESPACE_OID, PathEntry};
use pgmem_types::{RegKind, SqlError, SqlResult, Type, Value, code, text};

/// Namespaces pgmem knows, with Postgres' fixed OIDs (information_schema's varies per
/// cluster; any stable value will do).
const NAMESPACES: &[(&str, u32)] = &[
    ("pg_catalog", PG_CATALOG_NAMESPACE_OID),
    ("public", PUBLIC_NAMESPACE_OID),
    ("pg_toast", 99),
    ("information_schema", crate::catalog_views::INFORMATION_SCHEMA_NAMESPACE_OID),
];

/// The session user's OID (the bootstrap superuser).
const BOOTSTRAP_ROLE_OID: u32 = 10;

/// Splits a possibly qualified, possibly quoted name (`public."My Table"`) into folded parts.
pub fn parse_qualified(s: &str) -> SqlResult<Vec<String>> {
    let invalid = || SqlError::new(code::INVALID_NAME, "invalid name syntax");
    let mut parts = Vec::new();
    let mut chars = s.trim().chars().peekable();
    loop {
        let mut part = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        part.push('"');
                    }
                    Some('"') => break,
                    Some(c) => part.push(c),
                    None => return Err(invalid()),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' {
                    break;
                }
                part.extend(c.to_lowercase());
                chars.next();
            }
            part = part.trim().to_owned();
        }
        if part.is_empty() {
            return Err(invalid());
        }
        parts.push(part);
        match chars.next() {
            Some('.') => {}
            None => return Ok(parts),
            Some(_) => return Err(invalid()),
        }
    }
}

/// Resolves a relation named in a string (`'app."My T"'`) through the search path.
pub fn relation_from_text(s: &str, catalog: &Catalog) -> SqlResult<Option<pgmem_catalog::Relation>> {
    let parts = parse_qualified(s)?;
    catalog.lookup_relation(&pgmem_ir::ObjectName(parts))
}

fn namespace_name(oid: u32) -> Option<&'static str> {
    NAMESPACES.iter().find(|(_, o)| *o == oid).map(|(n, _)| *n)
}

/// The output text of a reg* OID: the object's name, or the number when there is none.
pub fn reg_name(kind: RegKind, oid: u32, catalog: &Catalog, user: &str) -> String {
    if oid == 0 {
        return "-".to_owned();
    }
    let name = match kind {
        RegKind::Class => catalog
            .relation_by_oid(oid)
            .map(|r| class_display(catalog, &r))
            .or_else(|| crate::catalog_views::system_relation_by_oid(oid).map(|d| system_display(catalog, d))),
        RegKind::Type => Type::from_oid(oid).map(|t| t.name()),
        RegKind::Namespace => namespace_name(oid).map(quote_ident).or_else(|| catalog.schema_by_oid(oid).map(|sc| quote_ident(&sc.name))),
        RegKind::Role => role_name(catalog, oid, user),
        RegKind::Proc => proc_name(catalog, oid),
        RegKind::Procedure => procedure_name(catalog, oid),
    };
    name.unwrap_or_else(|| oid.to_string())
}

fn lookup_class(s: &str, catalog: &Catalog) -> SqlResult<Value> {
    let parts = parse_qualified(s)?;
    let missing = || SqlError::undefined_table(s.trim());
    let system = |d: &crate::catalog_views::Def| Value::Reg(RegKind::Class, d.oid, system_display(catalog, d));
    let user = |rel: &pgmem_catalog::Relation| Value::Reg(RegKind::Class, rel.oid(), class_display(catalog, rel));
    match parts.as_slice() {
        // An unqualified name takes the first match along the effective search path
        // (pg_catalog implicitly first), system relations included.
        [n] => {
            for e in catalog.search_path() {
                let schema = match &e {
                    PathEntry::System => PG_CATALOG,
                    PathEntry::Schema(s) => s.as_str(),
                };
                if let Some(d) = crate::catalog_views::system_relation(Some(schema), n) {
                    return Ok(system(d));
                }
                if let Some(rel) = catalog.relation(schema, n) {
                    return Ok(user(&rel));
                }
            }
            Err(missing())
        }
        [schema, n] if schema == "pg_catalog" || schema == "information_schema" => {
            crate::catalog_views::system_relation(Some(schema), n).map(system).ok_or_else(missing)
        }
        _ => catalog.find_relation(&pgmem_ir::ObjectName(parts))?.map(|rel| user(&rel)).ok_or_else(missing),
    }
}

/// `RelationIsVisible`: whether an unqualified name finds the relation `schema.name`, user
/// or system, along the effective search path.
pub fn relation_visible(catalog: &Catalog, schema: &str, name: &str) -> bool {
    catalog.relation_visible(schema, name, |s, n| crate::catalog_views::system_relation(Some(s), n).is_some())
}

/// `regclassout`: the name, schema-qualified unless the search path finds it by name.
fn class_display(catalog: &Catalog, rel: &pgmem_catalog::Relation) -> String {
    crate::deparse::relation_name(catalog, rel.schema(), rel.name())
}

/// How a system relation prints as regclass.
fn system_display(catalog: &Catalog, d: &crate::catalog_views::Def) -> String {
    crate::deparse::relation_name(catalog, d.schema, d.name)
}

/// `regtypein`: a type name as `format_type` or SQL writes it (`integer`, `int4`,
/// `character varying(10)`, `text[]`, `pg_catalog.int8`).
fn lookup_type(s: &str, catalog: &Catalog) -> SqlResult<Value> {
    let missing = || SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{}\" does not exist", s.trim()));
    let mut name = s.trim().to_ascii_lowercase();
    let mut dims = 0;
    while let Some(base) = name.strip_suffix("[]") {
        name = base.trim_end().to_owned();
        dims += 1;
    }
    if let Some(open) = name.find('(') {
        let close = name.rfind(')').ok_or_else(missing)?;
        name = format!("{}{}", name[..open].trim_end(), &name[close + 1..]);
    }
    let name = name.strip_prefix("pg_catalog.").unwrap_or(&name);
    let mut ty = match Type::from_sql_name(name) {
        Some(t) => t,
        // A user type: the name as written (quoted names keep their case), optionally public.
        None => {
            let mut written = s.trim();
            while let Some(base) = written.strip_suffix("[]") {
                written = base.trim_end();
            }
            let parts = parse_qualified(written)?;
            crate::usertypes::lookup_type(catalog, &pgmem_ir::ObjectName(parts))?.ok_or_else(missing)?
        }
    };
    for _ in 0..dims {
        ty = Type::array_of(ty);
    }
    Ok(Value::Reg(RegKind::Type, ty.oid(), ty.name()))
}

fn lookup_namespace(s: &str, catalog: &Catalog) -> SqlResult<Value> {
    let parts = parse_qualified(s)?;
    let [name] = parts.as_slice() else {
        return Err(SqlError::new(code::INVALID_NAME, "invalid name syntax"));
    };
    let oid = catalog.namespace_oid(name).ok_or_else(|| pgmem_catalog::undefined_schema(name))?;
    Ok(Value::Reg(RegKind::Namespace, oid, quote_ident(name)))
}

/// `regrolein`: a role name (one part; quoting preserves case).
fn lookup_role(s: &str, catalog: &Catalog, user: &str) -> SqlResult<Value> {
    let parts = parse_qualified(s)?;
    let [name] = parts.as_slice() else {
        return Err(SqlError::new(code::INVALID_NAME, "invalid name syntax"));
    };
    let oid = role_oid(catalog, user, name);
    let oid = oid.ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("role \"{name}\" does not exist")))?;
    Ok(Value::Reg(RegKind::Role, oid, quote_ident(name)))
}

/// A role's OID by name (the session user is the bootstrap superuser).
pub fn role_oid(catalog: &Catalog, user: &str, name: &str) -> Option<u32> {
    if name == user {
        return Some(BOOTSTRAP_ROLE_OID);
    }
    catalog.role(name).map(|r| r.oid).or_else(|| crate::catalog_views::predefined_role(name))
}

/// `regroleout`: the role's name, or its number once it is gone.
fn role_name(catalog: &Catalog, oid: u32, user: &str) -> Option<String> {
    if oid == BOOTSTRAP_ROLE_OID {
        return Some(quote_ident(user));
    }
    let name = catalog
        .roles()
        .find(|r| r.oid == oid)
        .map(|r| r.name.clone())
        .or_else(|| crate::catalog_views::predefined_role_name(oid).map(str::to_owned))?;
    Some(quote_ident(&name))
}

/// The functions (user ones and builtins) a possibly qualified name finds: (oid, argument
/// type OIDs).
fn functions_named(parts: &[String], catalog: &Catalog) -> Vec<(u32, Vec<u32>)> {
    let mut out: Vec<(u32, Vec<u32>)> = catalog
        .find_functions(&pgmem_ir::ObjectName(parts.to_vec()))
        .iter()
        .map(|f| (f.oid, f.input_types().iter().map(Type::oid).collect()))
        .collect();
    let builtin = match parts {
        [n] => Some(n),
        [schema, n] if schema == "pg_catalog" => Some(n),
        _ => None,
    };
    if let Some(n) = builtin {
        out.extend(crate::catalog_views::builtin_procs().iter().filter(|p| p.name == n).map(|p| (p.oid, p.args.clone())));
    }
    out
}

fn no_function(s: &str) -> SqlError {
    SqlError::new(code::UNDEFINED_FUNCTION, format!("function \"{}\" does not exist", s.trim()))
}

/// `regprocin`: a function name that must find exactly one function.
fn lookup_proc(s: &str, catalog: &Catalog) -> SqlResult<Value> {
    let parts = parse_qualified(s)?;
    match functions_named(&parts, catalog).as_slice() {
        [] => Err(no_function(s)),
        [(oid, _)] => Ok(Value::Reg(RegKind::Proc, *oid, reg_name(RegKind::Proc, *oid, catalog, ""))),
        _ => Err(SqlError::new(code::AMBIGUOUS_FUNCTION, format!("more than one function named \"{}\"", s.trim()))),
    }
}

/// `regprocedurein`: `name(type, ...)`.
fn lookup_procedure(s: &str, catalog: &Catalog) -> SqlResult<Value> {
    let text = s.trim();
    let open = text.find('(').ok_or_else(|| SqlError::new(code::INVALID_TEXT_REPRESENTATION, "expected a left parenthesis"))?;
    let close = text
        .rfind(')')
        .filter(|c| *c > open)
        .ok_or_else(|| SqlError::new(code::INVALID_TEXT_REPRESENTATION, "expected a right parenthesis"))?;
    let parts = parse_qualified(&text[..open])?;
    let inner = text[open + 1..close].trim();
    let mut args = Vec::new();
    if !inner.is_empty() {
        let mut depth = 0i32;
        let mut start = 0;
        let mut pieces = Vec::new();
        for (i, c) in inner.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    pieces.push(&inner[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        pieces.push(&inner[start..]);
        for p in pieces {
            let p = p.trim();
            // Pseudo-types (`"any"`, `anyelement`, `internal`) by their pg_type name.
            let pseudo = p.trim_matches('"').to_ascii_lowercase();
            let oid = match lookup_type(p, catalog) {
                Ok(Value::Reg(_, oid, _)) => oid,
                Ok(_) => 0,
                Err(e) => crate::catalog_views::builtin_type_name(|_, n| n == pseudo).map(|(oid, _, _)| oid).ok_or(e)?,
            };
            args.push(oid);
        }
    }
    let found = functions_named(&parts, catalog).into_iter().find(|(_, a)| *a == args).ok_or_else(|| no_function(s))?;
    Ok(Value::Reg(RegKind::Procedure, found.0, reg_name(RegKind::Procedure, found.0, catalog, "")))
}

/// `regprocout`: the function's name, schema-qualified unless that name alone finds exactly
/// this function (`FuncnameGetCandidates`).
fn proc_name(catalog: &Catalog, oid: u32) -> Option<String> {
    let (schema, name) = match catalog.function_by_oid(oid) {
        Some(f) => (f.schema.clone(), f.name.clone()),
        None => (PG_CATALOG.to_owned(), crate::catalog_views::builtin_procs().iter().find(|p| p.oid == oid)?.name.to_owned()),
    };
    let unique = matches!(path_candidates(catalog, &name).as_slice(), [(only, _)] if *only == oid);
    Some(if unique { quote_ident(&name) } else { format!("{}.{}", quote_ident(&schema), quote_ident(&name)) })
}

/// `FuncnameGetCandidates` for an unqualified name: (oid, argument type OIDs) of the functions
/// along the search path, a function hiding any later one with the same arguments.
fn path_candidates(catalog: &Catalog, name: &str) -> Vec<(u32, Vec<u32>)> {
    let temp = pgmem_types::context::search_path().temp_schema;
    let mut out: Vec<(u32, Vec<u32>)> = Vec::new();
    for e in catalog.search_path() {
        let found: Vec<(u32, Vec<u32>)> = match e {
            PathEntry::System => {
                crate::catalog_views::builtin_procs().iter().filter(|p| p.name == name).map(|p| (p.oid, p.args.clone())).collect()
            }
            PathEntry::Schema(s) if temp.as_deref() == Some(s.as_str()) => continue,
            PathEntry::Schema(s) => catalog
                .functions()
                .filter(|f| f.schema == s && f.name == name)
                .map(|f| (f.oid, f.input_types().iter().map(Type::oid).collect()))
                .collect(),
        };
        for c in found {
            if !out.iter().any(|(_, args)| *args == c.1) {
                out.push(c);
            }
        }
    }
    out
}

/// `regprocedureout` (`format_procedure`): `name(type,type)`, the name qualified unless
/// `FunctionIsVisible`.
fn procedure_name(catalog: &Catalog, oid: u32) -> Option<String> {
    let (name, args) = match catalog.function_by_oid(oid) {
        Some(f) => {
            let visible = catalog.function_visible(f, &crate::catalog_views::builtin_function_exists);
            let name = if visible { quote_ident(&f.name) } else { format!("{}.{}", quote_ident(&f.schema), quote_ident(&f.name)) };
            (name, f.input_types().iter().map(Type::oid).collect::<Vec<_>>())
        }
        None => {
            let p = crate::catalog_views::builtin_procs().iter().find(|p| p.oid == oid)?;
            let name =
                if builtin_visible(catalog, p.name, &p.args) { quote_ident(p.name) } else { format!("pg_catalog.{}", quote_ident(p.name)) };
            (name, p.args.clone())
        }
    };
    let types: Vec<String> =
        args.iter().map(|a| crate::funcs::catalog::format_type(*a, -1, catalog).unwrap_or_else(|| a.to_string())).collect();
    Some(format!("{name}({})", types.join(",")))
}

/// `FunctionIsVisible` for a pg_catalog function: no user function with the same signature
/// sits in a schema searched before pg_catalog (which is first unless listed later).
fn builtin_visible(catalog: &Catalog, name: &str, args: &[u32]) -> bool {
    let temp = pgmem_types::context::search_path().temp_schema;
    for e in catalog.search_path() {
        match e {
            PathEntry::System => return true,
            PathEntry::Schema(s) if temp.as_deref() == Some(s.as_str()) => {}
            PathEntry::Schema(s) => {
                let shadows = catalog
                    .functions()
                    .any(|f| f.schema == s && f.name == name && f.input_types().iter().map(Type::oid).eq(args.iter().copied()));
                if shadows {
                    return false;
                }
            }
        }
    }
    true
}

/// The text a `reg*` constant written `text` prints as in a deparsed expression. Postgres
/// stores the constant as an OID and prints it with the type's output function, so the name
/// follows the session's search path, not the one the text was written under: `text`
/// resolves under `def_path` (the path of the definition, when it is not the session's) and
/// prints under the session's. `None` when it names nothing (printed as written).
pub fn const_display(kind: RegKind, text: &str, catalog: &Catalog, user: &str, def_path: Option<&[String]>) -> Option<String> {
    let resolve = || to_reg(Value::Text(text.to_owned()), kind, catalog, user);
    let resolved = match def_path {
        Some(path) => pgmem_types::context::with_search_path(path, resolve),
        None => resolve(),
    };
    match resolved.ok()? {
        Value::Reg(_, oid, _) => Some(reg_name(kind, oid, catalog, user)),
        _ => None,
    }
}

/// Converts a value to a reg* type (`'t'::regclass`, `16384::regclass`, `oid::regtype`).
pub fn to_reg(v: Value, kind: RegKind, catalog: &Catalog, user: &str) -> SqlResult<Value> {
    let from_oid = |oid: u32| Value::Reg(kind, oid, reg_name(kind, oid, catalog, user));
    match v {
        Value::Null => Ok(Value::Null),
        Value::Oid(o) | Value::Reg(_, o, _) => Ok(from_oid(o)),
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => {
            let n = v.as_i64().unwrap_or_default();
            let oid = u32::try_from(n).or_else(|_| i32::try_from(n).map(i32::cast_unsigned)).map_err(|_| SqlError::out_of_range("OID"))?;
            Ok(from_oid(oid))
        }
        Value::Text(s) => {
            // All-digit input is an OID.
            if !s.trim().is_empty() && s.trim().bytes().all(|b| b.is_ascii_digit()) {
                return Ok(from_oid(text::parse_oid(&s)?));
            }
            if s.trim() == "-" {
                return Ok(Value::Reg(kind, 0, "-".to_owned()));
            }
            match kind {
                RegKind::Class => lookup_class(&s, catalog),
                RegKind::Type => lookup_type(&s, catalog),
                RegKind::Namespace => lookup_namespace(&s, catalog),
                RegKind::Role => lookup_role(&s, catalog, user),
                RegKind::Proc => lookup_proc(&s, catalog),
                RegKind::Procedure => lookup_procedure(&s, catalog),
            }
        }
        other => Err(SqlError::cannot_coerce(&other.natural_type().name(), &Type::Reg(kind).name())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_names_fold_and_unquote() {
        assert_eq!(parse_qualified("Public.\"My T\"").ok(), Some(vec!["public".to_owned(), "My T".to_owned()]));
        assert_eq!(parse_qualified("t").ok(), Some(vec!["t".to_owned()]));
        assert!(parse_qualified("a..b").is_err());
    }

    #[test]
    fn regtype_names() {
        let catalog = Catalog::new();
        let name = |s: &str| match lookup_type(s, &catalog) {
            Ok(Value::Reg(_, _, n)) => n,
            other => format!("{other:?}"),
        };
        assert_eq!(name("int4"), "integer");
        assert_eq!(name("character varying(10)"), "character varying");
        assert_eq!(name("text[]"), "text[]");
        assert_eq!(name("pg_catalog.int8"), "bigint");
        assert!(lookup_type("nope", &catalog).is_err());
    }
}
