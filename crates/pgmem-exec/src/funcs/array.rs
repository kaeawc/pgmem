//! Array functions and operators, and the set-returning `unnest` / `generate_series`.

use super::{Resolved, no_function, no_operator, resolved, strict};
use crate::eval::EvalCtx;
use crate::plan::{Row, SrfFn};
use pgmem_types::datetime::add_interval;
use pgmem_types::{Numeric, SqlError, SqlResult, Type, Value, code, compare_values};

fn items(v: &Value) -> &[Value] {
    match v {
        Value::Array(i) => i,
        _ => &[],
    }
}

fn len_i32(n: usize) -> Value {
    Value::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

fn elem_of(t: &Type) -> Type {
    t.element().cloned().unwrap_or(Type::Unknown)
}

fn arr_type(t: &Type) -> Option<Type> {
    match t {
        Type::Array(_) => Some(t.clone()),
        Type::Unknown => Some(Type::array_of(Type::Text)),
        _ => None,
    }
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    match name {
        "array_length" | "array_upper" | "array_lower" if n == 2 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            let f = match name {
                "array_length" => strict("array_length", array_length),
                "array_upper" => strict("array_upper", array_length),
                _ => strict("array_lower", array_lower),
            };
            resolved(f, vec![a, Type::Int4], Type::Int4)
        }
        "cardinality" if n == 1 => {
            resolved(strict("cardinality", |a, _| Ok(len_i32(items(&a[0]).len()))), vec![arr_type(&args[0]).ok_or_else(err)?], Type::Int4)
        }
        "array_ndims" if n == 1 => resolved(
            strict("array_ndims", |a, _| Ok(if items(&a[0]).is_empty() { Value::Null } else { Value::Int4(1) })),
            vec![arr_type(&args[0]).ok_or_else(err)?],
            Type::Int4,
        ),
        "array_dims" if n == 1 => resolved(
            strict("array_dims", |a, _| {
                let n = items(&a[0]).len();
                Ok(if n == 0 { Value::Null } else { Value::Text(format!("[1:{n}]")) })
            }),
            vec![arr_type(&args[0]).ok_or_else(err)?],
            Type::Text,
        ),
        "array_append" if n == 2 => {
            let a = arr_type(&args[0]).filter(|_| args[0] != Type::Unknown).unwrap_or_else(|| Type::array_of(args[1].clone()));
            resolved(super::lenient("array_append", array_append), vec![a.clone(), elem_of(&a)], a)
        }
        "array_prepend" if n == 2 => {
            let a = arr_type(&args[1]).filter(|_| args[1] != Type::Unknown).unwrap_or_else(|| Type::array_of(args[0].clone()));
            resolved(super::lenient("array_prepend", array_prepend), vec![elem_of(&a), a.clone()], a)
        }
        "array_cat" if n == 2 => {
            let a = super::common_type(&args[0], &args[1]).filter(|t| matches!(t, Type::Array(_))).ok_or_else(err)?;
            resolved(super::lenient("array_cat", array_cat), vec![a.clone(), a.clone()], a)
        }
        "array_remove" if n == 2 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            resolved(super::lenient("array_remove", array_remove), vec![a.clone(), elem_of(&a)], a)
        }
        "array_replace" if n == 3 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            resolved(super::lenient("array_replace", array_replace), vec![a.clone(), elem_of(&a), elem_of(&a)], a)
        }
        "array_position" if n == 2 || n == 3 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            let mut t = vec![a.clone(), elem_of(&a)];
            if n == 3 {
                t.push(Type::Int4);
            }
            resolved(super::lenient("array_position", array_position), t, Type::Int4)
        }
        "array_positions" if n == 2 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            resolved(super::lenient("array_positions", array_positions), vec![a.clone(), elem_of(&a)], Type::array_of(Type::Int4))
        }
        "array_to_string" if n == 2 || n == 3 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            let mut t = vec![a, Type::Text];
            if n == 3 {
                t.push(Type::Text);
            }
            resolved(super::lenient("array_to_string", array_to_string), t, Type::Text)
        }
        "string_to_array" if n == 2 || n == 3 => {
            resolved(super::lenient("string_to_array", string_to_array), vec![Type::Text; n], Type::array_of(Type::Text))
        }
        "unnest" | "generate_series" | "generate_subscripts" => Err(SqlError::feature_not_supported(format!(
            "{name}() must be used in FROM (set-returning functions in the select list are not supported by pgmem yet)"
        ))),
        _ => Ok(None),
    }
}

