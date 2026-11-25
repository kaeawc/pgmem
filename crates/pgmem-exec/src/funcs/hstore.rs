//! The `hstore` extension's operators and functions.

use super::{Resolved, no_operator, resolved, strict};
use crate::eval::EvalCtx;
use pgmem_types::hstore::{concat, normalize};
use pgmem_types::{SqlError, SqlResult, Type, Value, code};

type Pairs = Vec<(String, Option<String>)>;

fn pairs(v: &Value) -> SqlResult<&Pairs> {
    match v {
        Value::HStore(p) => Ok(p),
        _ => Err(SqlError::internal("hstore value expected")),
    }
}

fn texts(v: &Value) -> Vec<Option<String>> {
    match v {
        Value::Array(items) => items.iter().map(|i| i.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

fn get<'a>(p: &'a Pairs, k: &str) -> Option<&'a Option<String>> {
    p.iter().find(|(key, _)| key == k).map(|(_, v)| v)
}

fn opt_text(v: Option<String>) -> Value {
    v.map_or(Value::Null, Value::Text)
}

fn fetch(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let p = pairs(&a[0])?;
    Ok(opt_text(get(p, a[1].as_str().unwrap_or("")).cloned().flatten()))
}

fn fetch_many(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let p = pairs(&a[0])?;
    Ok(Value::Array(texts(&a[1]).into_iter().map(|k| opt_text(k.and_then(|k| get(p, &k).cloned().flatten()))).collect()))
}

fn exists(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(get(pairs(&a[0])?, a[1].as_str().unwrap_or("")).is_some()))
}

fn exists_all(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let p = pairs(&a[0])?;
    Ok(Value::Bool(texts(&a[1]).iter().flatten().all(|k| get(p, k).is_some())))
}

fn exists_any(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let p = pairs(&a[0])?;
    Ok(Value::Bool(texts(&a[1]).iter().flatten().any(|k| get(p, k).is_some())))
}

fn defined(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(get(pairs(&a[0])?, a[1].as_str().unwrap_or("")).is_some_and(Option::is_some)))
}

fn cat(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::HStore(concat(pairs(&a[0])?, pairs(&a[1])?)))
}

fn contains(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (outer, inner) = (pairs(&a[0])?, pairs(&a[1])?);
    Ok(Value::Bool(inner.iter().all(|(k, v)| get(outer, k) == Some(v))))
}

fn contained(a: &[Value], c: &EvalCtx) -> SqlResult<Value> {
    contains(&[a[1].clone(), a[0].clone()], c)
}

fn delete_key(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let k = a[1].as_str().unwrap_or("");
    Ok(Value::HStore(pairs(&a[0])?.iter().filter(|(key, _)| key != k).cloned().collect()))
}

fn delete_keys(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let ks: Vec<String> = texts(&a[1]).into_iter().flatten().collect();
    Ok(Value::HStore(pairs(&a[0])?.iter().filter(|(key, _)| !ks.contains(key)).cloned().collect()))
}

fn delete_pairs(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let other = pairs(&a[1])?;
    Ok(Value::HStore(pairs(&a[0])?.iter().filter(|(k, v)| get(other, k) != Some(v)).cloned().collect()))
}

fn akeys(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Array(pairs(&a[0])?.iter().map(|(k, _)| Value::Text(k.clone())).collect()))
}

fn avals(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Array(pairs(&a[0])?.iter().map(|(_, v)| opt_text(v.clone())).collect()))
}

fn from_kv(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    match &a[0] {
        Value::Null => Ok(Value::Null),
        k => Ok(Value::HStore(vec![(k.as_str().unwrap_or("").to_owned(), a[1].as_str().map(str::to_owned))])),
    }
}

