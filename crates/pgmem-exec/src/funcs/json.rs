//! json and jsonb functions and operators.
//!
//! `jsonb` values are parsed documents ([`Jsonb`]); `json` values are the text as written
//! ([`Value::Json`]). Functions producing `json` format their output the way Postgres'
//! `json.c` does (`{"a" : 1}` from `json_build_object`, compact `{"a":1}` from `row_to_json`),
//! and operators on `json` return pieces of the input text verbatim.

use super::{Resolved, lenient, no_function, no_operator, resolved, strict};
use crate::eval::EvalCtx;
use crate::plan::Row;
use pgmem_types::datetime::{format_date, format_time, format_timestamp, format_timestamptz, format_timetz};
use pgmem_types::json::write_json_string;
use pgmem_types::jsontext::{self, Node};
use pgmem_types::usertype::{self, RECORD_OID};
use pgmem_types::{Jsonb, SqlError, SqlResult, Type, Value, code, text};
use serde_json::{Map, Number, Value as J};

// ---------------------------------------------------------------------------------------------
// SQL value → JSON
// ---------------------------------------------------------------------------------------------

/// Field names of a composite value: a named composite's attributes, a blessed record's
/// columns, or `f1, f2, ...` for an anonymous `ROW(...)`.
fn field_names(oid: u32, n: usize) -> Vec<String> {
    let fields: Vec<String> = if oid == RECORD_OID {
        Vec::new()
    } else {
        usertype::lookup(oid).map(|t| t.fields().iter().map(|f| f.name.clone()).collect()).unwrap_or_default()
    };
    if fields.len() == n { fields } else { (1..=n).map(|i| format!("f{i}")).collect() }
}

/// `JsonEncodeDateTime`: ISO 8601 with a `T` and a `+HH:MM` offset.
fn iso_datetime(v: &Value) -> Option<String> {
    let offset = |s: &str| -> String {
        // `... 12:00:00+05` → `+05:00`; `+05:30` and `+05:30:15` stay.
        let cut = s.rfind(['+', '-']).filter(|&i| i > 10);
        match cut {
            Some(i) if s[i + 1..].len() == 2 => format!("{}:00", s),
            Some(_) | None => s.to_owned(),
        }
    };
    Some(match v {
        Value::Date(d) => format_date(*d),
        Value::Timestamp(t) => format_timestamp(*t).replacen(' ', "T", 1),
        Value::Timestamptz(t) => offset(&format_timestamptz(*t).replacen(' ', "T", 1)),
        Value::Time(t) => format_time(*t),
        Value::TimeTz(t, z) => offset(&format_timetz(*t, *z)),
        _ => return None,
    })
}

fn number(text: &str) -> J {
    text.parse::<Number>().map_or_else(|_| J::String(text.to_owned()), J::Number)
}

/// Converts a SQL value to a JSON value the way `to_jsonb` does.
pub fn to_json_value(v: &Value) -> SqlResult<J> {
    Ok(match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => J::Number(Number::from(v.as_i64().unwrap_or(0))),
        Value::Float4(_) | Value::Float8(_) => {
            let s = text::encode(v, &v.natural_type())?;
            if v.as_f64().is_some_and(f64::is_finite) { number(&s) } else { J::String(s) }
        }
        Value::Numeric(n) => {
            if n.is_finite() {
                number(&n.to_string())
            } else {
                J::String(n.to_string())
            }
        }
        Value::Text(s) => J::String(s.clone()),
        Value::Json(s) => pgmem_types::json::parse_json(s)?,
        Value::Jsonb(j) => j.as_value().clone(),
        Value::Array(items) => J::Array(items.iter().map(to_json_value).collect::<SqlResult<Vec<_>>>()?),
        Value::Record(oid, fields) => {
            let names = field_names(*oid, fields.len());
            let mut m = Map::new();
            for (n, f) in names.into_iter().zip(fields) {
                m.insert(n, to_json_value(f)?);
            }
            J::Object(m)
        }
        other => match iso_datetime(other) {
            Some(s) => J::String(s),
            None => J::String(text::encode(other, &other.natural_type())?),
        },
    })
}

fn to_jsonb(v: &Value) -> SqlResult<Value> {
    Ok(Value::Jsonb(Jsonb::from_value(to_json_value(v)?)))
}

/// `datum_to_json`: the json text of a value. Composites and arrays print compactly, with
/// `",\n "` between items when `pretty`.
pub fn write_datum(out: &mut String, v: &Value, pretty: bool) -> SqlResult<()> {
    let sep = if pretty { ",\n " } else { "," };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => out.push_str(&v.as_i64().unwrap_or(0).to_string()),
        Value::Float4(_) | Value::Float8(_) => {
            let s = text::encode(v, &v.natural_type())?;
            if v.as_f64().is_some_and(f64::is_finite) { out.push_str(&s) } else { write_json_string(out, &s) }
        }
        Value::Numeric(n) => {
            if n.is_finite() {
                out.push_str(&n.to_string());
            } else {
                write_json_string(out, &n.to_string());
            }
        }
        Value::Text(s) => write_json_string(out, s),
        Value::Json(s) => out.push_str(s),
        Value::Jsonb(j) => out.push_str(&j.to_string()),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                write_datum(out, item, false)?;
            }
            out.push(']');
        }
        Value::Record(oid, fields) => {
            out.push('{');
            for (i, (name, f)) in field_names(*oid, fields.len()).iter().zip(fields).enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                write_json_string(out, name);
                out.push(':');
                write_datum(out, f, false)?;
            }
            out.push('}');
        }
        other => match iso_datetime(other) {
            Some(s) => write_json_string(out, &s),
            None => write_json_string(out, &text::encode(other, &other.natural_type())?),
        },
    }
    Ok(())
}

fn datum_json(v: &Value) -> SqlResult<String> {
    let mut out = String::new();
    write_datum(&mut out, v, false)?;
    Ok(out)
}

/// A JSON object key: the value's text output (NULL is an error).
fn key_of(v: &Value, position: usize) -> SqlResult<String> {
    match v {
        Value::Null => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("argument {position}: key must not be null"))),
        Value::Text(s) | Value::Json(s) => Ok(s.clone()),
        other => text::encode(other, &other.natural_type()),
    }
}

// ---------------------------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------------------------

fn jsonb(v: &Value) -> SqlResult<&Jsonb> {
    match v {
        Value::Jsonb(j) => Ok(j),
        other => Err(SqlError::internal(format!("expected jsonb, got {}", other.kind()))),
    }
}

fn json_text(v: &Value) -> SqlResult<&str> {
    match v {
        Value::Json(s) | Value::Text(s) => Ok(s),
        other => Err(SqlError::internal(format!("expected json, got {}", other.kind()))),
    }
}

