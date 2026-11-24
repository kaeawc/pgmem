//! Builtin functions and operators, with Postgres-style overload resolution.
//!
//! A resolver takes the argument types (after literal/parameter inference) and returns the
//! implementation, the types each argument must be coerced to, and the result type.

pub mod array;
pub mod datetime;
pub mod json;
pub mod math;
pub mod misc;
pub mod string;

use crate::cast::{CoercionContext, can_coerce};
use crate::eval::EvalCtx;
use crate::plan::Func;
use pgmem_types::{SqlError, SqlResult, Type, Value, code, compare_values};
use std::cmp::Ordering;

/// A resolved call: implementation, argument coercion targets, result type.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub func: Func,
    pub args: Vec<Type>,
    pub ret: Type,
}

pub(crate) const fn strict(name: &'static str, f: crate::plan::ScalarFn) -> Func {
    Func { name, f, strict: true }
}

pub(crate) const fn lenient(name: &'static str, f: crate::plan::ScalarFn) -> Func {
    Func { name, f, strict: false }
}

pub(crate) fn resolved(func: Func, args: Vec<Type>, ret: Type) -> SqlResult<Option<Resolved>> {
    Ok(Some(Resolved { func, args, ret }))
}

fn type_list(types: &[Type]) -> String {
    types.iter().map(Type::name).collect::<Vec<_>>().join(", ")
}

pub fn no_function(name: &str, types: &[Type]) -> SqlError {
    SqlError::undefined_function(format!("function {name}({}) does not exist", type_list(types)))
        .with_hint("No function matches the given name and argument types. You might need to add explicit type casts.")
}

pub fn no_operator(op: &str, l: Option<&Type>, r: &Type) -> SqlError {
    let text = match l {
        Some(l) => format!("operator does not exist: {} {op} {}", l.name(), r.name()),
        None => format!("operator does not exist: {op} {}", r.name()),
    };
    SqlError::undefined_function(text)
        .with_hint("No operator matches the given name and argument types. You might need to add explicit type casts.")
}

