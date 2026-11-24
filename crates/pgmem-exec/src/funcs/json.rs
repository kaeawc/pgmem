//! jsonb functions and operators (`json` is carried as `jsonb`).

use super::{Resolved, lenient, no_function, no_operator, resolved, strict};
use crate::eval::EvalCtx;
use crate::plan::Row;
use pgmem_types::datetime::{format_date, format_time, format_timestamptz};
use pgmem_types::{Jsonb, SqlError, SqlResult, Type, Value, code, text};
use serde_json::{Map, Number, Value as J};

/// Converts a SQL value to a JSON value the way `to_jsonb` does.
pub fn to_json_value(v: &Value) -> SqlResult<J> {
    Ok(match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => J::Number(Number::from(v.as_i64().unwrap_or(0))),
        Value::Float4(_) | Value::Float8(_) => {
            let f = v.as_f64().unwrap_or(f64::NAN);
            if f.is_finite() {
                serde_json::from_str(&text::encode(v, &v.natural_type())?).unwrap_or(J::Null)
            } else {
                J::String(text::encode(v, &v.natural_type())?)
            }
        }
        Value::Numeric(n) => {
            if n.is_nan() {
                J::String("NaN".into())
            } else {
                serde_json::from_str(&n.to_string()).unwrap_or(J::Null)
            }
        }
        Value::Text(s) => J::String(s.clone()),
        Value::Jsonb(j) => j.as_value().clone(),
        Value::Array(items) => J::Array(items.iter().map(to_json_value).collect::<SqlResult<Vec<_>>>()?),
        // Timestamps use ISO 8601 with a `T` separator and a full offset.
        Value::Timestamptz(t) => {
            let s = format_timestamptz(*t);
            J::String(match s.strip_suffix("+00") {
                Some(base) => format!("{}+00:00", base.replacen(' ', "T", 1)),
                None => s,
            })
        }
        Value::Timestamp(t) => J::String(text::encode(&Value::Timestamp(*t), &Type::Timestamp)?.replacen(' ', "T", 1)),
        Value::Date(d) => J::String(format_date(*d)),
        Value::Time(t) => J::String(format_time(*t)),
        other => J::String(text::encode(other, &other.natural_type())?),
    })
}

fn jsonb(v: &Value) -> SqlResult<&Jsonb> {
    match v {
        Value::Jsonb(j) => Ok(j),
        other => Err(SqlError::internal(format!("expected jsonb, got {}", other.kind()))),
    }
}