fn wrap(j: J) -> Value {
    Value::Jsonb(Jsonb::from_value(j))
}

fn json_of(node: &Node) -> Value {
    Value::Json(node.text().to_owned())
}

/// Arguments of a variadic "any" function: unknown literals are text.
fn any_args(args: &[Type]) -> Vec<Type> {
    args.iter().map(|t| if *t == Type::Unknown { Type::Text } else { t.clone() }).collect()
}

fn is_json(t: &Type) -> bool {
    matches!(t, Type::Json)
}

/// The json-or-jsonb argument type of a `json_*` / `jsonb_*` function pair.
fn doc_type(name: &str, t: &Type) -> Type {
    if name.starts_with("jsonb") || *t == Type::Jsonb { Type::Jsonb } else { Type::Json }
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    if name.starts_with("jsonb_path_") {
        return resolve_path_function(name, args);
    }
    if let Some(r) = resolve_constructor(name, args)? {
        return Ok(Some(r));
    }
    match name {
        "jsonb_typeof" | "json_typeof" if n == 1 => {
            if is_json(&args[0]) {
                resolved(
                    strict("json_typeof", |a, _| Ok(Value::Text(jsontext::parse(json_text(&a[0])?)?.type_name().into()))),
                    vec![Type::Json],
                    Type::Text,
                )
            } else {
                resolved(
                    strict("jsonb_typeof", |a, _| Ok(Value::Text(jsonb(&a[0])?.type_name().to_owned()))),
                    vec![Type::Jsonb],
                    Type::Text,
                )
            }
        }
        "jsonb_array_length" | "json_array_length" if n == 1 => {
            let t = doc_type(name, &args[0]);
            resolved(strict("jsonb_array_length", array_length), vec![t], Type::Int4)
        }
        "jsonb_extract_path" | "json_extract_path" | "jsonb_extract_path_text" | "json_extract_path_text" if n >= 1 => {
            let doc = doc_type(name, &args[0]);
            let mut t = vec![Type::Text; n];
            t[0] = doc.clone();
            let as_text = name.ends_with("_text");
            let (f, ret) = match (doc == Type::Json, as_text) {
                (false, false) => (
                    strict("jsonb_extract_path", |a, _| Ok(path_get(jsonb(&a[0])?.as_value(), &a[1..]).map_or(Value::Null, wrap))),
                    Type::Jsonb,
                ),
                (false, true) => (
                    strict("jsonb_extract_path_text", |a, _| {
                        Ok(path_get(jsonb(&a[0])?.as_value(), &a[1..]).map_or(Value::Null, as_text_value))
                    }),
                    Type::Text,
                ),
                (true, false) => (strict("json_extract_path", |a, _| json_path(&a[0], &a[1..], false)), Type::Json),
                (true, true) => (strict("json_extract_path_text", |a, _| json_path(&a[0], &a[1..], true)), Type::Text),
            };
            resolved(f, t, ret)
        }
        "jsonb_set" if n == 3 || n == 4 => {
            let mut t = vec![Type::Jsonb, Type::array_of(Type::Text), Type::Jsonb];
            if n == 4 {
                t.push(Type::Bool);
            }
            resolved(strict("jsonb_set", jsonb_set), t, Type::Jsonb)
        }
        "jsonb_set_lax" if (3..=5).contains(&n) => {
            let mut t = vec![Type::Jsonb, Type::array_of(Type::Text), Type::Jsonb, Type::Bool, Type::Text];
            t.truncate(n);
            resolved(lenient("jsonb_set_lax", jsonb_set_lax), t, Type::Jsonb)
        }
        "jsonb_insert" if n == 3 || n == 4 => {
            let mut t = vec![Type::Jsonb, Type::array_of(Type::Text), Type::Jsonb];
            if n == 4 {
                t.push(Type::Bool);
            }
            resolved(strict("jsonb_insert", jsonb_insert), t, Type::Jsonb)
        }
        "jsonb_strip_nulls" | "json_strip_nulls" if n == 1 || n == 2 => {
            let mut t = vec![doc_type(name, &args[0])];
            if n == 2 {
                t.push(Type::Bool);
            }
            if t[0] == Type::Json {
                resolved(strict("json_strip_nulls", json_strip_nulls), t, Type::Json)
            } else {
                resolved(
                    strict("jsonb_strip_nulls", |a, _| {
                        Ok(wrap(strip_nulls(jsonb(&a[0])?.as_value(), a.get(1).and_then(Value::as_bool) == Some(true))))
                    }),
                    t,
                    Type::Jsonb,
                )
            }
        }
        "jsonb_pretty" if n == 1 => resolved(
            strict("jsonb_pretty", |a, _| {
                let mut out = String::new();
                pgmem_types::json::write_pretty(&mut out, jsonb(&a[0])?.as_value(), 0);
                Ok(Value::Text(out))
            }),
            vec![Type::Jsonb],
            Type::Text,
        ),
        "json" if n == 1 => resolved(
            strict("json", |a, _| {
                let s = a[0].as_str().unwrap_or("");
                pgmem_types::json::parse_json(s)?;
                Ok(Value::Json(s.to_owned()))
            }),
            vec![Type::Text],
            Type::Json,
        ),
        "json_serialize" if n == 1 => resolved(
            strict("json_serialize", |a, _| Ok(Value::Text(text::encode(&a[0], &a[0].natural_type())?))),
            vec![doc_type(name, &args[0])],
            Type::Text,
        ),
        "jsonb_object_keys"
        | "jsonb_each"
        | "jsonb_each_text"
        | "jsonb_array_elements"
        | "jsonb_array_elements_text"
        | "json_object_keys"
        | "json_each"
        | "json_each_text"
        | "json_array_elements"
        | "json_array_elements_text" => {
            let _ = err;
            Err(SqlError::feature_not_supported(format!(
                "{name}() must be used in FROM (set-returning functions in the select list are not supported by pgmem yet)"
            )))
        }
        _ => Ok(None),
    }
}