/// Resolves a function call by name. `None` means "no such function name" (the caller may
/// try aggregates); an error means the name exists but no overload fits.
pub fn resolve_function(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    if let Some(r) = math::resolve(name, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = string::resolve(name, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = datetime::resolve(name, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = json::resolve(name, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = array::resolve(name, args)? {
        return Ok(Some(r));
    }
    misc::resolve(name, args)
}

/// Rank used to pick the common numeric type of mixed operands.
fn numeric_rank(t: &Type) -> Option<u8> {
    Some(match t {
        Type::Int2 => 1,
        Type::Int4 => 2,
        Type::Int8 => 3,
        Type::Numeric => 4,
        Type::Float4 => 5,
        Type::Float8 => 6,
        _ => return None,
    })
}

/// The type both numeric operands are promoted to: integers widen, numeric beats integers,
/// any float makes the operation float8 (float4 only when both sides are float4).
pub fn numeric_common(l: &Type, r: &Type) -> Option<Type> {
    let (a, b) = (numeric_rank(l)?, numeric_rank(r)?);
    if l == r {
        return Some(l.clone());
    }
    if a >= 5 || b >= 5 {
        return Some(Type::Float8);
    }
    Some(if a >= b { l.clone() } else { r.clone() })
}

fn datetime_rank(t: &Type) -> Option<u8> {
    Some(match t {
        Type::Date => 1,
        Type::Timestamp => 2,
        Type::Timestamptz => 3,
        _ => return None,
    })
}

/// The common type two operands are compared (or unioned) as, or None if incompatible.
pub fn common_type(l: &Type, r: &Type) -> Option<Type> {
    match (l, r) {
        (Type::Unknown, Type::Unknown) => Some(Type::Text),
        (Type::Unknown, t) | (t, Type::Unknown) => Some(t.clone()),
        _ if l == r => Some(l.clone()),
        _ => {
            if let Some(t) = numeric_common(l, r) {
                return Some(t);
            }
            if let (Some(a), Some(b)) = (datetime_rank(l), datetime_rank(r)) {
                return Some(if a >= b { l.clone() } else { r.clone() });
            }
            match (l, r) {
                (Type::Time, Type::TimeTz) | (Type::TimeTz, Type::Time) => Some(Type::TimeTz),
                (Type::Array(a), Type::Array(b)) => common_type(a, b).map(Type::array_of),
                _ => None,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------------------------

fn cmp_args(a: &[Value]) -> SqlResult<Ordering> {
    compare_values(&a[0], &a[1])
}

fn op_eq(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(cmp_args(a)?.is_eq()))
}
fn op_ne(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(!cmp_args(a)?.is_eq()))
}
fn op_lt(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(cmp_args(a)?.is_lt()))
}
fn op_le(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(cmp_args(a)?.is_le()))
}
fn op_gt(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(cmp_args(a)?.is_gt()))
}
fn op_ge(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(cmp_args(a)?.is_ge()))
}

pub const EQ: Func = strict("=", op_eq);

/// The comparison implementation for an operator name, if it is one.
pub fn comparison(op: &str) -> Option<Func> {
    Some(match op {
        "=" => EQ,
        "<>" | "!=" => strict("<>", op_ne),
        "<" => strict("<", op_lt),
        "<=" => strict("<=", op_le),
        ">" => strict(">", op_gt),
        ">=" => strict(">=", op_ge),
        _ => return None,
    })
}

fn comparable(t: &Type) -> bool {
    !matches!(t, Type::Unknown)
}

// ---------------------------------------------------------------------------------------------
// Arithmetic
// ---------------------------------------------------------------------------------------------

fn overflow(v: &Value) -> SqlError {
    match v {
        Value::Int2(_) => SqlError::out_of_range("smallint"),
        Value::Int4(_) => SqlError::out_of_range("integer"),
        Value::Int8(_) => SqlError::out_of_range("bigint"),
        _ => SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"),
    }
}

fn float_check(r: f64, a: f64, b: f64) -> SqlResult<f64> {
    if r.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"));
    }
    Ok(r)
}

macro_rules! int_arith {
    ($name:ident, $checked:ident, $fop:tt, $nfn:ident) => {
        fn $name(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
            use Value as V;
            Ok(match (&a[0], &a[1]) {
                (V::Int2(x), V::Int2(y)) => V::Int2(x.$checked(*y).ok_or_else(|| overflow(&a[0]))?),
                (V::Int4(x), V::Int4(y)) => V::Int4(x.$checked(*y).ok_or_else(|| overflow(&a[0]))?),
                (V::Int8(x), V::Int8(y)) => V::Int8(x.$checked(*y).ok_or_else(|| overflow(&a[0]))?),
                (V::Float4(x), V::Float4(y)) => {
                    let r = f64::from(*x) $fop f64::from(*y);
                    #[allow(clippy::cast_possible_truncation)]
                    V::Float4(float_check(r, f64::from(*x), f64::from(*y))? as f32)
                }
                (V::Float8(x), V::Float8(y)) => V::Float8(float_check(x $fop y, *x, *y)?),
                (V::Numeric(x), V::Numeric(y)) => V::Numeric(x.$nfn(y)),
                (x, y) => return Err(SqlError::internal(format!("arithmetic on {} and {}", x.kind(), y.kind()))),
            })
        }
    };
}

int_arith!(op_add, checked_add, +, add);
int_arith!(op_sub, checked_sub, -, sub);
int_arith!(op_mul, checked_mul, *, mul);

fn op_div(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    use Value as V;
    let zero = match &a[1] {
        V::Int2(0) | V::Int4(0) | V::Int8(0) => true,
        V::Float4(f) => *f == 0.0,
        V::Float8(f) => *f == 0.0,
        V::Numeric(n) => n.is_zero(),
        _ => false,
    };
    if zero {
        return Err(SqlError::division_by_zero());
    }
    Ok(match (&a[0], &a[1]) {
        (V::Int2(x), V::Int2(y)) => V::Int2(x.checked_div(*y).ok_or_else(|| overflow(&a[0]))?),
        (V::Int4(x), V::Int4(y)) => V::Int4(x.checked_div(*y).ok_or_else(|| overflow(&a[0]))?),
        (V::Int8(x), V::Int8(y)) => V::Int8(x.checked_div(*y).ok_or_else(|| overflow(&a[0]))?),
        #[allow(clippy::cast_possible_truncation)]
        (V::Float4(x), V::Float4(y)) => V::Float4((f64::from(*x) / f64::from(*y)) as f32),
        (V::Float8(x), V::Float8(y)) => V::Float8(float_check(x / y, *x, *y)?),
        (V::Numeric(x), V::Numeric(y)) => V::Numeric(x.div(y)?),
        (x, y) => return Err(SqlError::internal(format!("division of {} by {}", x.kind(), y.kind()))),
    })
}

fn op_mod(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    use Value as V;
    Ok(match (&a[0], &a[1]) {
        (_, V::Int2(0) | V::Int4(0) | V::Int8(0)) => return Err(SqlError::division_by_zero()),
        // i32::MIN % -1 overflows in Rust; Postgres returns 0.
        (V::Int2(x), V::Int2(y)) => V::Int2(x.checked_rem(*y).unwrap_or(0)),
        (V::Int4(x), V::Int4(y)) => V::Int4(x.checked_rem(*y).unwrap_or(0)),
        (V::Int8(x), V::Int8(y)) => V::Int8(x.checked_rem(*y).unwrap_or(0)),
        (V::Numeric(x), V::Numeric(y)) => V::Numeric(x.rem(y)?),
        (x, y) => return Err(SqlError::internal(format!("modulo of {} by {}", x.kind(), y.kind()))),
    })
}

fn op_pow(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    math::power_values(&a[0], &a[1])
}

fn op_neg(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    use Value as V;
    Ok(match &a[0] {
        V::Int2(x) => V::Int2(x.checked_neg().ok_or_else(|| overflow(&a[0]))?),
        V::Int4(x) => V::Int4(x.checked_neg().ok_or_else(|| overflow(&a[0]))?),
        V::Int8(x) => V::Int8(x.checked_neg().ok_or_else(|| overflow(&a[0]))?),
        V::Float4(x) => V::Float4(-x),
        V::Float8(x) => V::Float8(-x),
        V::Numeric(n) => V::Numeric(n.neg()),
        V::Interval(iv) => V::Interval(iv.negate()?),
        other => return Err(SqlError::internal(format!("negation of {}", other.kind()))),
    })
}

fn op_identity(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(a[0].clone())
}

macro_rules! bit_op {
    ($name:ident, $op:tt) => {
        fn $name(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
            use Value as V;
            Ok(match (&a[0], &a[1]) {
                (V::Int2(x), V::Int2(y)) => V::Int2(x $op y),
                (V::Int4(x), V::Int4(y)) => V::Int4(x $op y),
                (V::Int8(x), V::Int8(y)) => V::Int8(x $op y),
                (x, y) => return Err(SqlError::internal(format!("bit op on {} and {}", x.kind(), y.kind()))),
            })
        }
    };
}
bit_op!(op_bitand, &);
bit_op!(op_bitor, |);
bit_op!(op_bitxor, ^);

fn op_shift(a: &[Value], left: bool) -> SqlResult<Value> {
    use Value as V;
    let n = u32::try_from(a[1].as_i64().unwrap_or(0).rem_euclid(64)).unwrap_or(0);
    Ok(match &a[0] {
        V::Int2(x) => V::Int2(if left { x.wrapping_shl(n) } else { x.wrapping_shr(n) }),
        V::Int4(x) => V::Int4(if left { x.wrapping_shl(n) } else { x.wrapping_shr(n) }),
        V::Int8(x) => V::Int8(if left { x.wrapping_shl(n) } else { x.wrapping_shr(n) }),
        other => return Err(SqlError::internal(format!("shift of {}", other.kind()))),
    })
}
fn op_shl(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    op_shift(a, true)
}
fn op_shr(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    op_shift(a, false)
}
fn op_bitnot(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    use Value as V;
    Ok(match &a[0] {
        V::Int2(x) => V::Int2(!x),
        V::Int4(x) => V::Int4(!x),
        V::Int8(x) => V::Int8(!x),
        other => return Err(SqlError::internal(format!("bitwise not of {}", other.kind()))),
    })
}

fn op_concat(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    match (&a[0], &a[1]) {
        (Value::Text(x), Value::Text(y)) => Ok(Value::Text(format!("{x}{y}"))),
        (Value::Bytea(x), Value::Bytea(y)) => Ok(Value::Bytea([x.as_slice(), y.as_slice()].concat())),
        (x, y) => Err(SqlError::internal(format!("concat of {} and {}", x.kind(), y.kind()))),
    }
}

/// Resolves a unary (prefix) operator.
pub fn resolve_unary(op: &str, t: &Type) -> SqlResult<Resolved> {
    let t = if *t == Type::Unknown { Type::Numeric } else { t.clone() };
    let ok = |func: Func| Ok(Resolved { func, args: vec![t.clone()], ret: t.clone() });
    match op {
        "-" if t.is_numeric() || t == Type::Interval => ok(strict("-", op_neg)),
        "+" if t.is_numeric() || t == Type::Interval => ok(strict("+", op_identity)),
        "~" if t.is_integer() => ok(strict("~", op_bitnot)),
        "@" if t.is_numeric() => Ok(Resolved { func: strict("abs", math::abs_fn), args: vec![t.clone()], ret: t }),
        "|/" if t.is_numeric() => Ok(Resolved { func: strict("sqrt", math::sqrt_fn), args: vec![Type::Float8], ret: Type::Float8 }),
        "||/" if t.is_numeric() => Ok(Resolved { func: strict("cbrt", math::cbrt_fn), args: vec![Type::Float8], ret: Type::Float8 }),
        _ => Err(no_operator(op, None, &t)),
    }
}

/// Resolves a binary operator for the given operand types. Unknown-typed operands (string
/// literals, untyped parameters) are assigned the type the chosen operator wants.
pub fn resolve_binary(op: &str, l: &Type, r: &Type) -> SqlResult<Resolved> {
    use Type::*;
    let res = |func: Func, lt: Type, rt: Type, ret: Type| Ok(Resolved { func, args: vec![lt, rt], ret });
    if let Some(func) = comparison(op) {
        if let Some(t) = common_type(l, r)
            && comparable(&t)
        {
            return res(func, t.clone(), t, Bool);
        }
        return Err(no_operator(op, Some(l), r));
    }
    // Date/time arithmetic first: those operators mix types on purpose.
    if let Some(r) = datetime::resolve_operator(op, l, r)? {
        return Ok(r);
    }
    if let Some(r) = json::resolve_operator(op, l, r)? {
        return Ok(r);
    }
    if let Some(r) = array::resolve_operator(op, l, r)? {
        return Ok(r);
    }
    if let Some(r) = string::resolve_operator(op, l, r)? {
        return Ok(r);
    }
    match op {
        "+" | "-" | "*" | "/" | "%" => {
            let t = match (l, r) {
                (Unknown, Unknown) => {
                    return Err(SqlError::new(code::AMBIGUOUS_FUNCTION, format!("operator is not unique: unknown {op} unknown")));
                }
                (Unknown, t) | (t, Unknown) if t.is_numeric() => t.clone(),
                _ => numeric_common(l, r).ok_or_else(|| no_operator(op, Some(l), r))?,
            };
            if op == "%" && t.is_float() {
                return Err(no_operator(op, Some(&t), &t));
            }
            let func = match op {
                "+" => strict("+", op_add),
                "-" => strict("-", op_sub),
                "*" => strict("*", op_mul),
                "/" => strict("/", op_div),
                _ => strict("%", op_mod),
            };
            res(func, t.clone(), t.clone(), t)
        }
        "^" => {
            let t = match numeric_common(&if *l == Unknown { Float8 } else { l.clone() }, &if *r == Unknown { Float8 } else { r.clone() }) {
                Some(Numeric) => Numeric,
                Some(_) => Float8,
                None => return Err(no_operator(op, Some(l), r)),
            };
            res(strict("^", op_pow), t.clone(), t.clone(), t)
        }
        "&" | "|" | "#" | "<<" | ">>" => {
            let t = match (l, r) {
                (Unknown, t) | (t, Unknown) if t.is_integer() => t.clone(),
                _ if l.is_integer() && r.is_integer() => {
                    if op == "<<" || op == ">>" {
                        l.clone()
                    } else {
                        numeric_common(l, r).unwrap_or(Int8)
                    }
                }
                _ => return Err(no_operator(op, Some(l), r)),
            };
            let func = match op {
                "&" => strict("&", op_bitand),
                "|" => strict("|", op_bitor),
                "#" => strict("#", op_bitxor),
                "<<" => return res(strict("<<", op_shl), t.clone(), Int4, t),
                _ => return res(strict(">>", op_shr), t.clone(), Int4, t),
            };
            res(func, t.clone(), t.clone(), t)
        }
        "||" => match (l, r) {
            (Bytea, Bytea) => res(strict("||", op_concat), Bytea, Bytea, Bytea),
            (Array(_), _) | (_, Array(_)) => Err(no_operator(op, Some(l), r)),
            // anynonarray || text and text || anynonarray: the other side goes through text.
            _ if *l == Text || *r == Text || (*l == Unknown && *r == Unknown) => res(strict("||", op_concat), Text, Text, Text),
            (Unknown, _) | (_, Unknown) => res(strict("||", op_concat), Text, Text, Text),
            _ => Err(no_operator(op, Some(l), r)),
        },
        _ => Err(no_operator(op, Some(l), r)),
    }
}

/// True if a value of type `from` may be passed where `to` is expected without a cast.
pub fn implicit_ok(from: &Type, to: &Type) -> bool {
    can_coerce(from, to, CoercionContext::Implicit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_promotion_matches_postgres() {
        assert_eq!(numeric_common(&Type::Int4, &Type::Float4), Some(Type::Float8));
        assert_eq!(numeric_common(&Type::Int8, &Type::Float4), Some(Type::Float8));
        assert_eq!(numeric_common(&Type::Int2, &Type::Int2), Some(Type::Int2));
        assert_eq!(numeric_common(&Type::Int4, &Type::Numeric), Some(Type::Numeric));
        assert_eq!(numeric_common(&Type::Int8, &Type::Int4), Some(Type::Int8));
    }

    #[test]
    fn operator_resolution() {
        let r = resolve_binary("+", &Type::Unknown, &Type::Int4).expect("resolve");
        assert_eq!(r.ret, Type::Int4);
        let r = resolve_binary("||", &Type::Text, &Type::Int4).expect("resolve");
        assert_eq!(r.ret, Type::Text);
        assert_eq!(resolve_binary("=", &Type::Text, &Type::Int4).expect_err("mismatch").message, "operator does not exist: text = integer");
    }
}
