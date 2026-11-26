//! Array functions and operators, and the set-returning `unnest` / `generate_series`.

use super::{Resolved, no_function, no_operator, resolved, strict};
use crate::eval::EvalCtx;
use crate::plan::{Row, SrfFn};
use pgmem_types::datetime::add_interval;
use pgmem_types::{Numeric, SqlError, SqlResult, Type, Value, code, compare_values};

fn items(v: &Value) -> &[Value] {
    match v {
        Value::Array(i) | Value::BoundedArray(_, i) => i,
        _ => &[],
    }
}

/// The lower bound of dimension `k` (0-based).
fn lower_bound(v: &Value, k: usize) -> i64 {
    match v {
        Value::BoundedArray(lb, _) => i64::from(lb.get(k).copied().unwrap_or(1)),
        _ => 1,
    }
}

/// The dimensions of an array value (`[]` when empty).
fn dims(v: &Value) -> Vec<usize> {
    let it = items(v);
    if it.is_empty() { Vec::new() } else { pgmem_types::binary::array_dims(it) }
}

/// Elements in storage order (all dimensions flattened).
fn flat(v: &Value) -> Vec<Value> {
    pgmem_types::binary::array_flatten(items(v)).into_iter().cloned().collect()
}

fn is_multi(v: &Value) -> bool {
    dims(v).len() > 1
}

fn incompatible() -> SqlError {
    SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "cannot concatenate incompatible arrays")
}

fn len_i32(n: usize) -> Value {
    Value::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

fn elem_of(t: &Type) -> Type {
    t.element().cloned().unwrap_or(Type::Unknown)
}

fn arr_type(t: &Type) -> Option<Type> {
    match t {
        Type::Array(_) | Type::Int2Vector | Type::OidVector => Some(t.clone()),
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
                "array_upper" => strict("array_upper", array_upper),
                _ => strict("array_lower", array_lower),
            };
            resolved(f, vec![a, Type::Int4], Type::Int4)
        }
        "cardinality" if n == 1 => resolved(
            strict("cardinality", |a, _| Ok(len_i32(dims(&a[0]).iter().product::<usize>() * usize::from(!items(&a[0]).is_empty())))),
            vec![arr_type(&args[0]).ok_or_else(err)?],
            Type::Int4,
        ),
        "array_ndims" if n == 1 => resolved(
            strict("array_ndims", |a, _| {
                Ok(match dims(&a[0]).len() {
                    0 => Value::Null,
                    d => len_i32(d),
                })
            }),
            vec![arr_type(&args[0]).ok_or_else(err)?],
            Type::Int4,
        ),
        "array_dims" if n == 1 => resolved(
            strict("array_dims", |a, _| {
                let d = dims(&a[0]);
                let text: String = d
                    .iter()
                    .enumerate()
                    .map(|(k, n)| {
                        let lo = lower_bound(&a[0], k);
                        format!("[{lo}:{}]", lo + i64::try_from(*n).unwrap_or(0) - 1)
                    })
                    .collect();
                Ok(if d.is_empty() { Value::Null } else { Value::Text(text) })
            }),
            vec![arr_type(&args[0]).ok_or_else(err)?],
            Type::Text,
        ),
        "array_fill" if n == 2 || n == 3 => {
            let t = if args[0] == Type::Unknown { Type::Text } else { args[0].clone() };
            let mut targets = vec![t.clone(), Type::array_of(Type::Int4)];
            if n == 3 {
                targets.push(Type::array_of(Type::Int4));
            }
            resolved(super::lenient("array_fill", array_fill), targets, Type::array_of(t))
        }
        "trim_array" if n == 2 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            resolved(strict("trim_array", trim_array), vec![a.clone(), Type::Int4], a)
        }
        "array_sample" if n == 2 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            resolved(strict("array_sample", array_sample), vec![a.clone(), Type::Int4], a)
        }
        "array_shuffle" | "array_reverse" if n == 1 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            let f = if name == "array_shuffle" { strict("array_shuffle", array_shuffle) } else { strict("array_reverse", array_reverse) };
            resolved(f, vec![a.clone()], a)
        }
        "array_sort" if (1..=3).contains(&n) => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            let mut t = vec![a.clone()];
            t.extend(std::iter::repeat_n(Type::Bool, n - 1));
            resolved(strict("array_sort", array_sort), t, a)
        }
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
        "__pgmem_array_set" | "__pgmem_array_set_slice" if n >= 4 => {
            let a = arr_type(&args[0]).ok_or_else(err)?;
            let v = if name == "__pgmem_array_set" { elem_of(&a) } else { a.clone() };
            let mut t = vec![a.clone(), v, Type::Text];
            t.extend(std::iter::repeat_n(Type::Int4, n - 3));
            resolved(super::lenient("array_set", array_set), t, a)
        }
        "unnest" | "generate_series" | "generate_subscripts" => Err(SqlError::feature_not_supported(format!(
            "{name}() must be used in FROM (set-returning functions in the select list are not supported by pgmem yet)"
        ))),
        _ => Ok(None),
    }
}