/// Functions building json / jsonb values from SQL values.
fn resolve_constructor(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    match name {
        "to_jsonb" | "to_json" | "json_scalar" if n == 1 => {
            if args[0] == Type::Unknown {
                return Err(SqlError::datatype_mismatch("could not determine polymorphic type because input has type unknown"));
            }
            if name == "to_jsonb" {
                resolved(strict("to_jsonb", |a, _| to_jsonb(&a[0])), any_args(args), Type::Jsonb)
            } else {
                resolved(strict("to_json", |a, _| Ok(Value::Json(datum_json(&a[0])?))), any_args(args), Type::Json)
            }
        }
        "row_to_json" if (n == 1 || n == 2) && matches!(args[0], Type::Record | Type::Composite(_)) => {
            let mut t = vec![args[0].clone()];
            if n == 2 {
                t.push(Type::Bool);
            }
            resolved(strict("row_to_json", row_to_json), t, Type::Json)
        }
        "array_to_json" if (n == 1 || n == 2) && matches!(args[0], Type::Array(_)) => {
            let mut t = vec![args[0].clone()];
            if n == 2 {
                t.push(Type::Bool);
            }
            resolved(strict("array_to_json", array_to_json), t, Type::Json)
        }
        "jsonb_build_object" | "json_build_object" | "__pgmem_json_object" => {
            if !n.is_multiple_of(2) {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "argument list must have even number of elements")
                    .with_hint(format!("The arguments of {name}() must consist of alternating keys and values.")));
            }
            if name == "jsonb_build_object" {
                resolved(lenient("jsonb_build_object", build_object_b), any_args(args), Type::Jsonb)
            } else {
                resolved(lenient("json_build_object", build_object), any_args(args), Type::Json)
            }
        }
        "jsonb_build_array" => resolved(lenient("jsonb_build_array", build_array_b), any_args(args), Type::Jsonb),
        "__pgmem_json_array_absent" => resolved(
            lenient("json_array", |a, c| {
                let kept: Vec<Value> = a.iter().filter(|v| !v.is_null()).cloned().collect();
                build_array(&kept, c)
            }),
            any_args(args),
            Type::Json,
        ),
        "__pgmem_json_object_absent" => {
            if !n.is_multiple_of(2) {
                return Err(err());
            }
            resolved(
                lenient("json_object", |a, c| {
                    let kept: Vec<Value> = a.chunks(2).filter(|p| !p[1].is_null()).flat_map(|p| p.to_vec()).collect();
                    build_object(&kept, c)
                }),
                any_args(args),
                Type::Json,
            )
        }
        "__pgmem_is_json" if n == 3 => {
            let input = match &args[0] {
                Type::Json | Type::Jsonb | Type::Bytea => args[0].clone(),
                t if t.is_string() || *t == Type::Unknown => Type::Text,
                t => {
                    return Err(SqlError::datatype_mismatch(format!("cannot use type {} in IS JSON predicate", t.name())));
                }
            };
            resolved(strict("is_json", is_json_fn), vec![input, Type::Text, Type::Bool], Type::Bool)
        }
        "json_build_array" | "__pgmem_json_array" => resolved(lenient("json_build_array", build_array), any_args(args), Type::Json),
        "json_object" | "jsonb_object" if n == 1 || n == 2 => {
            let t = vec![Type::array_of(Type::Text); n];
            if name == "json_object" {
                resolved(strict("json_object", json_object_fn), t, Type::Json)
            } else {
                resolved(strict("jsonb_object", jsonb_object_fn), t, Type::Jsonb)
            }
        }
        _ => Ok(None),
    }
}

/// Set-returning json/jsonb functions usable in FROM: (output column names/types, implementation).
pub fn resolve_srf(name: &str, args: &[Type]) -> Option<(Vec<(&'static str, Type)>, crate::plan::SrfFn)> {
    if args.len() != 1 || !matches!(args[0], Type::Jsonb | Type::Json | Type::Unknown) {
        return None;
    }
    let json = name.starts_with("json_");
    Some(match (name, json) {
        ("jsonb_object_keys", _) => (vec![("jsonb_object_keys", Type::Text)], object_keys),
        ("json_object_keys", _) => (vec![("json_object_keys", Type::Text)], json_object_keys),
        ("jsonb_each", _) => (vec![("key", Type::Text), ("value", Type::Jsonb)], each),
        ("json_each", _) => (vec![("key", Type::Text), ("value", Type::Json)], json_each),
        ("jsonb_each_text", _) => (vec![("key", Type::Text), ("value", Type::Text)], each_text),
        ("json_each_text", _) => (vec![("key", Type::Text), ("value", Type::Text)], json_each_text),
        ("jsonb_array_elements", _) => (vec![("value", Type::Jsonb)], array_elements),
        ("json_array_elements", _) => (vec![("value", Type::Json)], json_array_elements),
        ("jsonb_array_elements_text", _) => (vec![("value", Type::Text)], array_elements_text),
        ("json_array_elements_text", _) => (vec![("value", Type::Text)], json_array_elements_text),
        _ => return None,
    })
}

/// The argument type a json SRF wants (json functions keep json text).
pub fn srf_arg_type(name: &str) -> Type {
    if name.starts_with("json_") { Type::Json } else { Type::Jsonb }
}

// ---------------------------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------------------------

/// `json_build_object`: `{"k" : v, "k2" : v2}`.
fn build_object(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut out = String::from("{");
    for (i, pair) in a.chunks(2).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_json_string(&mut out, &key_of(&pair[0], i * 2 + 1)?);
        out.push_str(" : ");
        write_datum(&mut out, &pair[1], false)?;
    }
    out.push('}');
    Ok(Value::Json(out))
}

fn build_object_b(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut map = Map::new();
    for (i, pair) in a.chunks(2).enumerate() {
        map.insert(key_of(&pair[0], i * 2 + 1)?, to_json_value(&pair[1])?);
    }
    Ok(wrap(J::Object(map)))
}

/// `json_build_array`: `[1, "a", null]`.
fn build_array(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut out = String::from("[");
    for (i, v) in a.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_datum(&mut out, v, false)?;
    }
    out.push(']');
    Ok(Value::Json(out))
}

fn build_array_b(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(wrap(J::Array(a.iter().map(to_json_value).collect::<SqlResult<Vec<_>>>()?)))
}

fn row_to_json(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut out = String::new();
    write_datum(&mut out, &a[0], a.get(1).and_then(Value::as_bool) == Some(true))?;
    Ok(Value::Json(out))
}

fn array_to_json(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut out = String::new();
    write_datum(&mut out, &a[0], a.get(1).and_then(Value::as_bool) == Some(true))?;
    Ok(Value::Json(out))
}

/// The (key, value) pairs of `json_object(text[])` / `json_object(keys, values)`.
fn object_pairs(a: &[Value]) -> SqlResult<Vec<(String, Option<String>)>> {
    let items = |v: &Value| match v {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    };
    let text_or_null = |v: &Value| -> Option<String> { v.as_str().map(str::to_owned) };
    let null_key = || SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "null value not allowed for object key");
    if a.len() == 2 {
        let (keys, vals) = (items(&a[0]), items(&a[1]));
        if keys.len() != vals.len() {
            return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "mismatched array dimensions"));
        }
        return keys.iter().zip(&vals).map(|(k, v)| Ok((text_or_null(k).ok_or_else(null_key)?, text_or_null(v)))).collect();
    }
    let flat = items(&a[0]);
    if !flat.len().is_multiple_of(2) {
        return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "array must have even number of elements"));
    }
    flat.chunks(2).map(|kv| Ok((text_or_null(&kv[0]).ok_or_else(null_key)?, text_or_null(&kv[1])))).collect()
}

