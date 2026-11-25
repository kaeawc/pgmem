//! Range and multirange functions and operators (`rangetypes.c`, `multirangetypes.c`).

use super::{Resolved, no_function, no_operator, resolved, strict};
use crate::eval::EvalCtx;
use pgmem_types::range::{self, Range};
use pgmem_types::{SqlError, SqlResult, Type, Value, code};

fn is_range(t: &Type) -> bool {
    matches!(t, Type::Range(_))
}

fn is_multi(t: &Type) -> bool {
    matches!(t, Type::Multirange(_))
}

fn subtype(t: &Type) -> Type {
    match t {
        Type::Range(s) | Type::Multirange(s) => (**s).clone(),
        other => other.clone(),
    }
}

fn rng(v: &Value) -> SqlResult<&Range> {
    range::of(v)
}

fn multi(v: &Value) -> SqlResult<&[Range]> {
    match v {
        Value::Multirange(_, rs) => Ok(rs),
        _ => Err(SqlError::internal("multirange value expected")),
    }
}

fn range_value(r: Range) -> Value {
    Value::Range(Box::new(r))
}

fn bounds_flags(flags: &str) -> SqlResult<(bool, bool)> {
    let chars: Vec<char> = flags.chars().collect();
    match chars.as_slice() {
        [l @ ('[' | '('), u @ (']' | ')')] => Ok((*l == '[', *u == ']')),
        _ => Err(SqlError::syntax("invalid range bound flags").with_hint("Valid values are \"[]\", \"[)\", \"(]\", and \"()\".")),
    }
}

/// `int4range(lower, upper [, flags])` and the other constructors.
fn construct(sub: &Type, a: &[Value]) -> SqlResult<Value> {
    let (li, ui) = match a.get(2) {
        Some(Value::Null) => return Err(SqlError::new(code::DATA_EXCEPTION, "range constructor flags argument must not be null")),
        Some(f) => bounds_flags(f.as_str().unwrap_or(""))?,
        None => (true, false),
    };
    Ok(range_value(range::make(sub, Some(a[0].clone()), Some(a[1].clone()), li, ui)?))
}

/// `int4multirange(range, ...)`.
fn construct_multi(sub: &Type, a: &[Value]) -> SqlResult<Value> {
    let mut rs = Vec::new();
    for v in a {
        match v {
            Value::Null => return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "multirange values cannot contain null members")),
            Value::Array(items) => {
                for i in items {
                    if i.is_null() {
                        return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "multirange values cannot contain null members"));
                    }
                    rs.push(rng(i)?.clone());
                }
            }
            other => rs.push(rng(other)?.clone()),
        }
    }
    Ok(Value::Multirange(sub.clone(), range::normalize(rs)?))
}

macro_rules! constructors {
    ($($range:ident, $multi:ident, $sub:expr;)*) => {
        $(
            fn $range(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
                construct(&$sub, a)
            }
            fn $multi(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
                construct_multi(&$sub, a)
            }
        )*
    };
}
constructors! {
    int4range, int4multirange, Type::Int4;
    int8range, int8multirange, Type::Int8;
    numrange, nummultirange, Type::Numeric;
    daterange, datemultirange, Type::Date;
    tsrange, tsmultirange, Type::Timestamp;
    tstzrange, tstzmultirange, Type::Timestamptz;
}

/// `multirange(range)`.
fn to_multirange(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let sub = rng(&a[0])?.sub.clone();
    construct_multi(&sub, a)
}

fn lower(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let r = rng(&a[0])?;
    Ok(if r.empty { Value::Null } else { r.lower.clone().unwrap_or(Value::Null) })
}

fn upper(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let r = rng(&a[0])?;
    Ok(if r.empty { Value::Null } else { r.upper.clone().unwrap_or(Value::Null) })
}

fn flag(a: &[Value], test: fn(&Range) -> bool) -> SqlResult<Value> {
    Ok(Value::Bool(test(rng(&a[0])?)))
}

fn isempty(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    flag(a, |r| r.empty)
}
fn lower_inc(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    flag(a, |r| !r.empty && r.lower_inc)
}
fn upper_inc(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    flag(a, |r| !r.empty && r.upper_inc)
}
fn lower_inf(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    flag(a, |r| !r.empty && r.lower.is_none())
}
fn upper_inf(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    flag(a, |r| !r.empty && r.upper.is_none())
}

fn merge(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(range_value(range::union(rng(&a[0])?, rng(&a[1])?, false)?))
}

fn op_union(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(range_value(range::union(rng(&a[0])?, rng(&a[1])?, true)?))
}