/// Output column types, argument coercion targets and implementation of a set-returning function.
pub type SrfResolution = (Vec<Type>, Vec<Type>, SrfFn);

/// Output column names of builtin set-returning functions with several columns.
pub fn srf_column_names(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "pg_partition_tree" => Some(&["relid", "parentrelid", "isleaf", "level"]),
        "pg_partition_ancestors" => Some(&["relid"]),
        "pg_get_keywords" => Some(&["word", "catcode", "barelabel", "catdesc", "baredesc"]),
        _ => None,
    }
}

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
                    Type::Array(_) | Type::Int2Vector | Type::OidVector => a.clone(),
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
        "pg_get_keywords" if args.is_empty() => {
            Ok(Some((vec![Type::Text, Type::Char, Type::Bool, Type::Text, Type::Text], vec![], pg_get_keywords)))
        }
        "pg_listening_channels" if args.is_empty() => Ok(Some((vec![Type::Text], vec![], pg_listening_channels))),
        "pg_partition_ancestors" if args.len() == 1 => {
            let reg = Type::Reg(pgmem_types::RegKind::Class);
            Ok(Some((vec![reg.clone()], vec![reg], crate::partition::ancestors_srf)))
        }
        "pg_partition_tree" if args.len() == 1 => {
            let reg = Type::Reg(pgmem_types::RegKind::Class);
            Ok(Some((vec![reg.clone(), reg.clone(), Type::Bool, Type::Int4], vec![reg], crate::partition::tree_srf)))
        }
        "string_to_table" if args.len() == 2 || args.len() == 3 => {
            Ok(Some((vec![Type::Text], vec![Type::Text; args.len()], string_to_table)))
        }
        "regexp_matches" | "regexp_split_to_table" if args.len() == 2 || args.len() == 3 => {
            if !args.iter().all(|t| *t == Type::Unknown || super::implicit_ok(t, &Type::Text)) {
                return Err(no_function(name, args));
            }
            let (out, f): (Type, SrfFn) = if name == "regexp_matches" {
                (Type::array_of(Type::Text), super::string::regexp_matches)
            } else {
                (Type::Text, super::string::regexp_split_to_table)
            };
            Ok(Some((vec![out], vec![Type::Text; args.len()], f)))
        }
        "generate_subscripts" if args.len() == 2 || args.len() == 3 => {
            let mut t = vec![arr_type(&args[0]).unwrap_or(Type::array_of(Type::Text)), Type::Int4];
            if args.len() == 3 {
                t.push(Type::Bool);
            }
            // int2vector / oidvector subscripts start at 0.
            let f: SrfFn =
                if matches!(args[0], Type::Int2Vector | Type::OidVector) { generate_subscripts_zero } else { generate_subscripts };
            Ok(Some((vec![Type::Int4], t, f)))
        }
        _ => Ok(None),
    }
}

