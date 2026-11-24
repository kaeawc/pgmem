//! The `reg*` OID alias types: name ↔ OID resolution against the catalog (`regclassin`,
//! `regtypein`, ... and their output functions).

use crate::funcs::string::quote_ident;
use pgmem_catalog::{Catalog, PG_CATALOG_NAMESPACE_OID, PUBLIC_NAMESPACE_OID};
use pgmem_types::{RegKind, SqlError, SqlResult, Type, Value, code, text};

/// Namespaces pgmem knows, with Postgres' fixed OIDs (information_schema's varies per
/// cluster; any stable value will do).
const NAMESPACES: &[(&str, u32)] =
    &[("pg_catalog", PG_CATALOG_NAMESPACE_OID), ("public", PUBLIC_NAMESPACE_OID), ("pg_toast", 99), ("information_schema", 13_000)];

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

fn namespace_name(oid: u32) -> Option<&'static str> {
    NAMESPACES.iter().find(|(_, o)| *o == oid).map(|(n, _)| *n)
}

/// The output text of a reg* OID: the object's name, or the number when there is none.
pub fn reg_name(kind: RegKind, oid: u32, catalog: &Catalog) -> String {
    if oid == 0 {
        return "-".to_owned();
    }
    let name = match kind {
        RegKind::Class => catalog.relation_by_oid(oid).map(|r| quote_ident(r.name())),
        RegKind::Type => Type::from_oid(oid).map(|t| t.name()),
        RegKind::Namespace => namespace_name(oid).map(quote_ident),
        RegKind::Proc => None,
    };
    name.unwrap_or_else(|| oid.to_string())
}

fn lookup_class(s: &str, catalog: &Catalog) -> SqlResult<Value> {
    let parts = parse_qualified(s)?;
    let missing = || SqlError::undefined_table(s.trim());
    let name = match parts.as_slice() {
        [n] => n,
        [schema, n] if schema == "public" => n,
        [schema, _] if namespace_name_oid(schema).is_none() => {
            return Err(SqlError::new(code::INVALID_SCHEMA_NAME, format!("schema \"{schema}\" does not exist")));
        }
        _ => return Err(missing()),
    };
    let rel = catalog.relation(name).ok_or_else(missing)?;
    Ok(Value::Reg(RegKind::Class, rel.oid(), quote_ident(rel.name())))
}

fn namespace_name_oid(name: &str) -> Option<u32> {
    NAMESPACES.iter().find(|(n, _)| *n == name).map(|(_, o)| *o)
}

/// `regtypein`: a type name as `format_type` or SQL writes it (`integer`, `int4`,
/// `character varying(10)`, `text[]`, `pg_catalog.int8`).
fn lookup_type(s: &str) -> SqlResult<Value> {
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
    let mut ty = Type::from_sql_name(name).ok_or_else(missing)?;
    for _ in 0..dims {
        ty = Type::array_of(ty);
    }
    Ok(Value::Reg(RegKind::Type, ty.oid(), ty.name()))
}

fn lookup_namespace(s: &str) -> SqlResult<Value> {
    let parts = parse_qualified(s)?;
    let [name] = parts.as_slice() else {
        return Err(SqlError::new(code::INVALID_NAME, "invalid name syntax"));
    };
    let oid =
        namespace_name_oid(name).ok_or_else(|| SqlError::new(code::INVALID_SCHEMA_NAME, format!("schema \"{name}\" does not exist")))?;
    Ok(Value::Reg(RegKind::Namespace, oid, quote_ident(name)))
}

/// Converts a value to a reg* type (`'t'::regclass`, `16384::regclass`, `oid::regtype`).
pub fn to_reg(v: Value, kind: RegKind, catalog: &Catalog) -> SqlResult<Value> {
    let from_oid = |oid: u32| Value::Reg(kind, oid, reg_name(kind, oid, catalog));
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
                RegKind::Type => lookup_type(&s),
                RegKind::Namespace => lookup_namespace(&s),
                RegKind::Proc => {
                    let parts = parse_qualified(&s)?;
                    let name = parts.last().cloned().unwrap_or_default();
                    Ok(Value::Reg(RegKind::Proc, 0, name))
                }
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
        let name = |s: &str| match lookup_type(s) {
            Ok(Value::Reg(_, _, n)) => n,
            other => format!("{other:?}"),
        };
        assert_eq!(name("int4"), "integer");
        assert_eq!(name("character varying(10)"), "character varying");
        assert_eq!(name("text[]"), "text[]");
        assert_eq!(name("pg_catalog.int8"), "bigint");
        assert!(lookup_type("nope").is_err());
    }
}