/// Output column types, argument coercion targets and implementation of a set-returning function.
pub type SrfResolution = (Vec<Type>, Vec<Type>, SrfFn);

/// Resolves a set-returning function in FROM: output column types and implementation, plus
/// the argument coercion targets.
pub fn resolve_srf(name: &str, args: &[Type]) -> SqlResult<Option<SrfResolution>> {
    match name {
        "unnest" => {
            if args.is_empty() {
                return Err(no_function(name, args));
            }
            let mut outs = Vec::new();
            let mut targets = Vec::new();
            for a in args {
                let t = match a {
                    Type::Array(_) => a.clone(),
                    Type::Unknown => Type::array_of(Type::Text),
                    _ => return Err(no_function(name, args)),
                };
                outs.push(elem_of(&t));
                targets.push(t);
            }
            Ok(Some((outs, targets, unnest)))
        }
        "generate_series" => {
            let t = match args {
                [a, b] | [a, b, _] | [a, b, _, _] => {
                    let base = super::common_type(a, b).ok_or_else(|| no_function(name, args))?;
                    match base {
                        Type::Int2 | Type::Int4 | Type::Unknown => Type::Int4,
                        Type::Int8 => Type::Int8,
                        Type::Numeric => Type::Numeric,
                        Type::Float4 | Type::Float8 => Type::Numeric,
                        Type::Timestamptz | Type::Timestamp => base,
                        Type::Date => Type::Timestamptz,
                        _ => return Err(no_function(name, args)),
                    }
                }
                _ => return Err(no_function(name, args)),
            };
            if args.len() == 4 && t != Type::Timestamptz {
                return Err(no_function(name, args));
            }
            let mut targets = vec![t.clone(), t.clone()];
            if args.len() >= 3 {
                let step = match t {
                    Type::Int8 => {
                        if args[2] == Type::Int8 {
                            Type::Int8
                        } else {
                            super::numeric_common(&args[2], &Type::Int8).filter(|x| x.is_integer()).unwrap_or(Type::Int8)
                        }
                    }
                    Type::Int4 => {
                        if args[2] == Type::Int8 {
                            targets = vec![Type::Int8, Type::Int8];
                            Type::Int8
                        } else {
                            Type::Int4
                        }
                    }
                    Type::Numeric => Type::Numeric,
                    _ => Type::Interval,
                };
                targets.push(step);
                if args.len() == 4 {
                    targets.push(Type::Text);
                }
            } else if matches!(t, Type::Timestamptz | Type::Timestamp) {
                return Err(no_function(name, args));
            }
            let out = targets[0].clone();
            Ok(Some((vec![out], targets, generate_series)))
        }
        "generate_subscripts" if args.len() == 2 => {
            Ok(Some((vec![Type::Int4], vec![arr_type(&args[0]).unwrap_or(Type::array_of(Type::Text)), Type::Int4], generate_subscripts)))
        }
        _ => Ok(None),
    }
}

fn unnest(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    let max = a.iter().map(|v| items(v).len()).max().unwrap_or(0);
    Ok((0..max).map(|i| a.iter().map(|v| items(v).get(i).cloned().unwrap_or(Value::Null)).collect()).collect())
}

