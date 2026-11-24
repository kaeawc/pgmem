//! SQL/JSON query functions (`JSON_VALUE`, `JSON_QUERY`, `JSON_EXISTS`, `JSON_TABLE`), bound
//! from the internal calls `pgmem_parse` rewrites them into. Their argument layout:
//!
//! - `__pgmem_json_value(ctx, path, vars, NULL::T, empty_code, empty_default, error_code,
//!   error_default, T's oid)`
//! - `__pgmem_json_query(ctx, path, vars, NULL::T, wrapper, omit_quotes, empty_code,
//!   empty_default, error_code, error_default, T's oid)`
//! - `__pgmem_json_exists(ctx, path, vars, NULL::T, error_code, T's oid)`
//!
//! Behaviour codes are `pgmem_parse::sqljson`'s.

use super::{Resolved, lenient, resolved};
use crate::eval::EvalCtx;
use crate::plan::Row;
use pgmem_types::{Jsonb, SqlError, SqlResult, Type, Value};
use serde_json::Value as J;

const ON_NULL: i64 = 0;
const ON_ERROR_RAISE: i64 = 1;
const ON_DEFAULT: i64 = 2;
const ON_EMPTY_ARRAY: i64 = 3;
const ON_EMPTY_OBJECT: i64 = 4;
const ON_TRUE: i64 = 5;
const ON_FALSE: i64 = 6;
const ON_UNKNOWN: i64 = 7;

/// The context item type: json and character input are read as jsonb.
fn context_type(t: &Type) -> Type {
    match t {
        Type::Jsonb => Type::Jsonb,
        _ => Type::Jsonb,
    }
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let n = args.len();
    match name {
        "__pgmem_json_value" if n == 9 => {
            let t = args[3].clone();
            resolved(
                lenient("json_value", json_value),
                vec![
                    context_type(&args[0]),
                    Type::JsonPath,
                    Type::Jsonb,
                    t.clone(),
                    Type::Int4,
                    t.clone(),
                    Type::Int4,
                    t.clone(),
                    Type::Oid,
                ],
                t,
            )
        }
        "__pgmem_json_query" if n == 11 => {
            let t = args[3].clone();
            resolved(
                lenient("json_query", json_query),
                vec![
                    context_type(&args[0]),
                    Type::JsonPath,
                    Type::Jsonb,
                    t.clone(),
                    Type::Int4,
                    Type::Bool,
                    Type::Int4,
                    t.clone(),
                    Type::Int4,
                    t.clone(),
                    Type::Oid,
                ],
                t,
            )
        }
        "__pgmem_json_exists" if n == 6 => {
            let t = args[3].clone();
            resolved(
                lenient("json_exists", json_exists),
                vec![context_type(&args[0]), Type::JsonPath, Type::Jsonb, t.clone(), Type::Int4, Type::Oid],
                t,
            )
        }
        _ => Ok(None),
    }
}

fn target_type(v: &Value) -> Type {
    match v {
        Value::Oid(o) => Type::from_oid(*o).unwrap_or(Type::Text),
        _ => Type::Text,
    }
}

fn code_of(v: &Value) -> i64 {
    v.as_i64().unwrap_or(ON_NULL)
}

/// The value an ON EMPTY / ON ERROR behaviour yields, or the error to raise.
fn behave(code: i64, default: &Value, target: &Type, err: SqlError) -> SqlResult<Value> {
    match code {
        ON_ERROR_RAISE => Err(err),
        ON_DEFAULT => Ok(default.clone()),
        ON_EMPTY_ARRAY => crate::cast::cast_value(Value::Jsonb(Jsonb::from_value(J::Array(vec![]))), target, -1),
        ON_EMPTY_OBJECT => crate::cast::cast_value(Value::Jsonb(Jsonb::from_value(J::Object(serde_json::Map::new()))), target, -1),
        ON_TRUE => Ok(Value::Bool(true)),
        ON_FALSE => Ok(Value::Bool(false)),
        _ => Ok(Value::Null),
    }
}

fn no_item() -> SqlError {
    SqlError::new("22035", "no SQL/JSON item found for specified path")
}

/// Evaluates the path; Err carries the evaluation error.
fn items(ctx: &Value, path: &Value, vars: &Value) -> SqlResult<Vec<J>> {
    Ok(super::json::run_path(ctx, path, (!vars.is_null()).then_some(vars), false)?.unwrap_or_default())
}

fn json_value(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    json_value_impl(a)
}