fn op_intersect(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(range_value(range::intersect(rng(&a[0])?, rng(&a[1])?)?))
}

fn op_minus(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (x, y) = (rng(&a[0])?, rng(&a[1])?);
    Ok(range_value(range::canonical(&x.sub, range::minus(x, y)?)?))
}

macro_rules! range_pred {
    ($name:ident, $f:path) => {
        fn $name(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
            Ok(Value::Bool($f(rng(&a[0])?, rng(&a[1])?)?))
        }
    };
}
range_pred!(op_contains, range::contains);
range_pred!(op_overlaps, range::overlaps);
range_pred!(op_before, range::before);
range_pred!(op_overleft, range::overleft);
range_pred!(op_overright, range::overright);
range_pred!(op_adjacent, range::adjacent);

fn op_contained(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(range::contains(rng(&a[1])?, rng(&a[0])?)?))
}

fn op_after(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(range::before(rng(&a[1])?, rng(&a[0])?)?))
}

fn op_contains_elem(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(range::contains_elem(rng(&a[0])?, &a[1])?))
}

fn op_elem_contained(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(range::contains_elem(rng(&a[1])?, &a[0])?))
}

fn any_range(rs: &[Range], f: impl Fn(&Range) -> SqlResult<bool>) -> SqlResult<bool> {
    for r in rs {
        if f(r)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn multi_contains_elem(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(any_range(multi(&a[0])?, |r| range::contains_elem(r, &a[1]))?))
}

fn multi_contains_range(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let inner = rng(&a[1])?;
    Ok(Value::Bool(inner.empty || any_range(multi(&a[0])?, |r| range::contains(r, inner))?))
}

fn multi_overlaps_range(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let other = rng(&a[1])?;
    Ok(Value::Bool(any_range(multi(&a[0])?, |r| range::overlaps(r, other))?))
}

fn multi_overlaps_multi(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let other = multi(&a[1])?;
    Ok(Value::Bool(any_range(multi(&a[0])?, |r| any_range(other, |o| range::overlaps(r, o)))?))
}

fn multi_lower(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(multi(&a[0])?.first().and_then(|r| r.lower.clone()).unwrap_or(Value::Null))
}

fn multi_upper(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(multi(&a[0])?.last().and_then(|r| r.upper.clone()).unwrap_or(Value::Null))
}

fn multi_isempty(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(multi(&a[0])?.is_empty()))
}

/// `range_merge(multirange)`: the smallest range containing it.
fn multi_merge(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let Value::Multirange(sub, rs) = &a[0] else { return Err(SqlError::internal("multirange value expected")) };
    Ok(range_value(match (rs.first(), rs.last()) {
        (Some(f), Some(l)) => range::union(f, l, false)?,
        _ => Range::empty(sub),
    }))
}