/// `unnest(a [, b ...])`: elements in storage order, the shorter arrays padded with NULLs.
fn unnest(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    let cols: Vec<Vec<Value>> = a.iter().map(flat).collect();
    let max = cols.iter().map(Vec::len).max().unwrap_or(0);
    Ok((0..max).map(|i| cols.iter().map(|c| c.get(i).cloned().unwrap_or(Value::Null)).collect()).collect())
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
                        let zone = pgmem_types::zone::Zone::from_spec(z.as_str().unwrap_or("")).ok_or_else(|| {
                            SqlError::new(
                                code::INVALID_PARAMETER_VALUE,
                                format!("time zone \"{}\" not recognized", z.as_str().unwrap_or("")),
                            )
                        })?;
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
    let d = dims(&a[0]);
    let Some(len) = a[1].as_i64().and_then(|k| usize::try_from(k - 1).ok()).and_then(|k| d.get(k)) else { return Ok(Vec::new()) };
    let reverse = a.get(2).and_then(Value::as_bool) == Some(true);
    let lo = lower_bound(&a[0], dim_index(a));
    let mut out: Vec<Row> = (0..i64::try_from(*len).unwrap_or(0)).map(|i| vec![int4(lo + i)]).collect();
    if reverse {
        out.reverse();
    }
    Ok(out)
}

fn generate_subscripts_zero(a: &[Value], c: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(generate_subscripts(a, c)?.into_iter().map(|r| r.into_iter().map(|v| int4(v.as_i64().unwrap_or(1) - 1)).collect()).collect())
}

/// The length of dimension `a[1]` (1-based), NULL when there is none.
fn dim_len(a: &[Value]) -> Option<usize> {
    let d = dims(&a[0]);
    let k = usize::try_from(a[1].as_i64()? - 1).ok()?;
    d.get(k).copied()
}

fn array_length(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(dim_len(a).map_or(Value::Null, len_i32))
}

fn dim_index(a: &[Value]) -> usize {
    a[1].as_i64().and_then(|k| usize::try_from(k - 1).ok()).unwrap_or(0)
}

fn int4(n: i64) -> Value {
    Value::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

fn array_lower(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(dim_len(a).map_or(Value::Null, |_| int4(lower_bound(&a[0], dim_index(a)))))
}

fn array_upper(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(dim_len(a).map_or(Value::Null, |n| int4(lower_bound(&a[0], dim_index(a)) + i64::try_from(n).unwrap_or(0) - 1)))
}

fn one_dimensional(v: &Value) -> SqlResult<()> {
    if is_multi(v) {
        return Err(SqlError::new(code::DATA_EXCEPTION, "argument must be empty or one-dimensional array"));
    }
    Ok(())
}

fn array_append(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    one_dimensional(&a[0])?;
    let mut v = items(&a[0]).to_vec();
    v.push(a[1].clone());
    Ok(Value::Array(v))
}

fn array_prepend(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    one_dimensional(&a[1])?;
    let mut v = vec![a[0].clone()];
    v.extend_from_slice(items(&a[1]));
    Ok(Value::Array(v))
}

/// `array_cat` / `||`: arrays of equal rank join along the first dimension; an array of one
/// rank less becomes a new element of the other. Inner dimensions must match.
fn array_cat(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (x, y) = (&a[0], &a[1]);
    match (x, y) {
        (Value::Null, Value::Null) => return Ok(Value::Null),
        (Value::Null, other) | (other, Value::Null) => return Ok(other.clone()),
        _ => {}
    }
    let (dx, dy) = (dims(x), dims(y));
    if dx.is_empty() {
        return Ok(y.clone());
    }
    if dy.is_empty() {
        return Ok(x.clone());
    }
    if dx.len() == dy.len() {
        if dx[1..] != dy[1..] {
            return Err(incompatible().with_detail("Arrays with differing element dimensions are not compatible for concatenation."));
        }
        return Ok(Value::Array([items(x), items(y)].concat()));
    }
    if dx.len() == dy.len() + 1 {
        if dx[1..] != dy[..] {
            return Err(incompatible().with_detail("Arrays with differing dimensions are not compatible for concatenation."));
        }
        let mut v = items(x).to_vec();
        v.push(y.clone());
        return Ok(Value::Array(v));
    }
    if dy.len() == dx.len() + 1 {
        if dy[1..] != dx[..] {
            return Err(incompatible().with_detail("Arrays with differing dimensions are not compatible for concatenation."));
        }
        let mut v = vec![x.clone()];
        v.extend_from_slice(items(y));
        return Ok(Value::Array(v));
    }
    Err(incompatible().with_detail(format!("Arrays of {} and {} dimensions are not compatible for concatenation.", dx.len(), dy.len())))
}

/// `array_fill(value, dims [, lower_bounds])`.
fn array_fill(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[1].is_null() {
        return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "dimension array or low bound array cannot be null"));
    }
    let d: Vec<usize> = items(&a[1]).iter().map(|v| v.as_i64().and_then(|n| usize::try_from(n).ok()).unwrap_or(0)).collect();
    let lower: Vec<i32> = match a.get(2) {
        Some(Value::Null) => return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "dimension array or low bound array cannot be null")),
        Some(lb) => items(lb).iter().map(|v| v.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(1)).collect(),
        None => Vec::new(),
    };
    if !lower.is_empty() && lower.len() != d.len() {
        return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts")
            .with_detail("Low bound array has different size than dimensions array."));
    }
    fn build(v: &Value, d: &[usize]) -> Vec<Value> {
        match d {
            [] => Vec::new(),
            [n] => vec![v.clone(); *n],
            [n, rest @ ..] => (0..*n).map(|_| Value::Array(build(v, rest))).collect(),
        }
    }
    if d.contains(&0) {
        return Ok(Value::Array(Vec::new()));
    }
    Ok(Value::array_with_bounds(lower, build(&a[0], &d)))
}

