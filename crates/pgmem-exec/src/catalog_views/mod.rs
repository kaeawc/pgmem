//! Virtual `pg_catalog` / `information_schema` relations, computed from the running
//! transaction's catalog and the session each time a statement reads them (no materialized
//! copy to drift).
//!
//! Each relation is a column spec (`"oid:oid, relname:name, relpages:int4=0"`, the column list
//! of the Postgres 18 relation with the value columns default to) and a function producing its
//! rows. Builtin data that never changes (builtin types, predefined roles) is recorded from
//! Postgres 18 in the `*.tsv` files next to this module.

mod information_schema;
mod model;
mod pg_catalog;
pub mod routines;

use crate::eval::EvalCtx;
use crate::plan::{Row, VirtualRowsFn};
use pgmem_types::{Type, Value, text};

pub use model::rowtype_oid as rowtype_oid_of;
pub use model::{
    ATTRDEF_CLASS_OID, CLASS_CLASS_OID, CONSTRAINT_CLASS_OID, INFORMATION_SCHEMA_NAMESPACE_OID, Model, NAMESPACE_CLASS_OID, PROC_CLASS_OID,
    TRIGGER_CLASS_OID, constraint_def, index_def, not_null_constraint_name, sequence_type,
};
pub use pg_catalog::{DomainCon, available_extension, domain_constraints, predefined_role};

pub struct VirtualTable {
    pub columns: Vec<(String, Type)>,
    pub rows: VirtualRowsFn,
}

/// A relation's columns with the value each takes when a row doesn't set it.
pub struct Spec(Vec<(String, Type, Value)>);

/// Types the spec strings use that pgmem stores as something simpler.
fn spec_type(name: &str) -> Type {
    let (base, array) = match name.strip_suffix("[]") {
        Some(b) => (b, true),
        None => (name, false),
    };
    let t = match base {
        "aclitem" | "pg_node_tree" | "anyarray" | "inet" | "pg_lsn" => Type::Text,
        "xid" | "cid" => Type::Oid,
        other => Type::from_sql_name(other).unwrap_or(Type::Text),
    };
    if array { Type::array_of(t) } else { t }
}

impl Spec {
    pub fn parse(spec: &str) -> Spec {
        Spec(
            spec.split(',')
                .map(|col| {
                    let col = col.trim();
                    let (decl, default) = match col.split_once('=') {
                        Some((d, v)) => (d, Some(v)),
                        None => (col, None),
                    };
                    let (name, ty) = decl.split_once(':').unwrap_or((decl, "text"));
                    let ty = spec_type(ty.trim());
                    let value = match default {
                        Some("~") => Value::Text(String::new()),
                        Some(v) => text::decode(v, &ty).unwrap_or(Value::Null),
                        None => Value::Null,
                    };
                    (name.trim().to_owned(), ty, value)
                })
                .collect(),
        )
    }

    pub fn columns(&self) -> Vec<(String, Type)> {
        self.0.iter().map(|(n, t, _)| (n.clone(), t.clone())).collect()
    }

    /// A row with the given columns set and every other column at its default.
    pub fn row(&self, values: Vec<(&str, Value)>) -> Row {
        let mut row: Row = self.0.iter().map(|(_, _, d)| d.clone()).collect();
        for (name, v) in values {
            if let Some(i) = self.0.iter().position(|(n, _, _)| n == name) {
                row[i] = v;
            } else {
                debug_assert!(false, "no column {name} in spec");
            }
        }
        row
    }
}

/// A virtual relation: schema, name, `pg_class` OID and kind, column spec, rows.
pub struct Def {
    pub schema: &'static str,
    pub name: &'static str,
    pub oid: u32,
    /// `pg_class.relkind`: `r` for catalog tables, `v` for views.
    pub kind: char,
    pub spec: &'static str,
    pub rows: VirtualRowsFn,
}

