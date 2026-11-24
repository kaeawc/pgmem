//! Mathematical functions.

use super::{Resolved, lenient, no_function, resolved, strict};
use crate::eval::EvalCtx;
use pgmem_types::{Numeric, SqlError, SqlResult, Type, Value, code};

/// Overloads that exist for `double precision` and `numeric`: numeric arguments keep numeric,
/// everything else numeric-ish (integers, unknown) resolves to the preferred type, float8.
fn float_or_numeric(t: &Type) -> Option<Type> {
    match t {
        Type::Numeric => Some(Type::Numeric),
        Type::Unknown | Type::Int2 | Type::Int4 | Type::Int8 | Type::Float4 | Type::Float8 => Some(Type::Float8),
        _ => None,
    }
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let one = |f: crate::plan::Func| -> SqlResult<Option<Resolved>> {
        match args {
            [t] => {
                let t = float_or_numeric(t).ok_or_else(err)?;
                resolved(f, vec![t.clone()], t)
            }
            _ => Err(err()),
        }
    };
    match name {
        "abs" => match args {
            [t] if t.is_numeric() => resolved(strict("abs", abs_fn), vec![t.clone()], t.clone()),
            [Type::Unknown] => resolved(strict("abs", abs_fn), vec![Type::Float8], Type::Float8),
            _ => Err(err()),
        },
        "ceil" | "ceiling" => one(strict("ceil", ceil_fn)),
        "floor" => one(strict("floor", floor_fn)),
        "sign" => one(strict("sign", sign_fn)),
        "sqrt" => one(strict("sqrt", sqrt_fn)),
        "cbrt" => resolved(strict("cbrt", cbrt_fn), vec![Type::Float8], Type::Float8),
        "exp" => one(strict("exp", exp_fn)),
        "ln" => one(strict("ln", ln_fn)),
        "log10" => one(strict("log10", log10_fn)),
        "log" => match args {
            [_] => one(strict("log", log10_fn)),
            [_, _] => resolved(strict("log", log_base_fn), vec![Type::Numeric, Type::Numeric], Type::Numeric),
            _ => Err(err()),
        },
        "round" | "trunc" => {
            let f = if name == "round" { strict("round", round_fn) } else { strict("trunc", trunc_fn) };
            match args {
                [t] => {
                    let t = float_or_numeric(t).ok_or_else(err)?;
                    resolved(f, vec![t.clone()], t)
                }
                [t, s] if (t.is_integer() || *t == Type::Numeric || *t == Type::Unknown) && (s.is_integer() || *s == Type::Unknown) => {
                    resolved(f, vec![Type::Numeric, Type::Int4], Type::Numeric)
                }
                _ => Err(err()),
            }
        }
        "power" | "pow" => match args {
            [a, b] => {
                let t = if *a == Type::Numeric || *b == Type::Numeric { Type::Numeric } else { Type::Float8 };
                if float_or_numeric(a).is_none() || float_or_numeric(b).is_none() {
                    return Err(err());
                }
                resolved(strict("power", power_fn), vec![t.clone(), t.clone()], t)
            }
            _ => Err(err()),
        },
        "mod" => match args {
            [a, b] => {
                let t = match (a, b) {
                    (Type::Unknown, t) | (t, Type::Unknown) if t.is_integer() || *t == Type::Numeric => t.clone(),
                    _ => super::numeric_common(a, b).filter(|t| !t.is_float()).ok_or_else(err)?,
                };
                resolved(strict("mod", mod_fn), vec![t.clone(), t.clone()], t)
            }
            _ => Err(err()),
        },
        "div" => match args {
            [_, _] => resolved(strict("div", div_fn), vec![Type::Numeric, Type::Numeric], Type::Numeric),
            _ => Err(err()),
        },
        "gcd" | "lcm" => match args {
            [a, b] => {
                let t = match (a, b) {
                    (Type::Unknown, t) | (t, Type::Unknown) if t.is_integer() || *t == Type::Numeric => t.clone(),
                    _ => super::numeric_common(a, b).filter(|t| t.is_integer() || *t == Type::Numeric).ok_or_else(err)?,
                };
                let f = if name == "gcd" { strict("gcd", gcd_fn) } else { strict("lcm", lcm_fn) };
                resolved(f, vec![t.clone(), t.clone()], t)
            }
            _ => Err(err()),
        },
        "scale" | "min_scale" | "trim_scale" => match args {
            [t] if t.is_numeric() || *t == Type::Unknown => {
                let (f, ret) = match name {
                    "scale" => (strict("scale", |a, _| Ok(numeric_scale(&a[0], |n| n.scale()))), Type::Int4),
                    "min_scale" => (strict("min_scale", |a, _| Ok(numeric_scale(&a[0], Numeric::min_scale))), Type::Int4),
                    _ => (strict("trim_scale", |a, _| Ok(Value::Numeric(num(&a[0]).trim_scale()))), Type::Numeric),
                };
                resolved(f, vec![Type::Numeric], ret)
            }
            _ => Err(err()),
        },
        "factorial" => match args {
            [t] if t.is_integer() || *t == Type::Unknown => resolved(
                strict("factorial", |a, _| Ok(Value::Numeric(Numeric::factorial(a[0].as_i64().unwrap_or(0))?))),
                vec![Type::Int8],
                Type::Numeric,
            ),
            _ => Err(err()),
        },
        "pi" if args.is_empty() => resolved(strict("pi", |_, _| Ok(Value::Float8(std::f64::consts::PI))), vec![], Type::Float8),
        "random" if args.is_empty() => resolved(lenient("random", random_fn), vec![], Type::Float8),
        "degrees" => resolved(strict("degrees", |a, _| Ok(Value::Float8(f(&a[0]).to_degrees()))), vec![Type::Float8], Type::Float8),
        "radians" => resolved(strict("radians", |a, _| Ok(Value::Float8(f(&a[0]).to_radians()))), vec![Type::Float8], Type::Float8),
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "cot" | "sind" | "cosd" | "tand" | "asind" | "acosd" | "atand" | "cotd"
        | "sinh" | "cosh" | "tanh" | "asinh" | "acosh" | "atanh" => {
            let func = match name {
                "sin" => strict("sin", |a, _| trig("sin", &a[0])),
                "cos" => strict("cos", |a, _| trig("cos", &a[0])),
                "tan" => strict("tan", |a, _| trig("tan", &a[0])),
                "asin" => strict("asin", |a, _| trig("asin", &a[0])),
                "acos" => strict("acos", |a, _| trig("acos", &a[0])),
                "atan" => strict("atan", |a, _| trig("atan", &a[0])),
                "cot" => strict("cot", |a, _| trig("cot", &a[0])),
                "sind" => strict("sind", |a, _| trig("sind", &a[0])),
                "cosd" => strict("cosd", |a, _| trig("cosd", &a[0])),
                "tand" => strict("tand", |a, _| trig("tand", &a[0])),
                "asind" => strict("asind", |a, _| trig("asind", &a[0])),
                "acosd" => strict("acosd", |a, _| trig("acosd", &a[0])),
                "atand" => strict("atand", |a, _| trig("atand", &a[0])),
                "cotd" => strict("cotd", |a, _| trig("cotd", &a[0])),
                "sinh" => strict("sinh", |a, _| trig("sinh", &a[0])),
                "cosh" => strict("cosh", |a, _| trig("cosh", &a[0])),
                "tanh" => strict("tanh", |a, _| trig("tanh", &a[0])),
                "asinh" => strict("asinh", |a, _| trig("asinh", &a[0])),
                "acosh" => strict("acosh", |a, _| trig("acosh", &a[0])),
                _ => strict("atanh", |a, _| trig("atanh", &a[0])),
            };
            resolved(func, vec![Type::Float8], Type::Float8)
        }
        "atan2d" => resolved(
            strict("atan2d", |a, _| {
                let (y, x) = (f(&a[0]), f(&a[1]));
                Ok(Value::Float8(y.atan2(x) / 1.0f64.atan() * 45.0))
            }),
            vec![Type::Float8, Type::Float8],
            Type::Float8,
        ),
        "atan2" => {
            resolved(strict("atan2", |a, _| Ok(Value::Float8(f(&a[0]).atan2(f(&a[1]))))), vec![Type::Float8, Type::Float8], Type::Float8)
        }
        "width_bucket" if args.len() == 2 => match args {
            [t, Type::Array(e)] => {
                let common = super::numeric_common(t, e).or_else(|| (**e == *t).then(|| t.clone())).unwrap_or_else(|| (**e).clone());
                resolved(strict("width_bucket", width_bucket_array), vec![common.clone(), Type::array_of(common)], Type::Int4)
            }
            _ => Err(err()),
        },
        "width_bucket" if args.len() == 4 => {
            resolved(strict("width_bucket", width_bucket_fn), vec![Type::Float8, Type::Float8, Type::Float8, Type::Int4], Type::Int4)
        }
        _ => Ok(None),
    }
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(f64::NAN)
}