fn wrap(j: J) -> Value {
    Value::Jsonb(Jsonb::from_value(j))
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    let any_args = || args.iter().map(|t| if *t == Type::Unknown { Type::Text } else { t.clone() }).collect::<Vec<_>>();
    match name {
        "to_jsonb" | "to_json" if n == 1 => {
            if args[0] == Type::Unknown {
                return Err(SqlError::datatype_mismatch("could not determine polymorphic type because input has type unknown"));
            }
            resolved(strict("to_jsonb", |a, _| Ok(wrap(to_json_value(&a[0])?))), any_args(), Type::Jsonb)
        }
        "jsonb_build_object" | "json_build_object" => {
            if !n.is_multiple_of(2) {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "argument list must have even number of elements")
                    .with_hint(format!("The arguments of {name}() must consist of alternating keys and values.")));
            }
            resolved(lenient("jsonb_build_object", build_object), any_args(), Type::Jsonb)
        }
        "jsonb_build_array" | "json_build_array" => resolved(lenient("jsonb_build_array", build_array), any_args(), Type::Jsonb),
        "jsonb_typeof" | "json_typeof" if n == 1 => {
            resolved(strict("jsonb_typeof", |a, _| Ok(Value::Text(jsonb(&a[0])?.type_name().to_owned()))), vec![Type::Jsonb], Type::Text)
        }
        "jsonb_array_length" | "json_array_length" if n == 1 => {
            resolved(strict("jsonb_array_length", array_length), vec![Type::Jsonb], Type::Int4)
        }
        "jsonb_extract_path" | "json_extract_path" if n >= 1 => {
            let mut t = vec![Type::Text; n];
            t[0] = Type::Jsonb;
            resolved(
                strict("jsonb_extract_path", |a, _| Ok(path_get(jsonb(&a[0])?.as_value(), &a[1..]).map_or(Value::Null, wrap))),
                t,
                Type::Jsonb,
            )
        }
        "jsonb_extract_path_text" | "json_extract_path_text" if n >= 1 => {
            let mut t = vec![Type::Text; n];
            t[0] = Type::Jsonb;
            resolved(
                strict("jsonb_extract_path_text", |a, _| Ok(path_get(jsonb(&a[0])?.as_value(), &a[1..]).map_or(Value::Null, as_text))),
                t,
                Type::Text,
            )
        }
        "jsonb_set" if n == 3 || n == 4 => {
            let mut t = vec![Type::Jsonb, Type::array_of(Type::Text), Type::Jsonb];
            if n == 4 {
                t.push(Type::Bool);
            }
            resolved(strict("jsonb_set", jsonb_set), t, Type::Jsonb)
        }
        "jsonb_strip_nulls" | "json_strip_nulls" if n == 1 => {
            resolved(strict("jsonb_strip_nulls", |a, _| Ok(wrap(strip_nulls(jsonb(&a[0])?.as_value())))), vec![Type::Jsonb], Type::Jsonb)
        }
        "jsonb_pretty" if n == 1 => resolved(
            strict("jsonb_pretty", |a, _| {
                Ok(Value::Text(serde_json::to_string_pretty(jsonb(&a[0])?.as_value()).unwrap_or_default().replace("  ", "    ")))
            }),
            vec![Type::Jsonb],
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

/// Set-returning jsonb functions usable in FROM: (output column names/types, implementation).
pub fn resolve_srf(name: &str, args: &[Type]) -> Option<(Vec<(&'static str, Type)>, crate::plan::SrfFn)> {
    if args.len() != 1 || !matches!(args[0], Type::Jsonb | Type::Unknown) {
        return None;
    }
    Some(match name {
        "jsonb_object_keys" | "json_object_keys" => {
            (vec![(if name.starts_with("jsonb") { "jsonb_object_keys" } else { "json_object_keys" }, Type::Text)], object_keys)
        }
        "jsonb_each" | "json_each" => (vec![("key", Type::Text), ("value", Type::Jsonb)], each),
        "jsonb_each_text" | "json_each_text" => (vec![("key", Type::Text), ("value", Type::Text)], each_text),
        "jsonb_array_elements" | "json_array_elements" => (vec![("value", Type::Jsonb)], array_elements),
        "jsonb_array_elements_text" | "json_array_elements_text" => (vec![("value", Type::Text)], array_elements_text),
        _ => return None,
    })
}

fn obj<'a>(v: &'a Value, f: &str) -> SqlResult<&'a Map<String, J>> {
    match jsonb(v)?.as_value() {
        J::Object(m) => Ok(m),
        _ => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("cannot call {f} on a non-object"))),
    }
}

fn arr<'a>(v: &'a Value, f: &str) -> SqlResult<&'a Vec<J>> {
    match jsonb(v)?.as_value() {
        J::Array(a) => Ok(a),
        J::Object(_) => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("cannot extract elements from an object ({f})"))),
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
    Ok(obj(&a[0], "jsonb_each_text")?.iter().map(|(k, v)| vec![Value::Text(k.clone()), as_text(v.clone())]).collect())
}

fn array_elements(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    Ok(arr(&a[0], "jsonb_array_elements")?.iter().map(|v| vec![wrap(v.clone())]).collect())
}

