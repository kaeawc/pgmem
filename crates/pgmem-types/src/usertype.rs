//! User-defined types: enums, domains and composite types (including table row types).
//!
//! [`Type`] and [`Value`] identify a user type by OID only, so they stay small and their
//! identity survives `ALTER TYPE`. The definitions live in a [`Registry`] built from the
//! running transaction's catalog and installed with the session context (see
//! [`crate::context`]); text and binary I/O, ordering and type names look them up there, the
//! way Postgres consults the syscache.

use crate::{SqlError, SqlResult, Type, Value, code};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

/// `record`: the type of `ROW(...)` and of rows without a named composite type.
pub const RECORD_OID: u32 = 2249;
/// `record[]`.
pub const RECORD_ARRAY_OID: u32 = 2287;

/// One enum label. Labels are kept in sort order; `sort` is Postgres' `enumsortorder`.
#[derive(Clone, Debug, PartialEq)]
pub struct EnumLabel {
    pub oid: u32,
    pub sort: f32,
    pub label: String,
}

/// One attribute of a composite type.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: Type,
    pub typmod: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UserKind {
    /// Labels in sort order.
    Enum { labels: Vec<EnumLabel> },
    /// The constraints (CHECK expressions, DEFAULT) are catalog-level; the executor enforces
    /// them. Only what I/O needs is here.
    Domain { base: Type, typmod: i32, not_null: bool },
    /// `relid` is the table a row type belongs to, or the `pg_class` entry (relkind `c`) of a
    /// `CREATE TYPE ... AS (...)` type.
    Composite { fields: Vec<Field>, relid: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct UserType {
    pub oid: u32,
    /// The OID of `_name`, the type's array type.
    pub array_oid: u32,
    pub name: String,
    pub kind: UserKind,
}

impl UserType {
    pub fn label(&self, label_oid: u32) -> Option<&EnumLabel> {
        match &self.kind {
            UserKind::Enum { labels } => labels.iter().find(|l| l.oid == label_oid),
            _ => None,
        }
    }

    pub fn fields(&self) -> &[Field] {
        match &self.kind {
            UserKind::Composite { fields, .. } => fields,
            _ => &[],
        }
    }

    /// The `Type` this definition describes.
    pub fn as_type(&self) -> Type {
        match self.kind {
            UserKind::Enum { .. } => Type::Enum(self.oid),
            UserKind::Domain { .. } => Type::Domain(self.oid),
            UserKind::Composite { .. } => Type::Composite(self.oid),
        }
    }
}

/// The user types visible to a statement, by OID (and array OID).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Registry {
    types: HashMap<u32, Arc<UserType>>,
    /// Array type OID → element type OID.
    arrays: HashMap<u32, u32>,
}

impl Registry {
    pub fn new(types: impl IntoIterator<Item = Arc<UserType>>) -> Registry {
        let mut r = Registry::default();
        for t in types {
            if t.array_oid != 0 {
                r.arrays.insert(t.array_oid, t.oid);
            }
            r.types.insert(t.oid, t);
        }
        r
    }

    pub fn get(&self, oid: u32) -> Option<&Arc<UserType>> {
        self.types.get(&oid)
    }

    /// The element type OID of a user type's array type.
    pub fn array_element(&self, array_oid: u32) -> Option<u32> {
        self.arrays.get(&array_oid).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

/// Looks a user type up in the current context's registry.
pub fn lookup(oid: u32) -> Option<Arc<UserType>> {
    crate::context::with_types(|r| r.get(oid).cloned())
}

/// A user type's name as `format_type` prints it: quoted when it is not a plain lower-case
/// identifier, `???` when the type is not visible.
pub fn type_name(oid: u32) -> String {
    lookup(oid).map_or_else(|| "???".to_owned(), |t| quote_ident(&t.name))
}

/// A user type's `pg_type.typname`.
pub fn raw_type_name(oid: u32) -> String {
    lookup(oid).map_or_else(|| "???".to_owned(), |t| t.name.clone())
}

/// `quote_identifier` for type names (keywords aside).
fn quote_ident(s: &str) -> String {
    let plain = s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain { s.to_owned() } else { format!("\"{}\"", s.replace('"', "\"\"")) }
}

/// The OID of a user type's array type (0 when unknown).
pub fn array_oid(oid: u32) -> u32 {
    lookup(oid).map_or(0, |t| t.array_oid)
}

/// A user type or a user type's array type, by OID (for Parse parameter OIDs).
pub fn type_for_oid(oid: u32) -> Option<Type> {
    crate::context::with_types(|r| {
        if let Some(t) = r.get(oid) {
            return Some(t.as_type());
        }
        let elem = r.array_element(oid)?;
        r.get(elem).map(|t| Type::array_of(t.as_type()))
    })
}

/// A domain's base type and typmod, following domains over domains.
pub fn domain_base(oid: u32) -> Option<(Type, i32)> {
    let t = lookup(oid)?;
    match &t.kind {
        UserKind::Domain { base: Type::Domain(inner), typmod, .. } => {
            let (b, m) = domain_base(*inner)?;
            Some((b, if *typmod >= 0 { *typmod } else { m }))
        }
        UserKind::Domain { base, typmod, .. } => Some((base.clone(), *typmod)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------------------------

/// `enum_in`.
pub fn parse_enum(s: &str, type_oid: u32) -> SqlResult<Value> {
    let t = lookup(type_oid).ok_or_else(|| SqlError::internal(format!("cache lookup failed for type {type_oid}")))?;
    let UserKind::Enum { labels } = &t.kind else { return Err(SqlError::internal(format!("type {} is not an enum", t.name))) };
    labels
        .iter()
        .find(|l| l.label == s)
        .map(|l| Value::Enum(type_oid, l.oid))
        .ok_or_else(|| SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("invalid input value for enum {}: \"{s}\"", t.name)))
}

/// `enum_out`.
pub fn enum_label(type_oid: u32, label_oid: u32) -> SqlResult<String> {
    lookup(type_oid)
        .and_then(|t| t.label(label_oid).map(|l| l.label.clone()))
        .ok_or_else(|| SqlError::new(code::INVALID_BINARY_REPRESENTATION, format!("invalid internal value for enum: {label_oid}")))
}

/// `enum_cmp`: labels order by `enumsortorder`.
pub fn compare_enum(type_oid: u32, a: u32, b: u32) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let sort = |t: &UserType, l: u32| t.label(l).map_or(f32::MAX, |x| x.sort);
    match lookup(type_oid) {
        Some(t) => sort(&t, a).total_cmp(&sort(&t, b)),
        None => a.cmp(&b),
    }
}

// ---------------------------------------------------------------------------------------------
// Composite (record) text I/O, as rowtypes.c's record_in / record_out
// ---------------------------------------------------------------------------------------------

fn malformed(s: &str, detail: &str) -> SqlError {
    SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("malformed record literal: \"{s}\"")).with_detail(detail)
}

/// `record_in` for a named composite type.
pub fn parse_record(s: &str, type_oid: u32) -> SqlResult<Value> {
    let t = lookup(type_oid).ok_or_else(|| SqlError::internal(format!("cache lookup failed for type {type_oid}")))?;
    let fields = t.fields();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if chars.get(i) != Some(&'(') {
        return Err(malformed(s, "Missing left parenthesis."));
    }
    i += 1;
    let mut values = Vec::with_capacity(fields.len());
    for (n, field) in fields.iter().enumerate() {
        if n > 0 {
            // The previous column ended at a comma; anything else means too few columns.
            match chars.get(i) {
                Some(',') => i += 1,
                Some(')') => return Err(malformed(s, "Too few columns.")),
                _ => return Err(malformed(s, "Unexpected end of input.")),
            }
        }
        if matches!(chars.get(i), Some(',' | ')')) {
            values.push(Value::Null);
            continue;
        }
        let mut buf = String::new();
        let mut in_quote = false;
        loop {
            let Some(&c) = chars.get(i) else { return Err(malformed(s, "Unexpected end of input.")) };
            if !in_quote && (c == ',' || c == ')') {
                break;
            }
            i += 1;
            match c {
                '\\' => {
                    let Some(&next) = chars.get(i) else { return Err(malformed(s, "Unexpected end of input.")) };
                    buf.push(next);
                    i += 1;
                }
                '"' if !in_quote => in_quote = true,
                '"' if chars.get(i) == Some(&'"') => {
                    buf.push('"');
                    i += 1;
                }
                '"' => in_quote = false,
                other => buf.push(other),
            }
        }
        values.push(crate::text::decode(&buf, &field.ty)?);
    }
    match chars.get(i) {
        Some(')') => i += 1,
        Some(',') => return Err(malformed(s, "Too many columns.")),
        _ => return Err(malformed(s, "Unexpected end of input.")),
    }
    if chars[i..].iter().any(|c| !c.is_whitespace()) {
        return Err(malformed(s, "Junk after right parenthesis."));
    }
    Ok(Value::Record(type_oid, values))
}

/// `record_out`: fields that are empty or contain quotes, backslashes, parentheses, commas or
/// whitespace are double-quoted, with `"` and `\` doubled inside.
pub fn format_record(type_oid: u32, values: &[Value]) -> SqlResult<String> {
    let declared = lookup(type_oid);
    let mut out = String::from("(");
    for (n, v) in values.iter().enumerate() {
        if n > 0 {
            out.push(',');
        }
        if v.is_null() {
            continue;
        }
        let ty = declared.as_ref().and_then(|t| t.fields().get(n).map(|f| f.ty.clone())).unwrap_or_else(|| v.natural_type());
        let text = crate::text::encode(v, &ty)?;
        let quote = text.is_empty() || text.chars().any(|c| matches!(c, '"' | '\\' | '(' | ')' | ',') || c.is_whitespace());
        if quote {
            out.push('"');
            for c in text.chars() {
                if c == '"' || c == '\\' {
                    out.push(c);
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(&text);
        }
    }
    out.push(')');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{self, Context};

    fn registry() -> Arc<Registry> {
        let mood = UserType {
            oid: 20000,
            array_oid: 20001,
            name: "mood".into(),
            kind: UserKind::Enum {
                labels: vec![
                    EnumLabel { oid: 20002, sort: 1.0, label: "sad".into() },
                    EnumLabel { oid: 20004, sort: 1.5, label: "meh".into() },
                    EnumLabel { oid: 20003, sort: 2.0, label: "happy".into() },
                ],
            },
        };
        let address = UserType {
            oid: 20010,
            array_oid: 20011,
            name: "address".into(),
            kind: UserKind::Composite {
                fields: vec![
                    Field { name: "street".into(), ty: Type::Text, typmod: -1 },
                    Field { name: "zip".into(), ty: Type::Int4, typmod: -1 },
                ],
                relid: 0,
            },
        };
        Arc::new(Registry::new([Arc::new(mood), Arc::new(address)]))
    }

    #[test]
    fn enums_parse_and_order_by_sort_order() {
        let _g = context::enter(Context { types: registry(), ..Context::default() });
        assert_eq!(parse_enum("happy", 20000).ok(), Some(Value::Enum(20000, 20003)));
        let err = parse_enum("Happy", 20000).expect_err("case matters");
        assert_eq!((err.code, err.message.as_str()), ("22P02", "invalid input value for enum mood: \"Happy\""));
        assert_eq!(compare_enum(20000, 20004, 20003), Ordering::Less);
        assert_eq!(compare_enum(20000, 20002, 20004), Ordering::Less);
        assert_eq!(type_for_oid(20001), Some(Type::array_of(Type::Enum(20000))));
    }

    #[test]
    fn records_round_trip_like_postgres() {
        let _g = context::enter(Context { types: registry(), ..Context::default() });
        let v = parse_record("(\"Elm, Rd\",)", 20010).expect("parse");
        assert_eq!(v, Value::Record(20010, vec![Value::Text("Elm, Rd".into()), Value::Null]));
        assert_eq!(format_record(20010, &[Value::Text("Elm, Rd".into()), Value::Null]).ok().as_deref(), Some("(\"Elm, Rd\",)"));
        assert_eq!(format_record(20010, &[Value::Text("a\"b".into()), Value::Int4(1)]).ok().as_deref(), Some("(\"a\"\"b\",1)"));
        assert_eq!(format_record(20010, &[Value::Text(String::new()), Value::Int4(1)]).ok().as_deref(), Some("(\"\",1)"));
        let err = parse_record("(a,b)", 20010).expect_err("bad int");
        assert_eq!(err.message, "invalid input syntax for type integer: \"b\"");
        assert_eq!(parse_record("(a)", 20010).expect_err("few").detail(), Some("Too few columns."));
        assert_eq!(parse_record("(a,1,2)", 20010).expect_err("many").detail(), Some("Too many columns."));
        assert_eq!(parse_record("a,1", 20010).expect_err("paren").detail(), Some("Missing left parenthesis."));
    }
}