fn num(v: &Value) -> Numeric {
    match v {
        Value::Numeric(n) => n.clone(),
        other => other.as_i64().map_or_else(|| Numeric::from_f64(f(other)).unwrap_or(Numeric::NaN), Numeric::from_i64),
    }
}

pub fn abs_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    use Value as V;
    Ok(match &a[0] {
        V::Int2(x) => V::Int2(x.checked_abs().ok_or_else(|| SqlError::out_of_range("smallint"))?),
        V::Int4(x) => V::Int4(x.checked_abs().ok_or_else(|| SqlError::out_of_range("integer"))?),
        V::Int8(x) => V::Int8(x.checked_abs().ok_or_else(|| SqlError::out_of_range("bigint"))?),
        V::Float4(x) => V::Float4(x.abs()),
        V::Float8(x) => V::Float8(x.abs()),
        V::Numeric(n) => V::Numeric(n.abs()),
        other => return Err(SqlError::internal(format!("abs of {}", other.kind()))),
    })
}

fn ceil_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match &a[0] {
        Value::Numeric(n) => Value::Numeric(n.ceil()),
        v => Value::Float8(f(v).ceil()),
    })
}

fn floor_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match &a[0] {
        Value::Numeric(n) => Value::Numeric(n.floor()),
        v => Value::Float8(f(v).floor()),
    })
}