fn array_elements_text(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    Ok(arr(&a[0], "jsonb_array_elements_text")?.iter().map(|v| vec![as_text(v.clone())]).collect())
}

fn key_of(v: &Value) -> SqlResult<String> {
    match v {
        Value::Null => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "argument 1: key must not be null")),
        Value::Text(s) => Ok(s.clone()),
        other => text::encode(other, &other.natural_type()),
    }
}

fn build_object(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut map = Map::new();
    for pair in a.chunks(2) {
        map.insert(key_of(&pair[0])?, to_json_value(&pair[1])?);
    }
    Ok(wrap(J::Object(map)))
}

fn build_array(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(wrap(J::Array(a.iter().map(to_json_value).collect::<SqlResult<Vec<_>>>()?)))
}

fn array_length(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    match jsonb(&a[0])?.as_value() {
        J::Array(items) => Ok(Value::Int4(i32::try_from(items.len()).unwrap_or(i32::MAX))),
        J::Object(_) => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot get array length of a non-array")),
        _ => Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "cannot get array length of a scalar")),
    }
}

/// `->>` / `#>>` text rendering: strings unwrap, JSON null becomes SQL NULL.
fn as_text(j: J) -> Value {
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
    Ok(step(jsonb(&a[0])?.as_value(), &a[1]).map_or(Value::Null, as_text))
}

fn op_path(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(path_get(jsonb(&a[0])?.as_value(), &path_values(&a[1])).map_or(Value::Null, wrap))
}

fn op_path_text(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(path_get(jsonb(&a[0])?.as_value(), &path_values(&a[1])).map_or(Value::Null, as_text))
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

fn set_path(doc: &mut J, path: &[String], value: J, create: bool) {
    let Some((first, rest)) = path.split_first() else {
        *doc = value;
        return;
    };
    match doc {
        J::Object(m) => {
            if rest.is_empty() {
                if create || m.contains_key(first) {
                    m.insert(first.clone(), value);
                }
            } else if let Some(child) = m.get_mut(first) {
                set_path(child, rest, value, create);
            }
        }
        J::Array(items) => {
            let Ok(i) = first.parse::<i64>() else { return };
            let len = i64::try_from(items.len()).unwrap_or(0);
            let idx = if i < 0 { len + i } else { i };
            if rest.is_empty() {
                match usize::try_from(idx) {
                    Ok(u) if u < items.len() => items[u] = value,
                    _ if create => {
                        if idx < 0 {
                            items.insert(0, value);
                        } else {
                            items.push(value);
                        }
                    }
                    _ => {}
                }
            } else if let Ok(u) = usize::try_from(idx)
                && let Some(child) = items.get_mut(u)
            {
                set_path(child, rest, value, create);
            }
        }
        _ => {}
    }
}

fn jsonb_set(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut doc = jsonb(&a[0])?.as_value().clone();
    let path: Vec<String> = path_values(&a[1]).iter().map(|p| p.as_str().unwrap_or("").to_owned()).collect();
    let create = a.get(3).and_then(Value::as_bool).unwrap_or(true);
    set_path(&mut doc, &path, jsonb(&a[2])?.as_value().clone(), create);
    Ok(wrap(doc))
}

fn op_delete_path(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut doc = jsonb(&a[0])?.as_value().clone();
    let path: Vec<String> = path_values(&a[1]).iter().map(|p| p.as_str().unwrap_or("").to_owned()).collect();
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
                if let Ok(i) = first.parse::<usize>() {
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
    del(&mut doc, &path);
    Ok(wrap(doc))
}

fn strip_nulls(j: &J) -> J {
    match j {
        J::Object(m) => J::Object(m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), strip_nulls(v))).collect()),
        J::Array(items) => J::Array(items.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}

pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    use Type::*;
    let is_json = |t: &Type| matches!(t, Jsonb);
    if !(is_json(l) || (is_json(r) && matches!(op, "@>" | "<@" | "||"))) {
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