fn json_value_impl(a: &[Value]) -> SqlResult<Value> {
    if a[0].is_null() || a[1].is_null() {
        return Ok(Value::Null);
    }
    let target = target_type(&a[8]);
    let (empty_code, empty_default, error_code, error_default) = (code_of(&a[4]), &a[5], code_of(&a[6]), &a[7]);
    let found = match items(&a[0], &a[1], &a[2]) {
        Ok(v) => v,
        Err(e) => return behave(error_code, error_default, &target, e),
    };
    let item = match found.as_slice() {
        [] => return behave(empty_code, empty_default, &target, no_item()),
        [one] => one.clone(),
        _ => {
            return behave(
                error_code,
                error_default,
                &target,
                SqlError::new("22034", "JSON path expression in JSON_VALUE must return single item"),
            );
        }
    };
    let text = match item {
        J::Null => return Ok(Value::Null),
        J::String(s) => s,
        J::Array(_) | J::Object(_) => {
            return behave(
                error_code,
                error_default,
                &target,
                SqlError::new("2203F", "JSON path expression in JSON_VALUE must return single scalar item"),
            );
        }
        other => other.to_string(),
    };
    match crate::cast::cast_value(Value::Text(text), &target, -1) {
        Ok(v) => Ok(v),
        Err(e) => behave(error_code, error_default, &target, e),
    }
}

fn json_query(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    json_query_impl(a)
}

fn json_query_impl(a: &[Value]) -> SqlResult<Value> {
    if a[0].is_null() || a[1].is_null() {
        return Ok(Value::Null);
    }
    let target = target_type(&a[10]);
    let (wrapper, omit_quotes) = (code_of(&a[4]), a[5].as_bool() == Some(true));
    let (empty_code, empty_default, error_code, error_default) = (code_of(&a[6]), &a[7], code_of(&a[8]), &a[9]);
    let found = match items(&a[0], &a[1], &a[2]) {
        Ok(v) => v,
        Err(e) => return behave(error_code, error_default, &target, e),
    };
    if found.is_empty() {
        return behave(empty_code, empty_default, &target, no_item());
    }
    // Conditional wrapping leaves a single array or object alone.
    let wrap = match wrapper {
        1 => true,
        2 => !(found.len() == 1 && matches!(found[0], J::Array(_) | J::Object(_))),
        _ => false,
    };
    let result = if wrap {
        J::Array(found)
    } else if found.len() == 1 {
        found.into_iter().next().unwrap_or(J::Null)
    } else {
        return behave(
            error_code,
            error_default,
            &target,
            SqlError::new("22034", "JSON path expression in JSON_QUERY must return single item when no wrapper is requested")
                .with_hint("Use the WITH WRAPPER clause to wrap SQL/JSON items into an array."),
        );
    };
    let value = match (&result, omit_quotes) {
        (J::String(s), true) if !matches!(target, Type::Jsonb | Type::Json) => Value::Text(s.clone()),
        (J::String(s), true) => match pgmem_types::json::parse_json(s) {
            Ok(j) => Value::Jsonb(Jsonb::from_value(j)),
            Err(e) => return behave(error_code, error_default, &target, e),
        },
        _ => Value::Jsonb(Jsonb::from_value(result)),
    };
    match crate::cast::cast_value(value, &target, -1) {
        Ok(v) => Ok(v),
        Err(e) => behave(error_code, error_default, &target, e),
    }
}

fn json_exists(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    json_exists_impl(a)
}

fn json_exists_impl(a: &[Value]) -> SqlResult<Value> {
    if a[0].is_null() || a[1].is_null() {
        return Ok(Value::Null);
    }
    let target = target_type(&a[5]);
    let v = match items(&a[0], &a[1], &a[2]) {
        Ok(found) => Value::Bool(!found.is_empty()),
        Err(e) => match code_of(&a[4]) {
            ON_ERROR_RAISE => return Err(e),
            ON_TRUE => Value::Bool(true),
            ON_UNKNOWN => Value::Null,
            _ => Value::Bool(false),
        },
    };
    crate::cast::cast_value(v, &target, -1)
}

// ---------------------------------------------------------------------------------------------
// JSON_TABLE
// ---------------------------------------------------------------------------------------------