fn sign_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match &a[0] {
        Value::Numeric(n) => Value::Numeric(Numeric::from_i64(i64::from(n.signum()))),
        v => {
            let x = f(v);
            Value::Float8(if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                0.0
            })
        }
    })
}

pub fn sqrt_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match &a[0] {
        Value::Numeric(n) => Value::Numeric(n.sqrt()?),
        v => {
            let x = f(v);
            if x < 0.0 {
                return Err(SqlError::new("2201F", "cannot take square root of a negative number"));
            }
            Value::Float8(x.sqrt())
        }
    })
}

pub fn cbrt_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Float8(glibc_cbrt(f(&a[0]))))
}

/// glibc's `cbrt` (sysdeps/ieee754/dbl-64/s_cbrt.c), which Postgres calls: not correctly
/// rounded (`cbrt(27)` is 3.0000000000000004), so Rust's `f64::cbrt` would differ.
fn glibc_cbrt(x: f64) -> f64 {
    const CBRT2: f64 = 1.259_921_049_894_873_2;
    const SQR_CBRT2: f64 = 1.587_401_051_968_199_6;
    const FACTOR: [f64; 5] = [1.0 / SQR_CBRT2, 1.0 / CBRT2, 1.0, CBRT2, SQR_CBRT2];
    if x == 0.0 || !x.is_finite() {
        return x + x;
    }
    let (xm, xe) = frexp(x.abs());
    let u = 0.354_895_765_043_919_84
        + (1.508_191_937_815_849
            + (-2.114_994_941_673_713
                + (2.446_931_225_635_344_4
                    + (-1.834_692_774_836_130_8 + (0.784_932_344_976_639_2 - 0.145_263_899_385_486_37 * xm) * xm) * xm)
                    * xm)
                * xm)
            * xm;
    let t2 = u * u * u;
    let idx = usize::try_from(2 + xe % 3).unwrap_or(2);
    let ym = u * (t2 + 2.0 * xm) / (2.0 * t2 + xm) * FACTOR[idx];
    ldexp(if x > 0.0 { ym } else { -ym }, xe / 3)
}