fn json_object_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut out = String::from("{");
    for (i, (k, v)) in object_pairs(a)?.into_iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_json_string(&mut out, &k);
        out.push_str(" : ");
        match v {
            Some(v) => write_json_string(&mut out, &v),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    Ok(Value::Json(out))
}

fn jsonb_object_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut map = Map::new();
    for (k, v) in object_pairs(a)? {
        map.insert(k, v.map_or(J::Null, J::String));
    }
    Ok(wrap(J::Object(map)))
}

/// `json_agg` output: `[1, 2]`, with `", \n "` before arrays and composites after the first.
pub fn json_agg_text(items: &[Value]) -> SqlResult<String> {
    let mut out = String::from("[");
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
            if matches!(v, Value::Array(_) | Value::Record(..)) {
                out.push_str("\n ");
            }
        }
        write_datum(&mut out, v, false)?;
    }
    out.push(']');
    Ok(out)
}

/// `json_object_agg` output: `{ "a" : 1, "b" : 2 }`.
pub fn json_object_agg_text(items: &[(Value, Value)]) -> SqlResult<String> {
    let mut out = String::from("{ ");
    for (i, (k, v)) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_json_string(&mut out, &key_of(k, 1)?);
        out.push_str(" : ");
        write_datum(&mut out, v, false)?;
    }
    out.push_str(" }");
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Set-returning functions
// ---------------------------------------------------------------------------------------------

fn obj<'a>(v: &'a Value, f: &str) -> SqlResult<&'a Map<String, J>> {
    match jsonb(v)?.as_value() {
        J::Object(m) => Ok(m),
        _ => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("cannot call {f} on a non-object"))),
    }
}

fn arr(v: &Value) -> SqlResult<&Vec<J>> {
    match jsonb(v)?.as_value() {
        J::Array(a) => Ok(a),
        J::Object(_) => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot extract elements from an object")),
        _ => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot extract elements from a scalar")),
    }
}

fn object_keys(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    Ok(obj(&a[0], "jsonb_object_keys")?.keys().map(|k| vec![Value::Text(k.clone())]).collect())
}

fn each(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    Ok(obj(&a[0], "jsonb_each")?.iter().map(|(k, v)| vec![Value::Text(k.clone()), wrap(v.clone())]).collect())
}

fn each_text(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    Ok(obj(&a[0], "jsonb_each_text")?.iter().map(|(k, v)| vec![Value::Text(k.clone()), as_text_value(v.clone())]).collect())
}

fn array_elements(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    Ok(arr(&a[0])?.iter().map(|v| vec![wrap(v.clone())]).collect())
}

fn array_elements_text(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    Ok(arr(&a[0])?.iter().map(|v| vec![as_text_value(v.clone())]).collect())
}

fn json_members<'a>(doc: &'a Node<'a>, f: &str) -> SqlResult<&'a [(String, Node<'a>)]> {
    match doc {
        Node::Object { members, .. } => Ok(members),
        Node::Array { .. } => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("cannot call {f} on an array"))),
        _ => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("cannot call {f} on a scalar"))),
    }
}

fn json_items<'a>(doc: &'a Node<'a>) -> SqlResult<&'a [Node<'a>]> {
    match doc {
        Node::Array { items, .. } => Ok(items),
        Node::Object { .. } => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot extract elements from an object")),
        _ => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot extract elements from a scalar")),
    }
}

fn json_object_keys(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    let doc = jsontext::parse(json_text(&a[0])?)?;
    Ok(json_members(&doc, "json_object_keys")?.iter().map(|(k, _)| vec![Value::Text(k.clone())]).collect())
}

fn json_each(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    let doc = jsontext::parse(json_text(&a[0])?)?;
    Ok(json_members(&doc, "json_each")?.iter().map(|(k, v)| vec![Value::Text(k.clone()), json_of(v)]).collect())
}

fn json_each_text(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    let doc = jsontext::parse(json_text(&a[0])?)?;
    Ok(json_members(&doc, "json_each_text")?
        .iter()
        .map(|(k, v)| vec![Value::Text(k.clone()), v.as_text().map_or(Value::Null, Value::Text)])
        .collect())
}

fn json_array_elements(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    let doc = jsontext::parse(json_text(&a[0])?)?;
    Ok(json_items(&doc)?.iter().map(|v| vec![json_of(v)]).collect())
}

fn json_array_elements_text(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    let doc = jsontext::parse(json_text(&a[0])?)?;
    Ok(json_items(&doc)?.iter().map(|v| vec![v.as_text().map_or(Value::Null, Value::Text)]).collect())
}

// ---------------------------------------------------------------------------------------------
// Access and modification
// ---------------------------------------------------------------------------------------------

fn array_length(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let len = match &a[0] {
        Value::Json(s) => match jsontext::parse(s)? {
            Node::Array { items, .. } => items.len(),
            Node::Object { .. } => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot get array length of a non-array")),
            _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot get array length of a scalar")),
        },
        other => match jsonb(other)?.as_value() {
            J::Array(items) => items.len(),
            J::Object(_) => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot get array length of a non-array")),
            _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot get array length of a scalar")),
        },
    };
    Ok(Value::Int4(i32::try_from(len).unwrap_or(i32::MAX)))
}

/// `->>` / `#>>` text rendering: strings unwrap, JSON null becomes SQL NULL.
fn as_text_value(j: J) -> Value {
    match j {
        J::Null => Value::Null,
        J::String(s) => Value::Text(s),
        other => Value::Text(Jsonb::from_value(other).to_string()),
    }
}

/// One step of `->`: object key, or array index (negative counts from the end).
fn step(doc: &J, key: &Value) -> Option<J> {
    match (doc, key) {
        (J::Object(m), Value::Text(k)) => m.get(k).cloned(),
        (J::Array(items), k) => {
            let i = match k {
                Value::Text(s) => s.parse::<i64>().ok()?,
                other => other.as_i64()?,
            };
            let len = i64::try_from(items.len()).ok()?;
            let idx = if i < 0 { len + i } else { i };
            usize::try_from(idx).ok().and_then(|i| items.get(i).cloned())
        }
        _ => None,
    }
}