fn trim_array(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let v = items(&a[0]);
    let n = a[1].as_i64().unwrap_or(0);
    let len = i64::try_from(v.len()).unwrap_or(i64::MAX);
    if n < 0 || n > len {
        return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, format!("number of elements to trim must be between 0 and {len}")));
    }
    Ok(Value::Array(v[..usize::try_from(len - n).unwrap_or(0)].to_vec()))
}

fn random_index(bound: usize) -> SqlResult<usize> {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| SqlError::internal(format!("random: {e}")))?;
    Ok(usize::try_from(u64::from_le_bytes(buf) % u64::try_from(bound.max(1)).unwrap_or(1)).unwrap_or(0))
}

fn shuffled(mut v: Vec<Value>) -> SqlResult<Vec<Value>> {
    for i in (1..v.len()).rev() {
        let j = random_index(i + 1)?;
        v.swap(i, j);
    }
    Ok(v)
}

fn array_shuffle(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Array(shuffled(items(&a[0]).to_vec())?))
}

fn array_sample(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let v = items(&a[0]);
    let n = a[1].as_i64().unwrap_or(0);
    let len = i64::try_from(v.len()).unwrap_or(i64::MAX);
    if n < 0 || n > len {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("sample size must be between 0 and {len}")));
    }
    let mut s = shuffled(v.to_vec())?;
    s.truncate(usize::try_from(n).unwrap_or(0));
    Ok(Value::Array(s))
}

fn array_reverse(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut v = items(&a[0]).to_vec();
    v.reverse();
    Ok(Value::Array(v))
}