/// `frexp`: `x = m * 2^e` with `0.5 <= m < 1` (x finite and nonzero).
fn frexp(x: f64) -> (f64, i32) {
    let bits = x.to_bits();
    let exp = i32::try_from((bits >> 52) & 0x7ff).unwrap_or(0);
    if exp == 0 {
        // Subnormal: scale up first.
        let (m, e) = frexp(x * f64::from_bits(0x4350_0000_0000_0000)); // 2^54
        return (m, e - 54);
    }
    let m = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
    (m, exp - 1022)
}

fn ldexp(x: f64, e: i32) -> f64 {
    x * 2f64.powi(e)
}

fn exp_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match &a[0] {
        Value::Numeric(n) => Value::Numeric(n.exp()?),
        v => {
            let r = f(v).exp();
            if r.is_infinite() && f(v).is_finite() {
                return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"));
            }
            if r == 0.0 && f(v).is_finite() {
                return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: underflow"));
            }
            Value::Float8(r)
        }
    })
}

fn ln_check(x: f64) -> SqlResult<()> {
    if x == 0.0 {
        return Err(SqlError::new("2201E", "cannot take logarithm of zero"));
    }
    if x < 0.0 {
        return Err(SqlError::new("2201E", "cannot take logarithm of a negative number"));
    }
    Ok(())
}

fn ln_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if let Value::Numeric(n) = &a[0] {
        return Ok(Value::Numeric(n.ln()?));
    }
    let x = f(&a[0]);
    ln_check(x)?;
    Ok(Value::Float8(x.ln()))
}

fn log10_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if let Value::Numeric(n) = &a[0] {
        return Ok(Value::Numeric(Numeric::log(&Numeric::from_i64(10), n)?));
    }
    let x = f(&a[0]);
    ln_check(x)?;
    Ok(Value::Float8(x.log10()))
}

fn log_base_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Numeric(Numeric::log(&num(&a[0]), &num(&a[1]))?))
}

fn round_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match (&a[0], a.get(1)) {
        (v, Some(s)) => Value::Numeric(num(v).round(i32::try_from(s.as_i64().unwrap_or(0)).unwrap_or(0))),
        (Value::Numeric(n), None) => Value::Numeric(n.round(0)),
        (v, None) => Value::Float8(f(v).round_ties_even()),
    })
}

fn trunc_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match (&a[0], a.get(1)) {
        (v, Some(s)) => Value::Numeric(num(v).trunc(i32::try_from(s.as_i64().unwrap_or(0)).unwrap_or(0))),
        (Value::Numeric(n), None) => Value::Numeric(n.trunc(0)),
        (v, None) => Value::Float8(f(v).trunc()),
    })
}

pub fn power_values(x: &Value, y: &Value) -> SqlResult<Value> {
    match (x, y) {
        (Value::Numeric(_), _) | (_, Value::Numeric(_)) => Ok(Value::Numeric(num(x).power(&num(y))?)),
        _ => {
            let (b, e) = (f(x), f(y));
            if b == 0.0 && e < 0.0 {
                return Err(SqlError::new("2201F", "zero raised to a negative power is undefined"));
            }
            if b < 0.0 && e.fract() != 0.0 {
                return Err(SqlError::new("2201F", "a negative number raised to a non-integer power yields a complex result"));
            }
            let r = b.powf(e);
            if r.is_infinite() && b.is_finite() && e.is_finite() {
                return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"));
            }
            Ok(Value::Float8(r))
        }
    }
}

fn power_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    power_values(&a[0], &a[1])
}