/// `__pgmem_json_table(ctx, path, vars, spec, error_code)` with a column definition list:
/// rows of the row path's items; each column extracted by its own path (JSON_VALUE rules for
/// scalar columns, JSON_QUERY rules for json/jsonb ones, EXISTS columns, ordinality), NESTED
/// paths producing a row per nested item.
pub fn json_table(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[0].is_null() {
        return Ok(Vec::new());
    }
    let Value::Record(desc, _) = &a[5] else { return Err(SqlError::internal("JSON_TABLE without a descriptor")) };
    let fields = pgmem_types::usertype::lookup(*desc).map(|t| t.fields().to_vec()).unwrap_or_default();
    let spec: J = serde_json::from_str(a[3].as_str().unwrap_or("[]")).map_err(|e| SqlError::internal(format!("JSON_TABLE spec: {e}")))?;
    let error_code = code_of(&a[4]);
    let vars = (!a[2].is_null()).then_some(&a[2]);
    let rows = match super::json::run_path(&a[0], &a[1], vars, false) {
        Ok(found) => found.unwrap_or_default(),
        Err(e) if error_code == ON_ERROR_RAISE => return Err(e),
        Err(_) => Vec::new(),
    };
    let J::Array(cols) = spec else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for (i, item) in rows.iter().enumerate() {
        for row in table_rows(item, &cols, &fields, 0, i + 1)? {
            out.push(row);
        }
    }
    Ok(out)
}

/// The rows one item produces: its column values, crossed with each NESTED path's rows
/// (a nested path with no rows still yields the parent with NULLs).
fn table_rows(item: &J, cols: &[J], fields: &[pgmem_types::usertype::Field], start: usize, ord: usize) -> SqlResult<Vec<Row>> {
    let width = count_fields(cols);
    let mut base: Row = vec![Value::Null; width];
    let mut nested: Vec<(usize, Vec<Row>)> = Vec::new();
    let mut pos = 0;
    for col in cols {
        let kind = col.get("k").and_then(J::as_str).unwrap_or("value");
        match kind {
            "nested" => {
                let inner = col.get("c").and_then(J::as_array).cloned().unwrap_or_default();
                let n = count_fields(&inner);
                let path = col.get("p").and_then(J::as_str).unwrap_or("$");
                let doc = Value::Jsonb(Jsonb::from_value(item.clone()));
                let items = super::json::run_path(&doc, &Value::Text(path.to_owned()), None, true)?.unwrap_or_default();
                let mut rows = Vec::new();
                for (j, sub) in items.iter().enumerate() {
                    rows.extend(table_rows(sub, &inner, fields, start + pos, j + 1)?);
                }
                nested.push((pos, rows));
                pos += n;
            }
            "ord" => {
                base[pos] = Value::Int4(i32::try_from(ord).unwrap_or(i32::MAX));
                pos += 1;
            }
            _ => {
                let f = &fields[start + pos];
                base[pos] = column_value(item, col, kind, &f.ty)?;
                pos += 1;
            }
        }
    }
    if nested.is_empty() {
        return Ok(vec![base]);
    }
    // Sibling NESTED paths are unioned: each nested row fills its own columns only.
    let mut out = Vec::new();
    let mut any = false;
    for (at, rows) in &nested {
        for r in rows {
            any = true;
            let mut row = base.clone();
            for (k, v) in r.iter().enumerate() {
                row[at + k] = v.clone();
            }
            out.push(row);
        }
    }
    if !any {
        out.push(base);
    }
    Ok(out)
}

fn count_fields(cols: &[J]) -> usize {
    cols.iter()
        .map(|c| match c.get("k").and_then(J::as_str) {
            Some("nested") => count_fields(c.get("c").and_then(J::as_array).map_or(&[][..], Vec::as_slice)),
            _ => 1,
        })
        .sum()
}

fn column_value(item: &J, col: &J, kind: &str, ty: &Type) -> SqlResult<Value> {
    let path = col.get("p").and_then(J::as_str).unwrap_or("$");
    let doc = Value::Jsonb(Jsonb::from_value(item.clone()));
    let pathv = Value::Text(path.to_owned());
    let code = |k: &str, default: i64| col.get(k).and_then(J::as_i64).unwrap_or(default);
    let oid = Value::Oid(ty.oid());
    match kind {
        "exists" => {
            json_exists_impl(&[doc, pathv, Value::Null, Value::Null, Value::Int4(i32::try_from(code("r", ON_FALSE)).unwrap_or(0)), oid])
        }
        "query" => json_query_impl(&[
            doc,
            pathv,
            Value::Null,
            Value::Null,
            Value::Int4(i32::try_from(code("w", 0)).unwrap_or(0)),
            Value::Bool(col.get("q").and_then(J::as_bool) == Some(true)),
            Value::Int4(i32::try_from(code("e", ON_NULL)).unwrap_or(0)),
            Value::Null,
            Value::Int4(i32::try_from(code("r", ON_NULL)).unwrap_or(0)),
            Value::Null,
            oid,
        ]),
        _ => json_value_impl(&[
            doc,
            pathv,
            Value::Null,
            Value::Null,
            Value::Int4(i32::try_from(code("e", ON_NULL)).unwrap_or(0)),
            Value::Null,
            Value::Int4(i32::try_from(code("r", ON_NULL)).unwrap_or(0)),
            Value::Null,
            oid,
        ]),
    }
}