fn generate_series(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a.iter().any(Value::is_null) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    match (&a[0], &a[1]) {
        (Value::Timestamptz(_) | Value::Timestamp(_), _) => {
            let Some(Value::Interval(step)) = a.get(2) else { return Err(SqlError::internal("generate_series step")) };
            if step.total_micros_i128() == 0 {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
            }
            let (start, stop, tz) = match (&a[0], &a[1]) {
                (Value::Timestamptz(x), Value::Timestamptz(y)) => (*x, *y, true),
                (Value::Timestamp(x), Value::Timestamp(y)) => (*x, *y, false),
                _ => return Err(SqlError::internal("generate_series bounds")),
            };
            let up = step.total_micros_i128() > 0;
            let mut cur = start;
            while (up && cur <= stop) || (!up && cur >= stop) {
                out.push(vec![if tz { Value::Timestamptz(cur) } else { Value::Timestamp(cur) }]);
                cur = match a.get(3) {
                    // The 4-argument form steps in the given zone's local time (Postgres 16).
                    Some(z) => {
                        let zone = pgmem_types::zone::Zone::from_spec(z.as_str().unwrap_or(""))
                            .ok_or_else(|| SqlError::new(code::INVALID_PARAMETER_VALUE, format!("time zone \"{}\" not recognized", z.as_str().unwrap_or(""))))?;
                        let _ctx = pgmem_types::context::enter(pgmem_types::context::Context { zone, ..pgmem_types::context::current() });
                        super::datetime::tstz_add_interval(cur, step)?
                    }
                    None if tz => super::datetime::tstz_add_interval(cur, step)?,
                    None => add_interval(cur, step)?,
                };
                if out.len() > 10_000_000 {
                    return Err(SqlError::new("54000", "generate_series produced too many rows"));
                }
            }
        }
        (Value::Numeric(_), _) => {
            let num = |v: &Value| match v {
                Value::Numeric(n) => n.clone(),
                o => Numeric::from_i64(o.as_i64().unwrap_or(0)),
            };
            let (start, stop) = (num(&a[0]), num(&a[1]));
            let step = a.get(2).map_or_else(|| Numeric::from_i64(1), num);
            if step.is_zero() {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
            }
            let up = step.signum() > 0;
            let mut cur = start;
            while (up && cur.cmp_num(&stop).is_le()) || (!up && cur.cmp_num(&stop).is_ge()) {
                out.push(vec![Value::Numeric(cur.clone())]);
                cur = cur.add(&step);
            }
        }
        (x, y) => {
            let (start, stop) = (x.as_i64().unwrap_or(0), y.as_i64().unwrap_or(0));
            let step = a.get(2).and_then(Value::as_i64).unwrap_or(1);
            if step == 0 {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "step size cannot equal zero"));
            }
            let wide = matches!(x, Value::Int8(_));
            let mut cur = start;
            while (step > 0 && cur <= stop) || (step < 0 && cur >= stop) {
                out.push(vec![if wide { Value::Int8(cur) } else { Value::Int4(i32::try_from(cur).unwrap_or(0)) }]);
                match cur.checked_add(step) {
                    Some(n) => cur = n,
                    None => break,
                }
                if out.len() > 10_000_000 {
                    return Err(SqlError::new("54000", "generate_series produced too many rows"));
                }
            }
        }
    }
    Ok(out)
}

fn generate_subscripts(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    if a[1].as_i64() != Some(1) {
        return Ok(Vec::new());
    }
    Ok((1..=items(&a[0]).len()).map(|i| vec![len_i32(i)]).collect())
}

fn array_length(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let n = items(&a[0]).len();
    Ok(if a[1].as_i64() == Some(1) && n > 0 { len_i32(n) } else { Value::Null })
}

fn array_lower(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(if a[1].as_i64() == Some(1) && !items(&a[0]).is_empty() { Value::Int4(1) } else { Value::Null })
}

fn array_append(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut v = items(&a[0]).to_vec();
    v.push(a[1].clone());
    Ok(Value::Array(v))
}

fn array_prepend(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut v = vec![a[0].clone()];
    v.extend_from_slice(items(&a[1]));
    Ok(Value::Array(v))
}

fn array_cat(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    match (&a[0], &a[1]) {
        (Value::Null, Value::Null) => Ok(Value::Null),
        (x, y) => Ok(Value::Array([items(x), items(y)].concat())),
    }
}