type Ctor = (&'static str, Type, crate::plan::ScalarFn, crate::plan::ScalarFn);

const CONSTRUCTORS: &[Ctor] = &[
    ("int4range", Type::Int4, int4range, int4multirange),
    ("int8range", Type::Int8, int8range, int8multirange),
    ("numrange", Type::Numeric, numrange, nummultirange),
    ("daterange", Type::Date, daterange, datemultirange),
    ("tsrange", Type::Timestamp, tsrange, tsmultirange),
    ("tstzrange", Type::Timestamptz, tstzrange, tstzmultirange),
];

/// The type an argument of a range function resolves to: itself, or (for an unknown literal)
/// the range type of a sibling.
fn the_range(args: &[Type]) -> Option<Type> {
    args.iter().find(|t| is_range(t)).cloned()
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let n = args.len();
    if let Some((_, sub, f, _)) = CONSTRUCTORS.iter().find(|(c, ..)| *c == name) {
        if n == 2 || n == 3 {
            let mut types = vec![sub.clone(), sub.clone()];
            if n == 3 {
                types.push(Type::Text);
            }
            // Not strict: NULL bounds mean unbounded.
            return resolved(super::lenient("range constructor", *f), types, Type::Range(Box::new(sub.clone())));
        }
        return Err(no_function(name, args));
    }
    if let Some((_, sub, _, f)) = CONSTRUCTORS.iter().find(|(c, ..)| c.replace("range", "multirange") == name) {
        let r = Type::Range(Box::new(sub.clone()));
        return resolved(super::lenient("multirange", *f), vec![r; n], Type::Multirange(Box::new(sub.clone())));
    }
    let [arg] = args else {
        if name == "range_merge"
            && n == 2
            && let Some(t) = the_range(args)
        {
            return resolved(strict("range_merge", merge), vec![t.clone(), t.clone()], t);
        }
        return Ok(None);
    };
    if is_multi(arg) {
        let sub = subtype(arg);
        let f = match name {
            "lower" => return resolved(strict("lower", multi_lower), vec![arg.clone()], sub),
            "upper" => return resolved(strict("upper", multi_upper), vec![arg.clone()], sub),
            "isempty" => strict("isempty", multi_isempty),
            "range_merge" => return resolved(strict("range_merge", multi_merge), vec![arg.clone()], Type::Range(Box::new(sub))),
            _ => return Ok(None),
        };
        return resolved(f, vec![arg.clone()], Type::Bool);
    }
    if !is_range(arg) {
        return Ok(None);
    }
    let f = match name {
        "lower" => return resolved(strict("lower", lower), vec![arg.clone()], subtype(arg)),
        "upper" => return resolved(strict("upper", upper), vec![arg.clone()], subtype(arg)),
        "isempty" => strict("isempty", isempty),
        "lower_inc" => strict("lower_inc", lower_inc),
        "upper_inc" => strict("upper_inc", upper_inc),
        "lower_inf" => strict("lower_inf", lower_inf),
        "upper_inf" => strict("upper_inf", upper_inf),
        "multirange" => {
            let m = Type::Multirange(Box::new(subtype(arg)));
            return resolved(strict("multirange", to_multirange), vec![arg.clone()], m);
        }
        _ => return Ok(None),
    };
    resolved(f, vec![arg.clone()], Type::Bool)
}

pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    if !(is_range(l) || is_range(r) || is_multi(l) || is_multi(r)) {
        return Ok(None);
    }
    let bool_op = |f, lt: Type, rt: Type| resolved(f, vec![lt, rt], Type::Bool);
    // An unknown literal on the other side is read as the same range type.
    let (l, r) = match (l, r) {
        (Type::Unknown, t) | (t, Type::Unknown) if !matches!(op, "@>" | "<@") => (t.clone(), t.clone()),
        _ => (l.clone(), r.clone()),
    };
    if is_multi(&l) {
        let sub = subtype(&l);
        return match (op, &r) {
            ("@>", Type::Range(_)) => bool_op(strict("@>", multi_contains_range), l.clone(), r.clone()),
            ("@>", _) if r == Type::Unknown || super::common_type(&sub, &r).is_some() => {
                bool_op(strict("@>", multi_contains_elem), l.clone(), sub)
            }
            ("&&", Type::Range(_)) => bool_op(strict("&&", multi_overlaps_range), l.clone(), r.clone()),
            ("&&", Type::Multirange(_)) => bool_op(strict("&&", multi_overlaps_multi), l.clone(), r.clone()),
            _ => Err(no_operator(op, Some(&l), &r)),
        };
    }
    match op {
        "@>" if is_range(&l) && !is_range(&r) && r != Type::Unknown => {
            let sub = subtype(&l);
            super::common_type(&sub, &r).ok_or_else(|| no_operator(op, Some(&l), &r))?;
            bool_op(strict("@>", op_contains_elem), l.clone(), sub)
        }
        "<@" if is_range(&r) && !is_range(&l) && l != Type::Unknown => {
            let sub = subtype(&r);
            super::common_type(&sub, &l).ok_or_else(|| no_operator(op, Some(&l), &r))?;
            bool_op(strict("<@", op_elem_contained), sub, r.clone())
        }
        _ => {
            let t = match (&l, &r) {
                (Type::Unknown, t) | (t, Type::Unknown) => t.clone(),
                _ if l == r => l.clone(),
                _ => return Err(no_operator(op, Some(&l), &r)),
            };
            let f = match op {
                "@>" => strict("@>", op_contains),
                "<@" => strict("<@", op_contained),
                "&&" => strict("&&", op_overlaps),
                "<<" => strict("<<", op_before),
                ">>" => strict(">>", op_after),
                "&<" => strict("&<", op_overleft),
                "&>" => strict("&>", op_overright),
                "-|-" => strict("-|-", op_adjacent),
                "+" | "*" | "-" => {
                    let f = match op {
                        "+" => strict("+", op_union),
                        "*" => strict("*", op_intersect),
                        _ => strict("-", op_minus),
                    };
                    return resolved(f, vec![t.clone(), t.clone()], t);
                }
                _ => return Err(no_operator(op, Some(&l), &r)),
            };
            bool_op(f, t.clone(), t)
        }
    }
}