fn path_get(doc: &J, path: &[Value]) -> Option<J> {
    let mut cur = doc.clone();
    for p in path {
        if p.is_null() {
            return None;
        }
        cur = step(&cur, p)?;
    }
    Some(cur)
}

fn json_step<'a>(doc: &'a Node<'a>, key: &Value) -> Option<&'a Node<'a>> {
    match (doc, key) {
        (Node::Object { .. }, Value::Text(k)) => doc.field(k),
        (Node::Array { .. }, Value::Text(s)) => doc.element(s.parse().ok()?),
        (Node::Array { .. }, k) => doc.element(k.as_i64()?),
        _ => None,
    }
}

/// `json #> path` / `json_extract_path`: the text of the value at a path.
fn json_path(doc: &Value, path: &[Value], as_text: bool) -> SqlResult<Value> {
    let root = jsontext::parse(json_text(doc)?)?;
    let mut cur = &root;
    for p in path {
        if p.is_null() {
            return Ok(Value::Null);
        }
        match json_step(cur, p) {
            Some(n) => cur = n,
            None => return Ok(Value::Null),
        }
    }
    Ok(if as_text { cur.as_text().map_or(Value::Null, Value::Text) } else { json_of(cur) })
}

fn path_values(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items.clone(),
        _ => Vec::new(),
    }
}

fn op_arrow(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(step(jsonb(&a[0])?.as_value(), &a[1]).map_or(Value::Null, wrap))
}

fn op_arrow_text(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(step(jsonb(&a[0])?.as_value(), &a[1]).map_or(Value::Null, as_text_value))
}

fn op_path(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(path_get(jsonb(&a[0])?.as_value(), &path_values(&a[1])).map_or(Value::Null, wrap))
}

fn op_path_text(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(path_get(jsonb(&a[0])?.as_value(), &path_values(&a[1])).map_or(Value::Null, as_text_value))
}

fn json_arrow(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    json_path(&a[0], &a[1..2], false)
}

fn json_arrow_text(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    json_path(&a[0], &a[1..2], true)
}

fn json_path_op(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    json_path(&a[0], &path_values(&a[1]), false)
}

fn json_path_text_op(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    json_path(&a[0], &path_values(&a[1]), true)
}

fn op_contains(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(jsonb(&a[0])?.contains(jsonb(&a[1])?)))
}

fn op_contained(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(jsonb(&a[1])?.contains(jsonb(&a[0])?)))
}

/// `?`: top-level key (objects) or string element (arrays) or the string itself.
fn has_key(doc: &J, key: &str) -> bool {
    match doc {
        J::Object(m) => m.contains_key(key),
        J::Array(items) => items.iter().any(|i| matches!(i, J::String(s) if s == key)),
        J::String(s) => s == key,
        _ => false,
    }
}

fn op_exists(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(has_key(jsonb(&a[0])?.as_value(), a[1].as_str().unwrap_or(""))))
}

fn op_exists_any(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let doc = jsonb(&a[0])?.as_value();
    Ok(Value::Bool(path_values(&a[1]).iter().any(|k| k.as_str().is_some_and(|k| has_key(doc, k)))))
}

fn op_exists_all(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let doc = jsonb(&a[0])?.as_value();
    Ok(Value::Bool(path_values(&a[1]).iter().all(|k| k.as_str().is_some_and(|k| has_key(doc, k)))))
}

fn op_concat(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (l, r) = (jsonb(&a[0])?.as_value().clone(), jsonb(&a[1])?.as_value().clone());
    Ok(wrap(match (l, r) {
        (J::Object(mut x), J::Object(y)) => {
            x.extend(y);
            J::Object(x)
        }
        (J::Array(mut x), J::Array(y)) => {
            x.extend(y);
            J::Array(x)
        }
        (J::Array(mut x), y) => {
            x.push(y);
            J::Array(x)
        }
        (x, J::Array(y)) => {
            let mut out = vec![x];
            out.extend(y);
            J::Array(out)
        }
        (x, y) => J::Array(vec![x, y]),
    }))
}

fn op_minus(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let doc = jsonb(&a[0])?.as_value().clone();
    Ok(wrap(match (&a[1], doc) {
        (Value::Text(k), J::Object(mut m)) => {
            m.shift_remove(k);
            J::Object(m)
        }
        (Value::Text(k), J::Array(items)) => J::Array(items.into_iter().filter(|i| !matches!(i, J::String(s) if s == k)).collect()),
        (Value::Array(keys), J::Object(mut m)) => {
            for k in keys {
                if let Value::Text(k) = k {
                    m.shift_remove(k);
                }
            }
            J::Object(m)
        }
        (idx, J::Array(mut items)) if idx.as_i64().is_some() => {
            let len = i64::try_from(items.len()).unwrap_or(0);
            let i = idx.as_i64().unwrap_or(0);
            let i = if i < 0 { len + i } else { i };
            if let Ok(i) = usize::try_from(i)
                && i < items.len()
            {
                items.remove(i);
            }
            J::Array(items)
        }
        (_, J::Object(_) | J::Array(_)) => {
            return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot delete from object using integer index"));
        }
        _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot delete from scalar")),
    }))
}

/// How `set_path` treats the last path step (`jsonb_set` vs `jsonb_insert`).
#[derive(Clone, Copy, PartialEq)]
enum SetMode {
    /// Replace an existing value; add a missing key / out-of-range element when `create`.
    Set { create: bool },
    /// Insert before (or after) an array element; add a missing object key.
    Insert { after: bool },
}