fn same(x: &Value, y: &Value) -> bool {
    match (x.is_null(), y.is_null()) {
        (true, true) => true,
        (false, false) => compare_values(x, y).is_ok_and(|o| o.is_eq()),
        _ => false,
    }
}

fn array_remove(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    Ok(Value::Array(items(&a[0]).iter().filter(|v| !same(v, &a[1])).cloned().collect()))
}

fn array_replace(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    Ok(Value::Array(items(&a[0]).iter().map(|v| if same(v, &a[1]) { a[2].clone() } else { v.clone() }).collect()))
}

fn array_position(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let start = a.get(2).and_then(Value::as_i64).unwrap_or(1).max(1);
    let skip = usize::try_from(start - 1).unwrap_or(0);
    Ok(items(&a[0]).iter().enumerate().skip(skip).find(|(_, v)| same(v, &a[1])).map_or(Value::Null, |(i, _)| len_i32(i + 1)))
}

fn array_positions(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    Ok(Value::Array(items(&a[0]).iter().enumerate().filter(|(_, v)| same(v, &a[1])).map(|(i, _)| len_i32(i + 1)).collect()))
}

fn array_to_string(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() || a[1].is_null() {
        return Ok(Value::Null);
    }
    let delim = a[1].as_str().unwrap_or("");
    let null_str = a.get(2).and_then(Value::as_str);
    let mut parts = Vec::new();
    for v in items(&a[0]) {
        if v.is_null() {
            if let Some(n) = null_str {
                parts.push(n.to_owned());
            }
        } else {
            parts.push(super::string::any_text(v)?);
        }
    }
    Ok(Value::Text(parts.join(delim)))
}

fn string_to_array(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let Value::Text(src) = &a[0] else { return Ok(Value::Null) };
    if src.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    let null_str = a.get(2).and_then(Value::as_str);
    let to_val = |p: &str| if Some(p) == null_str { Value::Null } else { Value::Text(p.to_owned()) };
    Ok(Value::Array(match &a[1] {
        Value::Null => src.chars().map(|c| to_val(&c.to_string())).collect(),
        Value::Text(d) if d.is_empty() => vec![to_val(src)],
        Value::Text(d) => src.split(d.as_str()).map(to_val).collect(),
        _ => Vec::new(),
    }))
}

fn contains(hay: &[Value], needles: &[Value]) -> bool {
    needles.iter().all(|n| !n.is_null() && hay.iter().any(|h| !h.is_null() && same(h, n)))
}

fn op_contains(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(contains(items(&a[0]), items(&a[1]))))
}

fn op_contained(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(contains(items(&a[1]), items(&a[0]))))
}

fn op_overlap(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(items(&a[0]).iter().any(|x| !x.is_null() && items(&a[1]).iter().any(|y| same(x, y)))))
}

pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    let (la, ra) = (matches!(l, Type::Array(_)), matches!(r, Type::Array(_)));
    if !la && !ra {
        return Ok(None);
    }
    match op {
        "@>" | "<@" | "&&" => {
            let t = super::common_type(l, r).filter(|t| matches!(t, Type::Array(_))).ok_or_else(|| no_operator(op, Some(l), r))?;
            let f = match op {
                "@>" => strict("@>", op_contains),
                "<@" => strict("<@", op_contained),
                _ => strict("&&", op_overlap),
            };
            resolved(f, vec![t.clone(), t], Type::Bool)
        }
        "||" => {
            if la && (ra || *r == Type::Unknown) {
                let t = super::common_type(l, r).ok_or_else(|| no_operator(op, Some(l), r))?;
                resolved(super::lenient("||", array_cat), vec![t.clone(), t.clone()], t)
            } else if la {
                resolved(super::lenient("||", array_append), vec![l.clone(), elem_of(l)], l.clone())
            } else {
                resolved(super::lenient("||", array_prepend), vec![elem_of(r), r.clone()], r.clone())
            }
        }
        _ => Ok(None),
    }
}

#[allow(dead_code)]
fn unused_err() -> SqlError {
    SqlError::internal("unused")
}