fn all_defs() -> impl Iterator<Item = &'static Def> {
    pg_catalog::DEFS.iter().chain(information_schema::DEFS.iter())
}

/// Looks up a virtual relation by schema and name.
pub fn lookup(schema: &str, name: &str) -> Option<VirtualTable> {
    all_defs().find(|d| d.schema == schema && d.name == name).map(|d| VirtualTable { columns: Spec::parse(d.spec).columns(), rows: d.rows })
}

/// A virtual relation by name in `pg_catalog` (what an unqualified catalog name resolves to)
/// or with an explicit schema.
pub fn system_relation(schema: Option<&str>, name: &str) -> Option<&'static Def> {
    all_defs().find(|d| d.name == name && schema.is_none_or(|s| s == d.schema) && (schema.is_some() || d.schema == "pg_catalog"))
}

pub fn system_relation_by_oid(oid: u32) -> Option<&'static Def> {
    all_defs().find(|d| d.oid == oid)
}

pub fn defs() -> impl Iterator<Item = &'static Def> {
    all_defs()
}

/// `pg_get_viewdef` of the system views, recorded from Postgres 18: `schema.name`, the
/// definition, the pretty definition (`\n`, `\t` and `\\` escaped).
const SYSTEM_VIEWDEFS: &str = include_str!("system_viewdefs.tsv");

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// The definition of a system view (`pg_get_viewdef`), by OID.
pub fn system_viewdef(oid: u32, pretty: bool) -> Option<String> {
    let d = system_relation_by_oid(oid).filter(|d| d.kind == 'v')?;
    let key = format!("{}.{}", d.schema, d.name);
    let line = SYSTEM_VIEWDEFS.lines().find(|l| l.split('\t').next() == Some(key.as_str()))?;
    let mut fields = line.split('\t').skip(1);
    let plain = fields.next()?;
    Some(unescape(if pretty { fields.next().unwrap_or(plain) } else { plain }))
}

// Value helpers for row builders.

pub(crate) fn oid(o: u32) -> Value {
    Value::Oid(o)
}

pub(crate) fn name(s: &str) -> Value {
    Value::Text(text::truncate_name(s))
}

pub(crate) fn txt(s: impl Into<String>) -> Value {
    Value::Text(s.into())
}

pub(crate) fn ch(c: char) -> Value {
    if c == '\0' { Value::Text(String::new()) } else { Value::Text(c.to_string()) }
}

pub(crate) fn int2(n: usize) -> Value {
    Value::Int2(i16::try_from(n).unwrap_or(i16::MAX))
}

pub(crate) fn int4(n: i64) -> Value {
    Value::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

pub(crate) fn boolean(b: bool) -> Value {
    Value::Bool(b)
}

pub(crate) fn yes_no(b: bool) -> Value {
    txt(if b { "YES" } else { "NO" })
}

pub(crate) fn rows_of(spec: &str, f: impl FnOnce(&Spec) -> Vec<Row>) -> Vec<Row> {
    f(&Spec::parse(spec))
}

/// The model of the running transaction's catalog.
pub(crate) fn snapshot<'a>(ctx: &'a EvalCtx) -> Model<'a> {
    Model::new(ctx.txn.catalog(), ctx.env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_defaults_and_types() {
        let s = Spec::parse("oid:oid, relname:name, relpages:int4=0, relkind:\"char\"=r, relacl:aclitem[], x:\"char\"=~");
        assert_eq!(s.columns()[3], ("relkind".to_owned(), Type::Char));
        let row = s.row(vec![("relname", name("t"))]);
        assert_eq!(row, vec![Value::Null, name("t"), Value::Int4(0), txt("r"), Value::Null, txt("")]);
    }

    #[test]
    fn every_relation_spec_parses_and_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for d in defs() {
            assert!(seen.insert((d.schema, d.name)), "duplicate {}.{}", d.schema, d.name);
            assert!(!Spec::parse(d.spec).columns().is_empty());
        }
    }
}