fn set_path(doc: &mut J, path: &[String], value: J, mode: SetMode) -> SqlResult<()> {
    let Some((first, rest)) = path.split_first() else {
        *doc = value;
        return Ok(());
    };
    match doc {
        J::Object(m) => {
            if rest.is_empty() {
                match mode {
                    SetMode::Set { create } => {
                        if create || m.contains_key(first) {
                            m.insert(first.clone(), value);
                        }
                    }
                    SetMode::Insert { .. } => {
                        if m.contains_key(first) {
                            return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot replace existing key")
                                .with_hint("Try using the function jsonb_set to replace key value."));
                        }
                        m.insert(first.clone(), value);
                    }
                }
            } else if let Some(child) = m.get_mut(first) {
                set_path(child, rest, value, mode)?;
            }
        }
        J::Array(items) => {
            let i: i64 = first.parse().map_err(|_| {
                SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("path element at position 1 is not an integer: \"{first}\""))
            })?;
            let len = i64::try_from(items.len()).unwrap_or(0);
            let idx = if i < 0 { len + i } else { i };
            if rest.is_empty() {
                match mode {
                    SetMode::Set { create } => match usize::try_from(idx) {
                        Ok(u) if u < items.len() => items[u] = value,
                        _ if create => {
                            if idx < 0 {
                                items.insert(0, value);
                            } else {
                                items.push(value);
                            }
                        }
                        _ => {}
                    },
                    SetMode::Insert { after } => {
                        let at = if idx < 0 { 0 } else { usize::try_from(idx).unwrap_or(usize::MAX).min(items.len()) };
                        let at = if after && at < items.len() { at + 1 } else { at };
                        items.insert(at, value);
                    }
                }
            } else if let Ok(u) = usize::try_from(idx)
                && let Some(child) = items.get_mut(u)
            {
                set_path(child, rest, value, mode)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn text_path(v: &Value) -> Vec<String> {
    path_values(v).iter().map(|p| p.as_str().unwrap_or("").to_owned()).collect()
}

fn jsonb_set(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut doc = jsonb(&a[0])?.as_value().clone();
    let create = a.get(3).and_then(Value::as_bool).unwrap_or(true);
    set_path(&mut doc, &text_path(&a[1]), jsonb(&a[2])?.as_value().clone(), SetMode::Set { create })?;
    Ok(wrap(doc))
}

/// `jsonb_set_lax(target, path, new_value, create_if_missing, null_value_treatment)`: a NULL
/// new value is handled per the last argument (default `use_json_null`).
fn jsonb_set_lax(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() || a[1].is_null() || a.get(3).is_some_and(Value::is_null) {
        return Ok(Value::Null);
    }
    if !a[2].is_null() {
        return jsonb_set(a, ctx);
    }
    let treatment = a.get(4).and_then(Value::as_str).unwrap_or("use_json_null");
    match treatment {
        "use_json_null" => {
            let mut args = a.to_vec();
            args[2] = Value::Jsonb(Jsonb::null());
            jsonb_set(&args, ctx)
        }
        "raise_exception" => Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "JSON value must not be null")
            .with_detail("Exception was raised because null_value_treatment is \"raise_exception\".")
            .with_hint("To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed.")),
        "delete_key" => op_delete_path(&[a[0].clone(), a[1].clone()], ctx),
        "return_target" => Ok(a[0].clone()),
        _ => Err(SqlError::new(
            code::INVALID_PARAMETER_VALUE,
            "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\"",
        )),
    }
}

fn jsonb_insert(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut doc = jsonb(&a[0])?.as_value().clone();
    let after = a.get(3).and_then(Value::as_bool).unwrap_or(false);
    set_path(&mut doc, &text_path(&a[1]), jsonb(&a[2])?.as_value().clone(), SetMode::Insert { after })?;
    Ok(wrap(doc))
}

fn op_delete_path(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut doc = jsonb(&a[0])?.as_value().clone();
    fn del(doc: &mut J, path: &[String]) {
        let Some((first, rest)) = path.split_first() else { return };
        match doc {
            J::Object(m) => {
                if rest.is_empty() {
                    m.shift_remove(first);
                } else if let Some(c) = m.get_mut(first) {
                    del(c, rest);
                }
            }
            J::Array(items) => {
                if let Ok(i) = first.parse::<i64>() {
                    let len = i64::try_from(items.len()).unwrap_or(0);
                    let Ok(i) = usize::try_from(if i < 0 { len + i } else { i }) else { return };
                    if rest.is_empty() {
                        if i < items.len() {
                            items.remove(i);
                        }
                    } else if let Some(c) = items.get_mut(i) {
                        del(c, rest);
                    }
                }
            }
            _ => {}
        }
    }
    del(&mut doc, &text_path(&a[1]));
    Ok(wrap(doc))
}

fn strip_nulls(j: &J, in_arrays: bool) -> J {
    match j {
        J::Object(m) => J::Object(m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), strip_nulls(v, in_arrays))).collect()),
        J::Array(items) => J::Array(items.iter().filter(|v| !(in_arrays && v.is_null())).map(|v| strip_nulls(v, in_arrays)).collect()),
        other => other.clone(),
    }
}

/// `json_strip_nulls` re-emits the document compactly without null-valued members.
fn json_strip_nulls(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    fn emit(out: &mut String, n: &Node, in_arrays: bool) {
        match n {
            Node::Object { members, .. } => {
                out.push('{');
                let mut first = true;
                for (k, v) in members {
                    if matches!(v, Node::Null { .. }) {
                        continue;
                    }
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    write_json_string(out, k);
                    out.push(':');
                    emit(out, v, in_arrays);
                }
                out.push('}');
            }
            Node::Array { items, .. } => {
                out.push('[');
                let mut first = true;
                for v in items {
                    if in_arrays && matches!(v, Node::Null { .. }) {
                        continue;
                    }
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    emit(out, v, in_arrays);
                }
                out.push(']');
            }
            other => out.push_str(other.text()),
        }
    }
    let doc = jsontext::parse(json_text(&a[0])?)?;
    let mut out = String::new();
    emit(&mut out, &doc, a.get(1).and_then(Value::as_bool) == Some(true));
    Ok(Value::Json(out))
}

// ---------------------------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------------------------

pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    use Type::*;
    if let Some(r) = resolve_path_operator(op, l, r)? {
        return Ok(Some(r));
    }
    if *l == Json {
        let res = |name: &'static str, f: crate::plan::ScalarFn, rt: Type, ret: Type| resolved(strict(name, f), vec![Json, rt], ret);
        return match op {
            "->" => match r {
                t if t.is_integer() => res("->", json_arrow, Int4, Json),
                Text | Unknown => res("->", json_arrow, Text, Json),
                _ => Err(no_operator(op, Some(l), r)),
            },
            "->>" => match r {
                t if t.is_integer() => res("->>", json_arrow_text, Int4, Text),
                Text | Unknown => res("->>", json_arrow_text, Text, Text),
                _ => Err(no_operator(op, Some(l), r)),
            },
            "#>" => res("#>", json_path_op, Type::array_of(Text), Json),
            "#>>" => res("#>>", json_path_text_op, Type::array_of(Text), Text),
            // json has no equality, containment or concatenation operators.
            "=" | "<>" | "@>" | "<@" | "?" | "?|" | "?&" | "-" | "#-" => Err(no_operator(op, Some(l), r)),
            _ => Ok(None),
        };
    }
    let is_jsonb = |t: &Type| matches!(t, Jsonb);
    if !(is_jsonb(l) || (is_jsonb(r) && matches!(op, "@>" | "<@" | "||"))) {
        return Ok(None);
    }
    let res = |name: &'static str, f: crate::plan::ScalarFn, rt: Type, ret: Type| resolved(strict(name, f), vec![Jsonb, rt], ret);
    match op {
        "->" => match r {
            t if t.is_integer() => res("->", op_arrow, Int4, Jsonb),
            Text | Unknown => res("->", op_arrow, Text, Jsonb),
            _ => Err(no_operator(op, Some(l), r)),
        },
        "->>" => match r {
            t if t.is_integer() => res("->>", op_arrow_text, Int4, Text),
            Text | Unknown => res("->>", op_arrow_text, Text, Text),
            _ => Err(no_operator(op, Some(l), r)),
        },
        "#>" => res("#>", op_path, Type::array_of(Text), Jsonb),
        "#>>" => res("#>>", op_path_text, Type::array_of(Text), Text),
        "@>" => resolved(strict("@>", op_contains), vec![Jsonb, Jsonb], Bool),
        "<@" => resolved(strict("<@", op_contained), vec![Jsonb, Jsonb], Bool),
        "?" => res("?", op_exists, Text, Bool),
        "?|" => res("?|", op_exists_any, Type::array_of(Text), Bool),
        "?&" => res("?&", op_exists_all, Type::array_of(Text), Bool),
        "||" => resolved(strict("||", op_concat), vec![Jsonb, Jsonb], Jsonb),
        "-" => match r {
            t if t.is_integer() => res("-", op_minus, Int4, Jsonb),
            Array(_) => res("-", op_minus, Type::array_of(Text), Jsonb),
            _ => res("-", op_minus, Text, Jsonb),
        },
        "#-" => res("#-", op_delete_path, Type::array_of(Text), Jsonb),
        _ => Ok(None),
    }
}