fn mod_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    super::resolve_binary("%", &a[0].natural_type(), &a[1].natural_type()).and_then(|r| (r.func.f)(a, ctx))
}

fn div_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Numeric(num(&a[0]).div_trunc(&num(&a[1]))?))
}

fn gcd_i(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a.abs()
}

/// `scale` / `min_scale`: NULL for NaN and infinities.
fn numeric_scale(v: &Value, f: impl Fn(&Numeric) -> u32) -> Value {
    let n = num(v);
    if n.is_finite() { Value::Int4(i32::try_from(f(&n)).unwrap_or(i32::MAX)) } else { Value::Null }
}

fn gcd_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if let (Value::Numeric(x), Value::Numeric(y)) = (&a[0], &a[1]) {
        return Ok(Value::Numeric(x.gcd(y)));
    }
    let g = gcd_i(a[0].as_i64().unwrap_or(0), a[1].as_i64().unwrap_or(0));
    crate::cast::cast_value(Value::Int8(g), &a[0].natural_type(), -1)
}

fn lcm_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if let (Value::Numeric(x), Value::Numeric(y)) = (&a[0], &a[1]) {
        return Ok(Value::Numeric(x.lcm(y)));
    }
    let (x, y) = (a[0].as_i64().unwrap_or(0), a[1].as_i64().unwrap_or(0));
    let l = if x == 0 || y == 0 { 0 } else { (x / gcd_i(x, y)).checked_mul(y).ok_or_else(|| SqlError::out_of_range("bigint"))?.abs() };
    crate::cast::cast_value(Value::Int8(l), &a[0].natural_type(), -1)
}

fn random_fn(_: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| SqlError::internal(format!("random: {e}")))?;
    #[allow(clippy::cast_precision_loss)]
    let x = (u64::from_le_bytes(buf) >> 11) as f64 / (1u64 << 53) as f64;
    Ok(Value::Float8(x))
}

/// `width_bucket(operand, thresholds)`: the number of thresholds <= operand (binary search,
/// the array must be sorted).
fn width_bucket_array(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let Value::Array(th) = &a[1] else { return Ok(Value::Null) };
    if th.iter().any(|v| matches!(v, Value::Array(_))) {
        return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "thresholds must be one-dimensional array"));
    }
    if th.iter().any(Value::is_null) {
        return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "thresholds array must not contain NULLs"));
    }
    let (mut lo, mut hi) = (0usize, th.len());
    while lo < hi {
        let mid = usize::midpoint(lo, hi);
        if pgmem_types::compare_values(&a[0], &th[mid])?.is_lt() {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(Value::Int4(i32::try_from(lo).unwrap_or(i32::MAX)))
}

fn out_of_range_input() -> SqlError {
    SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "input is out of range")
}

const RADIANS_PER_DEGREE: f64 = 0.017_453_292_519_943_295;

/// `float.c`'s degree helpers: exact at the multiples of 30 and 45 degrees.
fn sind_0_to_30(x: f64) -> f64 {
    let sin_30 = (30.0 * RADIANS_PER_DEGREE).sin();
    ((x * RADIANS_PER_DEGREE).sin() / sin_30) / 2.0
}

fn cosd_0_to_60(x: f64) -> f64 {
    let one_minus_cos_60 = 1.0 - (60.0 * RADIANS_PER_DEGREE).cos();
    1.0 - ((1.0 - (x * RADIANS_PER_DEGREE).cos()) / one_minus_cos_60) / 2.0
}

fn sind_q1(x: f64) -> f64 {
    if x <= 30.0 { sind_0_to_30(x) } else { cosd_0_to_60(90.0 - x) }
}

fn cosd_q1(x: f64) -> f64 {
    if x <= 60.0 { cosd_0_to_60(x) } else { sind_0_to_30(90.0 - x) }
}

fn asind_q1(x: f64) -> f64 {
    if x <= 0.5 { (x.asin() / 0.5f64.asin()) * 30.0 } else { 90.0 - (x.acos() / 0.5f64.acos()) * 60.0 }
}

