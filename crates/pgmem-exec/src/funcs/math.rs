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
                [t, s] if (t.is_numeric() || *t == Type::Unknown) && (s.is_integer() || *s == Type::Unknown) => {
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
                let t = super::numeric_common(a, b).filter(|t| t.is_integer()).ok_or_else(err)?;
                let f = if name == "gcd" { strict("gcd", gcd_fn) } else { strict("lcm", lcm_fn) };
                resolved(f, vec![t.clone(), t.clone()], t)
            }
            _ => Err(err()),
        },
        "pi" if args.is_empty() => resolved(strict("pi", |_, _| Ok(Value::Float8(std::f64::consts::PI))), vec![], Type::Float8),
        "random" if args.is_empty() => resolved(lenient("random", random_fn), vec![], Type::Float8),
        "degrees" => resolved(strict("degrees", |a, _| Ok(Value::Float8(f(&a[0]).to_degrees()))), vec![Type::Float8], Type::Float8),
        "radians" => resolved(strict("radians", |a, _| Ok(Value::Float8(f(&a[0]).to_radians()))), vec![Type::Float8], Type::Float8),
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "cot" => {
            let func = match name {
                "sin" => strict("sin", |a, _| Ok(Value::Float8(f(&a[0]).sin()))),
                "cos" => strict("cos", |a, _| Ok(Value::Float8(f(&a[0]).cos()))),
                "tan" => strict("tan", |a, _| Ok(Value::Float8(f(&a[0]).tan()))),
                "asin" => strict("asin", |a, _| Ok(Value::Float8(f(&a[0]).asin()))),
                "acos" => strict("acos", |a, _| Ok(Value::Float8(f(&a[0]).acos()))),
                "atan" => strict("atan", |a, _| Ok(Value::Float8(f(&a[0]).atan()))),
                _ => strict("cot", |a, _| Ok(Value::Float8(1.0 / f(&a[0]).tan()))),
            };
            resolved(func, vec![Type::Float8], Type::Float8)
        }
        "atan2" => {
            resolved(strict("atan2", |a, _| Ok(Value::Float8(f(&a[0]).atan2(f(&a[1]))))), vec![Type::Float8, Type::Float8], Type::Float8)
        }
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
    Ok(Value::Float8(f(&a[0]).cbrt()))
}

fn exp_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(match &a[0] {
        Value::Numeric(n) => Value::Numeric(Numeric::from_f64(n.to_f64().exp())?),
        v => Value::Float8(f(v).exp()),
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
    let x = f(&a[0]);
    ln_check(x)?;
    Ok(match &a[0] {
        Value::Numeric(_) => Value::Numeric(Numeric::from_f64(x.ln())?),
        _ => Value::Float8(x.ln()),
    })
}

fn log10_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let x = f(&a[0]);
    ln_check(x)?;
    Ok(match &a[0] {
        Value::Numeric(_) => Value::Numeric(Numeric::from_f64(x.log10())?),
        _ => Value::Float8(x.log10()),
    })
}

fn log_base_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (b, x) = (f(&a[0]), f(&a[1]));
    ln_check(b)?;
    ln_check(x)?;
    Ok(Value::Numeric(Numeric::from_f64(x.ln() / b.ln())?))
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
        (Value::Numeric(_), _) | (_, Value::Numeric(_)) => {
            let (b, e) = (num(x), num(y));
            if b.is_zero() && e.signum() < 0 {
                return Err(SqlError::new("2201F", "zero raised to a negative power is undefined"));
            }
            // Integral exponents are computed exactly; others go through f64.
            if (e.scale() == 0 || e.trunc(0).cmp_num(&e).is_eq())
                && let Some(n) = e.to_i64()
                && (0..=1000).contains(&n)
            {
                let mut acc = Numeric::from_i64(1);
                for _ in 0..n {
                    acc = acc.mul(&b);
                }
                return Ok(Value::Numeric(acc));
            }
            Ok(Value::Numeric(Numeric::from_f64(b.to_f64().powf(e.to_f64()))?))
        }
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

fn gcd_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let g = gcd_i(a[0].as_i64().unwrap_or(0), a[1].as_i64().unwrap_or(0));
    crate::cast::cast_value(Value::Int8(g), &a[0].natural_type(), -1)
}

fn lcm_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
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
