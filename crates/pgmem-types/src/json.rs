//! `jsonb`: a parsed JSON document normalized the way Postgres stores it — object keys are
//! de-duplicated (last value wins) and sorted shorter-first then bytewise, and the text output
//! uses Postgres' spacing (`{"a": 1, "b": [1, 2]}`).

use crate::{SqlError, SqlResult, code};
use serde_json::{Map, Number, Value as J};
use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub struct Jsonb(pub J);

impl Jsonb {
    pub fn parse(text: &str) -> SqlResult<Jsonb> {
        let v: J = serde_json::from_str(text).map_err(|e| {
            SqlError::new(code::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type json").with_detail(e.to_string())
        })?;
        Ok(Jsonb::from_value(v))
    }

    /// Wraps a serde value, normalizing object key order recursively.
    pub fn from_value(v: J) -> Jsonb {
        Jsonb(normalize(v))
    }

    pub fn null() -> Jsonb {
        Jsonb(J::Null)
    }

    pub fn string(s: impl Into<String>) -> Jsonb {
        Jsonb(J::String(s.into()))
    }

    pub fn number_i64(n: i64) -> Jsonb {
        Jsonb(J::Number(Number::from(n)))
    }

    pub fn number_f64(f: f64) -> Jsonb {
        Number::from_f64(f).map_or(Jsonb(J::Null), |n| Jsonb(J::Number(n)))
    }

    pub fn as_value(&self) -> &J {
        &self.0
    }

    /// `jsonb_typeof`.
    pub fn type_name(&self) -> &'static str {
        match self.0 {
            J::Null => "null",
            J::Bool(_) => "boolean",
            J::Number(_) => "number",
            J::String(_) => "string",
            J::Array(_) => "array",
            J::Object(_) => "object",
        }
    }

    /// `jsonb @> jsonb` containment, following Postgres' rules: objects contain sub-objects
    /// key by key, arrays contain arrays element-wise (order and duplicates ignored), and a
    /// top-level array contains a bare scalar it has as an element.
    pub fn contains(&self, other: &Jsonb) -> bool {
        contains(&self.0, &other.0, true)
    }
}

fn contains(l: &J, r: &J, top: bool) -> bool {
    match (l, r) {
        (J::Object(lm), J::Object(rm)) => rm.iter().all(|(k, rv)| lm.get(k).is_some_and(|lv| contains(lv, rv, false))),
        (J::Array(la), J::Array(ra)) => ra.iter().all(|rv| {
            la.iter().any(|lv| match (lv, rv) {
                (J::Array(_) | J::Object(_), J::Array(_) | J::Object(_)) => contains(lv, rv, false),
                _ => json_eq(lv, rv),
            })
        }),
        (J::Array(la), scalar) if top && !matches!(scalar, J::Object(_) | J::Array(_)) => la.iter().any(|lv| json_eq(lv, scalar)),
        _ => json_eq(l, r),
    }
}

fn json_eq(a: &J, b: &J) -> bool {
    cmp_json(a, b) == Ordering::Equal
}

fn normalize(v: J) -> J {
    match v {
        J::Object(map) => {
            let mut entries: Vec<(String, J)> = map.into_iter().map(|(k, v)| (k, normalize(v))).collect();
            entries.sort_by(|(a, _), (b, _)| key_order(a, b));
            let mut out = Map::with_capacity(entries.len());
            for (k, v) in entries {
                out.insert(k, v);
            }
            J::Object(out)
        }
        J::Array(items) => J::Array(items.into_iter().map(normalize).collect()),
        other => other,
    }
}

/// Postgres orders jsonb object keys by length first, then bytewise.
fn key_order(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

fn number_f64(n: &Number) -> f64 {
    n.as_f64().or_else(|| n.to_string().parse().ok()).unwrap_or(f64::NAN)
}

/// Postgres' jsonb btree order: Object > Array > Boolean > Number > String > Null.
fn cmp_json(a: &J, b: &J) -> Ordering {
    fn rank(v: &J) -> u8 {
        match v {
            J::Null => 0,
            J::String(_) => 1,
            J::Number(_) => 2,
            J::Bool(_) => 3,
            J::Array(_) => 4,
            J::Object(_) => 5,
        }
    }
    match (a, b) {
        (J::Null, J::Null) => Ordering::Equal,
        (J::String(x), J::String(y)) => x.as_bytes().cmp(y.as_bytes()),
        (J::Number(x), J::Number(y)) => crate::value::float_cmp(number_f64(x), number_f64(y)),
        (J::Bool(x), J::Bool(y)) => x.cmp(y),
        (J::Array(x), J::Array(y)) => x.len().cmp(&y.len()).then_with(|| {
            for (l, r) in x.iter().zip(y) {
                let o = cmp_json(l, r);
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        }),
        (J::Object(x), J::Object(y)) => x.len().cmp(&y.len()).then_with(|| {
            for ((lk, lv), (rk, rv)) in x.iter().zip(y) {
                let o = key_order(lk, rk).then_with(|| cmp_json(lv, rv));
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        }),
        _ => rank(a).cmp(&rank(b)),
    }
}

impl PartialOrd for Jsonb {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(cmp_json(&self.0, &other.0))
    }
}

impl Jsonb {
    pub fn total_cmp(&self, other: &Self) -> Ordering {
        cmp_json(&self.0, &other.0)
    }
}

impl fmt::Display for Jsonb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        write_json(&mut out, &self.0);
        f.write_str(&out)
    }
}

/// Writes a JSON value in Postgres' jsonb output style.
pub fn write_json(out: &mut String, v: &J) {
    match v {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Number(n) => out.push_str(&n.to_string()),
        J::String(s) => write_json_string(out, s),
        J::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json(out, item);
            }
            out.push(']');
        }
        J::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_json_string(out, k);
                out.push_str(": ");
                write_json(out, v);
            }
            out.push('}');
        }
    }
}

/// JSON string escaping as Postgres' `escape_json` does it.
pub fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_postgres() {
        let j = Jsonb::parse(r#"{"bb":1,"a":{"z":true,"y":null},"a":2}"#).expect("parse");
        assert_eq!(j.to_string(), r#"{"a": 2, "bb": 1}"#);
        let j = Jsonb::parse(r#"[1, "x", {"k":[]}]"#).expect("parse");
        assert_eq!(j.to_string(), r#"[1, "x", {"k": []}]"#);
    }

    #[test]
    fn containment() {
        let doc = Jsonb::parse(r#"{"a":1,"tags":["x","y"],"n":{"m":2,"o":3}}"#).expect("parse");
        assert!(doc.contains(&Jsonb::parse(r#"{"a":1}"#).expect("parse")));
        assert!(doc.contains(&Jsonb::parse(r#"{"tags":["y"]}"#).expect("parse")));
        assert!(doc.contains(&Jsonb::parse(r#"{"n":{"o":3}}"#).expect("parse")));
        assert!(!doc.contains(&Jsonb::parse(r#"{"a":2}"#).expect("parse")));
        let arr = Jsonb::parse(r#"["a","b"]"#).expect("parse");
        assert!(arr.contains(&Jsonb::parse(r#""a""#).expect("parse")));
    }
}