fn acosd_q1(x: f64) -> f64 {
    if x <= 0.5 { 90.0 - (x.asin() / 0.5f64.asin()) * 30.0 } else { (x.acos() / 0.5f64.acos()) * 60.0 }
}

/// Reduces degrees to [0, 90] for sind/cosd/tand/cotd: (reduced, sign).
fn reduce_degrees(x: f64, odd_at_zero: bool, odd_past_180: bool, odd_past_90: bool) -> (f64, f64) {
    let mut sign = 1.0;
    let mut a = x % 360.0;
    if a < 0.0 {
        a = -a;
        if odd_at_zero {
            sign = -sign;
        }
    }
    if a > 180.0 {
        a = 360.0 - a;
        if odd_past_180 {
            sign = -sign;
        }
    }
    if a > 90.0 {
        a = 180.0 - a;
        if odd_past_90 {
            sign = -sign;
        }
    }
    (a, sign)
}

/// The trigonometric and hyperbolic functions, with `float.c`'s domain checks.
fn trig(name: &str, v: &Value) -> SqlResult<Value> {
    let x = f(v);
    if x.is_nan() {
        return Ok(Value::Float8(f64::NAN));
    }
    let unit = |ok: bool| if ok { Ok(()) } else { Err(out_of_range_input()) };
    let r = match name {
        "sin" | "cos" | "tan" | "cot" | "sind" | "cosd" | "tand" | "cotd" => {
            unit(x.is_finite())?;
            match name {
                "sin" => x.sin(),
                "cos" => x.cos(),
                "tan" => x.tan(),
                "cot" => 1.0 / x.tan(),
                "sind" => {
                    let (a, s) = reduce_degrees(x, true, true, false);
                    s * sind_q1(a)
                }
                "cosd" => {
                    let (a, s) = reduce_degrees(x, false, false, true);
                    s * cosd_q1(a)
                }
                "tand" => {
                    let (a, s) = reduce_degrees(x, true, true, true);
                    let tan_45 = sind_q1(45.0) / cosd_q1(45.0);
                    let r = s * ((sind_q1(a) / cosd_q1(a)) / tan_45);
                    if r == 0.0 { 0.0 } else { r }
                }
                _ => {
                    let (a, s) = reduce_degrees(x, true, true, true);
                    let cot_45 = cosd_q1(45.0) / sind_q1(45.0);
                    let r = s * ((cosd_q1(a) / sind_q1(a)) / cot_45);
                    if r == 0.0 { 0.0 } else { r }
                }
            }
        }
        "asin" | "acos" | "asind" | "acosd" => {
            unit((-1.0..=1.0).contains(&x))?;
            match name {
                "asin" => x.asin(),
                "acos" => x.acos(),
                "asind" => {
                    if x >= 0.0 {
                        asind_q1(x)
                    } else {
                        -asind_q1(-x)
                    }
                }
                _ => {
                    if x >= 0.0 {
                        acosd_q1(x)
                    } else {
                        90.0 + asind_q1(-x)
                    }
                }
            }
        }
        "atan" => x.atan(),
        "atand" => x.atan() / 1.0f64.atan() * 45.0,
        "sinh" => x.sinh(),
        "cosh" => x.cosh(),
        "tanh" => x.tanh(),
        "asinh" => x.asinh(),
        "acosh" => {
            unit(x >= 1.0)?;
            x.acosh()
        }
        _ => {
            unit((-1.0..=1.0).contains(&x))?;
            x.atanh()
        }
    };
    Ok(Value::Float8(r))
}

fn width_bucket_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (x, lo, hi) = (f(&a[0]), f(&a[1]), f(&a[2]));
    let n = a[3].as_i64().unwrap_or(0);
    if n <= 0 {
        return Err(SqlError::new("2201G", "count must be greater than zero"));
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let bucket = if x < lo {
        0
    } else if x >= hi {
        n + 1
    } else {
        ((x - lo) / (hi - lo) * n as f64).floor() as i64 + 1
    };
    Ok(Value::Int4(i32::try_from(bucket).unwrap_or(i32::MAX)))
}