fn from_arrays(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let keys = texts(&a[0]);
    let vals = if a.len() > 1 { texts(&a[1]) } else { Vec::new() };
    if a.len() == 1 {
        // hstore(text[]): alternating keys and values.
        if !keys.len().is_multiple_of(2) {
            return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "array must have even number of elements"));
        }
        let ps = keys.chunks(2).map(|c| (c[0].clone().unwrap_or_default(), c[1].clone())).collect();
        return Ok(Value::HStore(normalize(ps)));
    }
    if keys.len() != vals.len() {
        return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "arrays must have same bounds"));
    }
    let mut ps = Vec::new();
    for (k, v) in keys.into_iter().zip(vals) {
        let k = k.ok_or_else(|| SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "null value not allowed for hstore key"))?;
        ps.push((k, v));
    }
    Ok(Value::HStore(normalize(ps)))
}

fn to_json(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let map: serde_json::Map<String, serde_json::Value> =
        pairs(&a[0])?.iter().map(|(k, v)| (k.clone(), v.clone().map_or(serde_json::Value::Null, serde_json::Value::String))).collect();
    Ok(Value::Jsonb(pgmem_types::Jsonb::from_value(serde_json::Value::Object(map))))
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    use Type::{Bool, HStore, Text};
    let arr = Type::array_of(Text);
    let first_hstore = matches!(args.first(), Some(HStore | Type::Unknown));
    match (name, args.len()) {
        ("hstore", 2) if args[0].is_string() || args[0] == Type::Unknown => {
            resolved(super::lenient("hstore", from_kv), vec![Text, Text], HStore)
        }
        ("hstore", 2) => resolved(strict("hstore", from_arrays), vec![arr.clone(), arr], HStore),
        ("hstore", 1) if matches!(args[0], Type::Array(_)) => resolved(strict("hstore", from_arrays), vec![arr], HStore),
        ("akeys" | "skeys", 1) if first_hstore => resolved(strict("akeys", akeys), vec![HStore], arr),
        ("avals" | "svals", 1) if first_hstore => resolved(strict("avals", avals), vec![HStore], arr),
        ("exist", 2) if first_hstore => resolved(strict("exist", exists), vec![HStore, Text], Bool),
        ("defined", 2) if first_hstore => resolved(strict("defined", defined), vec![HStore, Text], Bool),
        ("delete", 2) if first_hstore => resolved(strict("delete", delete_key), vec![HStore, Text], HStore),
        ("hstore_to_json" | "hstore_to_jsonb", 1) if first_hstore => {
            let ret = if name == "hstore_to_json" { Type::Json } else { Type::Jsonb };
            resolved(strict("hstore_to_jsonb", to_json), vec![HStore], ret)
        }
        _ => Ok(None),
    }
}

pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    use Type::{Bool, HStore, Text};
    if *l != HStore && *r != HStore {
        return Ok(None);
    }
    let arr = Type::array_of(Text);
    let r_array = matches!(r, Type::Array(_));
    match op {
        "->" if r_array => resolved(strict("->", fetch_many), vec![HStore, arr.clone()], arr),
        "->" => resolved(strict("->", fetch), vec![HStore, Text], Text),
        "?" => resolved(strict("?", exists), vec![HStore, Text], Bool),
        "?&" => resolved(strict("?&", exists_all), vec![HStore, arr], Bool),
        "?|" => resolved(strict("?|", exists_any), vec![HStore, arr], Bool),
        "||" => resolved(strict("||", cat), vec![HStore, HStore], HStore),
        "@>" => resolved(strict("@>", contains), vec![HStore, HStore], Bool),
        "<@" => resolved(strict("<@", contained), vec![HStore, HStore], Bool),
        "-" if r_array => resolved(strict("-", delete_keys), vec![HStore, arr], HStore),
        "-" if *r == HStore => resolved(strict("-", delete_pairs), vec![HStore, HStore], HStore),
        "-" => resolved(strict("-", delete_key), vec![HStore, Text], HStore),
        _ => Err(no_operator(op, Some(l), r)),
    }
}