/// `array_sort(a [, descending [, nulls_first]])` (Postgres 18), on the first dimension.
fn array_sort(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let desc = a.get(1).and_then(Value::as_bool) == Some(true);
    let nulls_first = a.get(2).and_then(Value::as_bool).unwrap_or(desc);
    let mut v = items(&a[0]).to_vec();
    let mut err = None;
    v.sort_by(|x, y| match (x.is_null(), y.is_null()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => {
            if nulls_first {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        (false, false) => {
            let o = compare_values(x, y).unwrap_or_else(|e| {
                err = Some(e);
                std::cmp::Ordering::Equal
            });
            if desc { o.reverse() } else { o }
        }
    });
    if let Some(e) = err {
        return Err(e);
    }
    Ok(Value::Array(v))
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
    if is_multi(&a[0]) {
        return Err(SqlError::feature_not_supported("removing elements from multidimensional arrays is not supported"));
    }
    Ok(Value::Array(items(&a[0]).iter().filter(|v| !same(v, &a[1])).cloned().collect()))
}

fn array_replace(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    fn replace(items: &[Value], from: &Value, to: &Value) -> Vec<Value> {
        items
            .iter()
            .map(|v| match v {
                Value::Array(inner) => Value::Array(replace(inner, from, to)),
                v if same(v, from) => to.clone(),
                v => v.clone(),
            })
            .collect()
    }
    Ok(Value::Array(replace(items(&a[0]), &a[1], &a[2])))
}

fn searching_multi(v: &Value) -> SqlResult<()> {
    if is_multi(v) {
        return Err(SqlError::feature_not_supported("searching for elements in multidimensional arrays is not supported"));
    }
    Ok(())
}

fn array_position(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    searching_multi(&a[0])?;
    let start = a.get(2).and_then(Value::as_i64).unwrap_or(1).max(1);
    let skip = usize::try_from(start - 1).unwrap_or(0);
    Ok(items(&a[0]).iter().enumerate().skip(skip).find(|(_, v)| same(v, &a[1])).map_or(Value::Null, |(i, _)| len_i32(i + 1)))
}

fn array_positions(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    searching_multi(&a[0])?;
    Ok(Value::Array(items(&a[0]).iter().enumerate().filter(|(_, v)| same(v, &a[1])).map(|(i, _)| len_i32(i + 1)).collect()))
}

fn array_to_string(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() || a[1].is_null() {
        return Ok(Value::Null);
    }
    let delim = a[1].as_str().unwrap_or("");
    let null_str = a.get(2).and_then(Value::as_str);
    let mut parts = Vec::new();
    for v in &flat(&a[0]) {
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

fn pg_listening_channels(_: &[Value], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(ctx.env.objects.listening.iter().map(|c| vec![Value::Text(c.clone())]).collect())
}

/// PostgreSQL 18's keyword list (word, catcode, barelabel, catdesc, baredesc), recorded from the server.
pub(super) const KEYWORDS: &str = include_str!("../catalog_views/pg_keywords.tsv");

/// `pg_get_keywords()`: PostgreSQL 18's keyword list (recorded from the server), in its order.
fn pg_get_keywords(_: &[Value], _: &EvalCtx) -> SqlResult<Vec<Row>> {
    Ok(KEYWORDS
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            vec![
                Value::Text(f[0].to_owned()),
                Value::Text(f[1].to_owned()),
                Value::Bool(f[2] == "t"),
                Value::Text(f[3].to_owned()),
                Value::Text(f[4].to_owned()),
            ]
        })
        .collect())
}

fn string_to_table(a: &[Value], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    match string_to_array(a, ctx)? {
        Value::Array(items) => Ok(items.into_iter().map(|v| vec![v]).collect()),
        _ => Ok(Vec::new()),
    }
}

fn contains(hay: &[Value], needles: &[Value]) -> bool {
    needles.iter().all(|n| !n.is_null() && hay.iter().any(|h| !h.is_null() && same(h, n)))
}

fn op_contains(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(contains(&flat(&a[0]), &flat(&a[1]))))
}

fn op_contained(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(contains(&flat(&a[1]), &flat(&a[0]))))
}

