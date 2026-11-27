//! Index access methods and operator classes: the built-in `pg_opclass` rows (recorded from
//! PostgreSQL 18 in `catalog_views/pg_opclass.tsv`) and the CREATE INDEX checks on them.

use pgmem_catalog::IndexOpclass;
use pgmem_types::{SqlError, SqlResult, Type, code};
use std::sync::OnceLock;

pub struct Opclass {
    pub oid: u32,
    pub method: u32,
    pub name: &'static str,
    pub intype: u32,
    pub default: bool,
}

fn rows() -> &'static [Opclass] {
    static ROWS: OnceLock<Vec<Opclass>> = OnceLock::new();
    ROWS.get_or_init(|| {
        include_str!("catalog_views/pg_opclass.tsv")
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                Some(Opclass {
                    oid: f.next()?.parse().ok()?,
                    method: f.next()?.parse().ok()?,
                    name: f.next()?,
                    intype: f.next()?.parse().ok()?,
                    default: f.next()? == "t",
                })
            })
            .collect()
    })
}

/// `pg_am.oid` of an index access method.
pub fn method_oid(name: &str) -> Option<u32> {
    Some(match name {
        "btree" => 403,
        "hash" => 405,
        "gist" => 783,
        "gin" => 2742,
        "spgist" => 4000,
        "brin" => 3580,
        _ => return None,
    })
}

/// The `opcintype` a key of type `ty` looks up (binary-coercible and polymorphic classes).
pub fn intype_oid(ty: &Type) -> u32 {
    match ty.base() {
        Type::Varchar => 25,
        Type::Cidr => 869,
        Type::Array(_) => 2277,
        Type::Enum(_) => 3500,
        Type::Range(_) => 3831,
        Type::Multirange(_) => 4537,
        Type::Composite(_) | Type::Record => 2249,
        other => other.oid(),
    }
}

/// The default operator class of `method` (a `pg_am` oid) for a key of type `ty`.
pub fn default_for(method: u32, ty: &Type) -> Option<&'static Opclass> {
    let intype = intype_oid(ty);
    rows().iter().find(|r| r.method == method && r.default && r.intype == intype)
}

/// What each access method supports: (unique, multicolumn, INCLUDE).
pub fn capabilities(method: &str) -> (bool, bool, bool) {
    match method {
        "btree" => (true, true, true),
        "gist" => (false, true, true),
        "spgist" => (false, false, true),
        "gin" | "brin" => (false, true, false),
        _ => (false, false, false),
    }
}

/// Resolves one index key's operator class like `ComputeIndexAttrs`. `lenient` is set when an
/// installed extension may provide classes this table lacks.
pub fn resolve(method: &str, ty: &Type, explicit: Option<&str>, lenient: bool) -> SqlResult<IndexOpclass> {
    let Some(m) = method_oid(method) else { return Ok(IndexOpclass { oid: 0, shown: None }) };
    let default = default_for(m, ty);
    let Some(name) = explicit else {
        return match default {
            Some(d) => Ok(IndexOpclass { oid: d.oid, shown: None }),
            None if lenient || (matches!(method, "btree" | "hash") && !matches!(ty.base(), Type::Json)) => {
                Ok(IndexOpclass { oid: 0, shown: None })
            }
            None => Err(SqlError::new(
                code::UNDEFINED_OBJECT,
                format!("data type {} has no default operator class for access method \"{method}\"", ty.base().name()),
            )
            .with_hint("You must specify an operator class for the index or define a default operator class for the data type.")),
        };
    };
    let named: Vec<&Opclass> = rows().iter().filter(|r| r.method == m && r.name == name).collect();
    if named.is_empty() {
        if lenient {
            return Ok(IndexOpclass { oid: 0, shown: Some(name.to_owned()) });
        }
        return Err(SqlError::new(
            code::UNDEFINED_OBJECT,
            format!("operator class \"{name}\" does not exist for access method \"{method}\""),
        ));
    }
    let intype = intype_oid(ty);
    let Some(row) = named.iter().find(|r| r.intype == intype) else {
        return Err(SqlError::new(
            code::DATATYPE_MISMATCH,
            format!("operator class \"{name}\" does not accept data type {}", ty.base().name()),
        ));
    };
    let shown = (default.map(|d| d.oid) != Some(row.oid)).then(|| name.to_owned());
    Ok(IndexOpclass { oid: row.oid, shown })
}