/// `jsonb[...]` subscripting: one `->` step per subscript.
pub fn subscript(doc: &Value, key: &Value) -> SqlResult<Value> {
    if doc.is_null() || key.is_null() {
        return Ok(Value::Null);
    }
    Ok(step(jsonb(doc)?.as_value(), key).map_or(Value::Null, wrap))
}

// ---------------------------------------------------------------------------------------------
// SQL/JSON path functions
// ---------------------------------------------------------------------------------------------

/// Runs a jsonpath over a jsonb document with `vars` (a jsonb object). With `silent`, the
/// errors Postgres suppresses become an empty result (`None`).
pub fn run_path(doc: &Value, path: &Value, vars: Option<&Value>, silent: bool) -> SqlResult<Option<Vec<J>>> {
    let doc = jsonb(doc)?.as_value();
    let path = pgmem_types::jsonpath::parse(path.as_str().unwrap_or(""))?;
    let empty = Map::new();
    let vars = match vars {
        None | Some(Value::Null) => &empty,
        Some(v) => match jsonb(v)?.as_value() {
            J::Object(m) => m,
            _ => {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "\"vars\" argument is not an object")
                    .with_detail("Jsonpath parameters should be encoded as key-value pairs of \"vars\" object."));
            }
        },
    };
    let ev = pgmem_types::jsonpath::Evaluator { strict: path.strict, root: doc, vars };
    match ev.eval(&path) {
        Ok(items) => Ok(Some(items)),
        Err(e) if silent && pgmem_types::jsonpath::is_suppressible(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn silent_arg(a: &[Value], i: usize) -> bool {
    a.get(i).and_then(Value::as_bool) == Some(true)
}

fn path_exists(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match run_path(&a[0], &a[1], a.get(2), silent_arg(a, 3))? {
        Some(items) => Value::Bool(!items.is_empty()),
        None => Value::Null,
    })
}

fn path_match(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let silent = silent_arg(a, 3);
    let Some(items) = run_path(&a[0], &a[1], a.get(2), silent)? else { return Ok(Value::Null) };
    match items.as_slice() {
        [J::Bool(b)] => Ok(Value::Bool(*b)),
        [J::Null] => Ok(Value::Null),
        _ if silent => Ok(Value::Null),
        _ => Err(SqlError::new("22038", "single boolean result is expected")),
    }
}

fn path_query_array(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(run_path(&a[0], &a[1], a.get(2), silent_arg(a, 3))?.map_or(Value::Null, |items| wrap(J::Array(items))))
}

fn path_query_first(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(run_path(&a[0], &a[1], a.get(2), silent_arg(a, 3))?.and_then(|items| items.into_iter().next()).map_or(Value::Null, wrap))
}

fn path_query(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() || a[1].is_null() {
        return Ok(Vec::new());
    }
    Ok(run_path(&a[0], &a[1], a.get(2), silent_arg(a, 3))?.unwrap_or_default().into_iter().map(|v| vec![wrap(v)]).collect())
}

/// `jsonb_path_*` signatures: (target, path [, vars [, silent]]).
fn path_args(n: usize) -> Vec<Type> {
    let mut t = vec![Type::Jsonb, Type::JsonPath, Type::Jsonb, Type::Bool];
    t.truncate(n);
    t
}

pub fn resolve_path_function(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let n = args.len();
    if !(2..=4).contains(&n) {
        return Ok(None);
    }
    let base = name.strip_suffix("_tz").unwrap_or(name);
    match base {
        "jsonb_path_exists" => resolved(strict("jsonb_path_exists", path_exists), path_args(n), Type::Bool),
        "jsonb_path_match" => resolved(strict("jsonb_path_match", path_match), path_args(n), Type::Bool),
        "jsonb_path_query_array" => resolved(strict("jsonb_path_query_array", path_query_array), path_args(n), Type::Jsonb),
        "jsonb_path_query_first" => resolved(strict("jsonb_path_query_first", path_query_first), path_args(n), Type::Jsonb),
        "jsonb_path_query" => Err(SqlError::feature_not_supported(
            "jsonb_path_query() must be used in FROM (set-returning functions in the select list are not supported by pgmem yet)",
        )),
        _ => Ok(None),
    }
}

/// `jsonb_path_query` in FROM.
/// (output columns, argument types, implementation) of a set-returning function.
pub type SrfSpec = (Vec<(&'static str, Type)>, Vec<Type>, crate::plan::SrfFn);

pub fn resolve_path_srf(name: &str, args: &[Type]) -> Option<SrfSpec> {
    let base = name.strip_suffix("_tz").unwrap_or(name);
    (base == "jsonb_path_query" && (2..=4).contains(&args.len()))
        .then(|| (vec![("jsonb_path_query", Type::Jsonb)], path_args(args.len()), path_query as crate::plan::SrfFn))
}

fn op_path_exists(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(run_path(&a[0], &a[1], None, true)?.map_or(Value::Null, |items| Value::Bool(!items.is_empty())))
}

fn op_path_match(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match run_path(&a[0], &a[1], None, true)?.as_deref() {
        Some([J::Bool(b)]) => Value::Bool(*b),
        _ => Value::Null,
    })
}

pub fn resolve_path_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    if *l != Type::Jsonb || !matches!(r, Type::JsonPath | Type::Unknown | Type::Text) {
        return Ok(None);
    }
    match op {
        "@?" => resolved(strict("@?", op_path_exists), vec![Type::Jsonb, Type::JsonPath], Type::Bool),
        "@@" => resolved(strict("@@", op_path_match), vec![Type::Jsonb, Type::JsonPath], Type::Bool),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------
// json(b)_populate_record(set), json(b)_to_record(set)
// ---------------------------------------------------------------------------------------------

/// How a record-producing json function is called.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordFn {
    /// `json(b)_populate_record(base, doc)`: columns from the base argument's type.
    Populate { set: bool },
    /// `json(b)_to_record(doc) AS t(col type, ...)`: columns from the definition list.
    ToRecord { set: bool },
    /// `JSON_TABLE(...)` (rewritten): columns from the definition list.
    JsonTable,
}

pub fn record_function(name: &str) -> Option<RecordFn> {
    Some(match name {
        "__pgmem_json_table" => RecordFn::JsonTable,
        "json_populate_record" | "jsonb_populate_record" => RecordFn::Populate { set: false },
        "json_populate_recordset" | "jsonb_populate_recordset" => RecordFn::Populate { set: true },
        "json_to_record" | "jsonb_to_record" => RecordFn::ToRecord { set: false },
        "json_to_recordset" | "jsonb_to_recordset" => RecordFn::ToRecord { set: true },
        _ => return None,
    })
}

/// The SRF implementing a record function. Its arguments are the document, a record whose
/// (blessed) type describes the output columns, and for populate the base record.
pub fn record_srf(kind: RecordFn) -> crate::plan::SrfFn {
    match kind {
        RecordFn::Populate { set: false } => |a, c| record_rows(a, c, false, "json_populate_record"),
        RecordFn::Populate { set: true } => |a, c| record_rows(a, c, true, "json_populate_recordset"),
        RecordFn::ToRecord { set: false } => |a, c| record_rows(a, c, false, "json_to_record"),
        RecordFn::ToRecord { set: true } => |a, c| record_rows(a, c, true, "json_to_recordset"),
        RecordFn::JsonTable => super::sqljson::json_table,
    }
}

fn record_rows(a: &[Value], _: &EvalCtx, set: bool, fname: &str) -> SqlResult<Vec<Row>> {
    let Value::Record(desc, _) = &a[1] else { return Err(SqlError::internal("record function without a descriptor")) };
    let fields = usertype::lookup(*desc).map(|t| t.fields().to_vec()).unwrap_or_default();
    let base = match a.get(2) {
        Some(Value::Record(_, vals)) => Some(vals.clone()),
        _ => None,
    };
    let doc = match &a[0] {
        Value::Null => return Ok(if set { Vec::new() } else { vec![base.unwrap_or_else(|| vec![Value::Null; fields.len()])] }),
        Value::Json(s) => pgmem_types::json::parse_json(s)?,
        other => jsonb(other)?.as_value().clone(),
    };
    let one = |obj: &J| -> SqlResult<Row> {
        let J::Object(m) = obj else {
            return Err(SqlError::new(
                code::INVALID_PARAMETER_VALUE,
                if set { format!("argument of {fname} must be an array of objects") } else { format!("cannot call {fname} on a scalar") },
            ));
        };
        fields
            .iter()
            .enumerate()
            .map(|(i, f)| match m.get(&f.name) {
                Some(v) => json_to_field(v, &f.ty, f.typmod),
                None => Ok(base.as_ref().and_then(|b| b.get(i).cloned()).unwrap_or(Value::Null)),
            })
            .collect()
    };
    if set {
        let J::Array(items) = &doc else {
            return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot call json_populate_recordset on an object"));
        };
        items.iter().map(one).collect()
    } else {
        Ok(vec![one(&doc)?])
    }
}

/// `populate_record_field`: a JSON value converted to a column's type.
fn json_to_field(v: &J, ty: &Type, typmod: i32) -> SqlResult<Value> {
    Ok(match (v, ty) {
        (J::Null, _) => Value::Null,
        (_, Type::Json) => Value::Json(Jsonb::from_value(v.clone()).to_string()),
        (_, Type::Jsonb) => Value::Jsonb(Jsonb::from_value(v.clone())),
        (J::Array(items), Type::Array(elem)) => Value::Array(items.iter().map(|i| json_to_field(i, elem, -1)).collect::<SqlResult<_>>()?),
        (J::Object(m), Type::Composite(oid)) => {
            let fields = usertype::lookup(*oid).map(|t| t.fields().to_vec()).unwrap_or_default();
            Value::Record(
                *oid,
                fields
                    .iter()
                    .map(|f| m.get(&f.name).map_or(Ok(Value::Null), |x| json_to_field(x, &f.ty, f.typmod)))
                    .collect::<SqlResult<_>>()?,
            )
        }
        (J::Array(_) | J::Object(_), t) if !matches!(t, Type::Text | Type::Varchar | Type::Bpchar) => {
            return Err(SqlError::new(
                code::INVALID_TEXT_REPRESENTATION,
                format!("expected JSON array or object for a value of type {}", t.name()),
            ));
        }
        (J::String(s), t) => crate::cast::cast_value(Value::Text(s.clone()), t, typmod)?,
        (other, t) => crate::cast::cast_value(Value::Text(Jsonb::from_value(other.clone()).to_string()), t, typmod)?,
    })
}

/// `expr IS JSON [VALUE|SCALAR|ARRAY|OBJECT] [WITH UNIQUE KEYS]`.
fn is_json_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let unique = a[2].as_bool() == Some(true);
    let text = match &a[0] {
        Value::Jsonb(j) => j.to_string(),
        Value::Bytea(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_owned(),
            Err(_) => return Ok(Value::Bool(false)),
        },
        other => other.as_str().unwrap_or("").to_owned(),
    };
    let Ok(doc) = jsontext::parse(&text) else { return Ok(Value::Bool(false)) };
    let kind_ok = match a[1].as_str().unwrap_or("value") {
        "scalar" => !matches!(doc, Node::Object { .. } | Node::Array { .. }),
        "array" => matches!(doc, Node::Array { .. }),
        "object" => matches!(doc, Node::Object { .. }),
        _ => true,
    };
    fn unique_keys(n: &Node) -> bool {
        match n {
            Node::Object { members, .. } => {
                let mut seen = std::collections::HashSet::new();
                members.iter().all(|(k, v)| seen.insert(k.as_str()) && unique_keys(v))
            }
            Node::Array { items, .. } => items.iter().all(unique_keys),
            _ => true,
        }
    }
    Ok(Value::Bool(kind_ok && (!unique || unique_keys(&doc))))
}