fn op_overlap(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let r = flat(&a[1]);
    Ok(Value::Bool(flat(&a[0]).iter().any(|x| !x.is_null() && r.iter().any(|y| same(x, y)))))
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
                if super::common_type(&elem_of(l), r).is_none() {
                    return Err(no_operator(op, Some(l), r));
                }
                resolved(super::lenient("||", array_append), vec![l.clone(), elem_of(l)], l.clone())
            } else {
                if *l != Type::Unknown && super::common_type(l, &elem_of(r)).is_none() {
                    return Err(no_operator(op, Some(l), r));
                }
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

/// Subscripted assignment (`arr[i] = v`, `arr[lo:hi] = sub`): args are the array, the new
/// value, the subscript kinds (`i` index / `r` range) and their bounds.
fn array_set(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let flags: Vec<char> = a[2].as_str().unwrap_or("").chars().collect();
    let mut bounds = a[3..].iter();
    let mut subs: Vec<(Option<i64>, Option<i64>, bool)> = Vec::new();
    for f in &flags {
        let lo = bounds.next().and_then(Value::as_i64);
        if *f == 'r' {
            let hi = bounds.next().and_then(Value::as_i64);
            subs.push((lo, hi, true));
        } else {
            if lo.is_none() {
                return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "array subscript in assignment must not be null"));
            }
            subs.push((lo, None, false));
        }
    }
    let base = match &a[0] {
        Value::Null => Vec::new(),
        v => items(v).to_vec(),
    };
    let out_of_range = || SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "array subscript out of range");
    if flags.iter().all(|f| *f == 'i') {
        return Ok(Value::Array(set_element(base, &subs, a[1].clone(), &out_of_range)?));
    }
    // Slice assignment: replace the addressed block with the source's elements in order.
    let source: Vec<Value> = match &a[1] {
        Value::Null => return Ok(a[0].clone()),
        v => flat(v),
    };
    let mut src = source.into_iter();
    let shape = if base.is_empty() { Vec::new() } else { pgmem_types::binary::array_dims(&base) };
    if shape.len() > 1 && shape.len() != subs.len() {
        return Err(out_of_range());
    }
    fn assign(
        items: &mut Vec<Value>,
        subs: &[(Option<i64>, Option<i64>, bool)],
        src: &mut std::vec::IntoIter<Value>,
        err: &dyn Fn() -> SqlError,
    ) -> SqlResult<()> {
        let Some(((lo, hi, range), rest)) = subs.split_first() else { return Ok(()) };
        let len = i64::try_from(items.len()).unwrap_or(i64::MAX);
        let (lo, hi) = if *range { (lo.unwrap_or(1), hi.unwrap_or(len)) } else { (lo.unwrap_or(1), lo.unwrap_or(1)) };
        if lo < 1 || hi < lo {
            return Err(err());
        }
        if rest.is_empty() {
            // One dimension may grow (padding with NULLs).
            while i64::try_from(items.len()).unwrap_or(i64::MAX) < hi {
                items.push(Value::Null);
            }
            for i in lo..=hi {
                items[usize::try_from(i - 1).unwrap_or(0)] =
                    src.next().ok_or_else(|| SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "source array too small"))?;
            }
            return Ok(());
        }
        if hi > len {
            return Err(err());
        }
        for i in lo..=hi {
            match &mut items[usize::try_from(i - 1).unwrap_or(0)] {
                Value::Array(inner) => assign(inner, rest, src, err)?,
                _ => return Err(err()),
            }
        }
        Ok(())
    }
    let mut out = base;
    assign(&mut out, &subs, &mut src, &out_of_range)?;
    Ok(Value::Array(out))
}

/// `arr[i]...[k] = v`: a one-dimensional array grows to `i` (NULL-padded); a multidimensional
/// one must already have the element.
fn set_element(
    mut items: Vec<Value>,
    subs: &[(Option<i64>, Option<i64>, bool)],
    v: Value,
    err: &dyn Fn() -> SqlError,
) -> SqlResult<Vec<Value>> {
    let Some(((Some(i), _, _), rest)) = subs.split_first() else { return Err(err()) };
    if *i < 1 {
        return Err(SqlError::feature_not_supported("array lower bounds other than 1 are not supported by pgmem"));
    }
    let idx = usize::try_from(i - 1).unwrap_or(usize::MAX);
    if rest.is_empty() {
        if items.first().is_some_and(|x| matches!(x, Value::Array(_))) {
            return Err(err());
        }
        while items.len() <= idx {
            items.push(Value::Null);
        }
        items[idx] = v;
        return Ok(items);
    }
    match items.get_mut(idx) {
        Some(Value::Array(inner)) => {
            let updated = set_element(std::mem::take(inner), rest, v, err)?;
            *inner = updated;
            Ok(items)
        }
        _ => Err(err()),
    }
}
