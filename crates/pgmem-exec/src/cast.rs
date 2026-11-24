//! Casts and coercion rules (`pg_cast`).

use pgmem_types::datetime::{DATE_INFINITY, DATE_NEG_INFINITY, TS_INFINITY, TS_NEG_INFINITY, USECS_PER_DAY};
use pgmem_types::{Numeric, SqlError, SqlResult, Type, Value, code, text};

pub fn date_to_micros(d: i32) -> i64 {
    match d {
        DATE_INFINITY => TS_INFINITY,
        DATE_NEG_INFINITY => TS_NEG_INFINITY,
        _ => i64::from(d) * USECS_PER_DAY,
    }
}

pub fn micros_to_date(t: i64) -> i32 {
    match t {
        TS_INFINITY => DATE_INFINITY,
        TS_NEG_INFINITY => DATE_NEG_INFINITY,
        _ => i32::try_from(t.div_euclid(USECS_PER_DAY)).unwrap_or(DATE_INFINITY),
    }
}

/// When a conversion may be applied without an explicit cast (`pg_cast.castcontext`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CoercionContext {
    Implicit,
    Assignment,
    Explicit,
}

/// The weakest context in which `from` converts to `to`, or None if no cast exists.
pub fn cast_context(from: &Type, to: &Type) -> Option<CoercionContext> {
    use CoercionContext::{Assignment, Explicit, Implicit};
    use Type::*;
    if from == to || *from == Unknown {
        return Some(Implicit);
    }
    Some(match (from, to) {
        (Int2, Int4 | Int8 | Float4 | Float8 | Numeric) | (Int4, Int8 | Float4 | Float8 | Numeric) | (Int8, Float4 | Float8 | Numeric) => {
            Implicit
        }
        (Float4, Float8) | (Numeric, Float4 | Float8) => Implicit,
        (Int8 | Int4, Int2) | (Int8, Int4) => Assignment,
        (Float4 | Float8 | Numeric, Int2 | Int4 | Int8) | (Float8, Float4) | (Float4 | Float8, Numeric) => Assignment,
        (Bool, Int4) | (Int4, Bool) => Explicit,
        (Date, Timestamp | Timestamptz) | (Timestamp, Timestamptz) | (Time, TimeTz | Interval) => Implicit,
        (Timestamptz, Timestamp | Date | Time | TimeTz) | (Timestamp, Date | Time) | (TimeTz, Time) | (Interval, Time) => Assignment,
        (Jsonb, Bool | Int2 | Int4 | Int8 | Float4 | Float8 | Numeric) => Explicit,
        // The string types (pg_cast): text, varchar, bpchar and name convert among themselves
        // implicitly; "char" converts to text implicitly and to the others by assignment.
        (Text | Varchar | Bpchar | Name, Text | Varchar | Bpchar | Name) | (Char, Text) => Implicit,
        (Char, Varchar | Bpchar) | (Text | Varchar | Bpchar, Char) => Assignment,
        (Char, Int4) | (Int4, Char) => Explicit,
        (Json, Jsonb) | (Jsonb, Json) => Assignment,
        // OIDs: integers and oid convert to the reg* aliases implicitly, and back by
        // assignment; text and varchar name a relation implicitly (`'t'::regclass`).
        (Int2 | Int4 | Int8 | Oid | Reg(_), Oid | Reg(_)) => Implicit,
        (Oid | Reg(_), Int4 | Int8) => Assignment,
        // Every type converts to a string type via its output function in assignment context,
        // and from one via its input function only when asked explicitly.
        (_, Text | Varchar | Bpchar | Name) => Assignment,
        (Text | Varchar | Bpchar | Name, _) => Explicit,
        (Array(a), Array(b)) => cast_context(a, b)?,
        _ => return None,
    })
}

pub fn can_coerce(from: &Type, to: &Type, ctx: CoercionContext) -> bool {
    cast_context(from, to).is_some_and(|c| c <= ctx)
}

/// Converts a value to `to`, applying a type modifier (`varchar(n)` length, `numeric(p,s)`).
/// `explicit` controls varchar truncation: explicit casts truncate, assignments error.
pub fn cast_value(v: Value, to: &Type, typmod: i32) -> SqlResult<Value> {
    cast_value_ctx(v, to, typmod, true)
}

pub fn cast_value_ctx(v: Value, to: &Type, typmod: i32, explicit: bool) -> SqlResult<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let out = convert(v, to)?;
    apply_typmod(out, to, typmod, explicit)
}

fn int_out_of_range(to: &Type) -> SqlError {
    match to {
        Type::Int2 => SqlError::out_of_range("smallint"),
        Type::Int8 => SqlError::out_of_range("bigint"),
        _ => SqlError::out_of_range("integer"),
    }
}

fn make_int(n: i64, to: &Type) -> SqlResult<Value> {
    Ok(match to {
        Type::Int2 => Value::Int2(i16::try_from(n).map_err(|_| int_out_of_range(to))?),
        Type::Int4 => Value::Int4(i32::try_from(n).map_err(|_| int_out_of_range(to))?),
        _ => Value::Int8(n),
    })
}

/// float → integer rounds half to even (`rint`), as Postgres does.
fn float_to_int(f: f64, to: &Type) -> SqlResult<Value> {
    if f.is_nan() || f.is_infinite() {
        return Err(int_out_of_range(to));
    }
    let r = f.round_ties_even();
    if r < -9.223_372_036_854_776e18 || r >= 9.223_372_036_854_776e18 {
        return Err(int_out_of_range(to));
    }
    #[allow(clippy::cast_possible_truncation)]
    make_int(r as i64, to)
}

/// Strips a bpchar value's padding (Postgres' `rtrim1`, used by bpchar → text).
pub fn bpchar_trim(v: Value) -> Value {
    match v {
        Value::Text(s) => Value::Text(s.trim_end_matches(' ').to_owned()),
        other => other,
    }
}

pub fn bpchar_trim_fn(a: &[Value], _: &crate::eval::EvalCtx) -> SqlResult<Value> {
    Ok(bpchar_trim(a[0].clone()))
}

/// `int8` → `oid`: the value must fit in 32 bits (negative values wrap, as in Postgres).
fn int_to_oid(n: i64) -> SqlResult<u32> {
    if !(-2_147_483_648..=4_294_967_295).contains(&n) {
        return Err(SqlError::out_of_range("OID"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(n as u32)
}

fn convert(v: Value, to: &Type) -> SqlResult<Value> {
    use Value as V;
    let from_ty = v.natural_type();
    if from_ty == *to && !matches!(v, V::Array(_)) {
        return Ok(v);
    }
    Ok(match (v, to) {
        (V::Text(s), t) => text::decode(&s, t)?,
        (V::Oid(o) | V::Reg(_, o, _), Type::Oid) => V::Oid(o),
        (V::Oid(o) | V::Reg(_, o, _), Type::Int8) => V::Int8(i64::from(o)),
        #[allow(clippy::cast_possible_wrap)]
        (V::Oid(o) | V::Reg(_, o, _), Type::Int4) => V::Int4(o as i32),
        (v @ (V::Int2(_) | V::Int4(_) | V::Int8(_)), Type::Oid) => V::Oid(int_to_oid(v.as_i64().unwrap_or_default())?),
        (v, t @ (Type::Varchar | Type::Bpchar | Type::Name | Type::Char | Type::Json)) => {
            let s = match &v {
                V::Bool(b) => (if *b { "true" } else { "false" }).to_owned(),
                other => text::encode(other, &other.natural_type())?,
            };
            text::decode(&s, t)?
        }
        (v, Type::Text) => V::Text(match &v {
            V::Bool(b) => (if *b { "true" } else { "false" }).to_owned(),
            other => text::encode(other, &other.natural_type())?,
        }),
        (v @ (V::Int2(_) | V::Int4(_) | V::Int8(_)), t @ (Type::Int2 | Type::Int4 | Type::Int8)) => {
            make_int(v.as_i64().unwrap_or_default(), t)?
        }
        (v @ (V::Int2(_) | V::Int4(_) | V::Int8(_)), Type::Float8) =>
        {
            #[allow(clippy::cast_precision_loss)]
            V::Float8(v.as_i64().unwrap_or_default() as f64)
        }
        (v @ (V::Int2(_) | V::Int4(_) | V::Int8(_)), Type::Float4) =>
        {
            #[allow(clippy::cast_precision_loss)]
            V::Float4(v.as_i64().unwrap_or_default() as f32)
        }
        (v @ (V::Int2(_) | V::Int4(_) | V::Int8(_)), Type::Numeric) => V::Numeric(Numeric::from_i64(v.as_i64().unwrap_or_default())),
        (v @ (V::Int2(_) | V::Int4(_) | V::Int8(_)), Type::Bool) => V::Bool(v.as_i64().unwrap_or_default() != 0),
        (V::Float4(f), Type::Float8) => V::Float8(f64::from(f)),
        #[allow(clippy::cast_possible_truncation)]
        (V::Float8(f), Type::Float4) => {
            let r = f as f32;
            if r.is_infinite() && !f.is_infinite() {
                return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"));
            }
            V::Float4(r)
        }
        (v @ (V::Float4(_) | V::Float8(_)), t @ (Type::Int2 | Type::Int4 | Type::Int8)) => float_to_int(v.as_f64().unwrap_or_default(), t)?,
        (v @ (V::Float4(_) | V::Float8(_)), Type::Numeric) => V::Numeric(Numeric::from_f64(v.as_f64().unwrap_or_default())?),
        (V::Numeric(n), t @ (Type::Int2 | Type::Int4 | Type::Int8)) => {
            if n.is_nan() {
                return Err(SqlError::feature_not_supported(format!("cannot convert NaN to {}", t.name())));
            }
            make_int(n.to_i64().ok_or_else(|| int_out_of_range(t))?, t)?
        }
        (V::Numeric(n), Type::Float8) => V::Float8(n.to_f64()),
        #[allow(clippy::cast_possible_truncation)]
        (V::Numeric(n), Type::Float4) => V::Float4(n.to_f64() as f32),
        (V::Bool(b), Type::Int4) => V::Int4(i32::from(b)),
        (V::Date(d), Type::Timestamp) => V::Timestamp(date_to_micros(d)),
        (V::Date(d), Type::Timestamptz) => V::Timestamptz(date_to_micros(d)),
        (V::Timestamp(t), Type::Timestamptz) => V::Timestamptz(t),
        (V::Timestamptz(t), Type::Timestamp) => V::Timestamp(t),
        (V::Timestamp(t) | V::Timestamptz(t), Type::Date) => V::Date(micros_to_date(t)),
        (V::Timestamp(t) | V::Timestamptz(t), Type::Time) => V::Time(t.rem_euclid(USECS_PER_DAY)),
        (V::Timestamptz(t), Type::TimeTz) => V::TimeTz(t.rem_euclid(USECS_PER_DAY), 0),
        (V::Time(t), Type::TimeTz) => V::TimeTz(t, 0),
        (V::TimeTz(t, _), Type::Time) => V::Time(t),
        (V::Time(t), Type::Interval) => V::Interval(pgmem_types::Interval::from_micros(t)),
        (V::Interval(iv), Type::Time) => V::Time(iv.micros.rem_euclid(USECS_PER_DAY)),
        (V::Jsonb(j), t) => jsonb_to_scalar(&j, t)?,
        (V::Array(items), Type::Array(elem)) => {
            V::Array(items.into_iter().map(|i| cast_value(i, elem, -1)).collect::<SqlResult<Vec<_>>>()?)
        }
        (v, t) => return Err(SqlError::cannot_coerce(&v.natural_type().name(), &t.name())),
    })
}

/// `jsonb::int`, `jsonb::bool`, ...: only from a JSON scalar of the matching kind.
fn jsonb_to_scalar(j: &pgmem_types::Jsonb, to: &Type) -> SqlResult<Value> {
    use serde_json::Value as J;
    let wrong = |want: &str| SqlError::new(code::INVALID_PARAMETER_VALUE, format!("cannot cast jsonb {} to type {want}", j.type_name()));
    match (j.as_value(), to) {
        (J::Bool(b), Type::Bool) => Ok(Value::Bool(*b)),
        (J::Number(n), t @ (Type::Int2 | Type::Int4 | Type::Int8 | Type::Float4 | Type::Float8 | Type::Numeric)) => {
            cast_value(Value::Numeric(Numeric::parse(&n.to_string())?), t, -1)
        }
        (_, t) => Err(wrong(&t.name())),
    }
}

/// Postgres typmod encodings: `varchar(n)` is `n + 4`; `numeric(p, s)` is `((p << 16) | s) + 4`.
pub fn varchar_typmod(n: i64) -> i32 {
    i32::try_from(n + 4).unwrap_or(-1)
}

pub fn numeric_typmod(p: i64, s: i64) -> i32 {
    i32::try_from(((p << 16) | s) + 4).unwrap_or(-1)
}

fn apply_typmod(v: Value, to: &Type, typmod: i32, explicit: bool) -> SqlResult<Value> {
    if typmod < 0 {
        return Ok(v);
    }
    match (v, to) {
        (Value::Text(s), Type::Bpchar) => {
            let len = usize::try_from(typmod - 4).unwrap_or(usize::MAX);
            let n = s.chars().count();
            if n <= len {
                return Ok(Value::Text(format!("{s}{}", " ".repeat(len - n))));
            }
            if explicit || s.chars().skip(len).all(|c| c == ' ') {
                Ok(Value::Text(s.chars().take(len).collect()))
            } else {
                Err(SqlError::new(code::STRING_DATA_RIGHT_TRUNCATION, format!("value too long for type character({len})")))
            }
        }
        (Value::Text(s), Type::Text | Type::Varchar) => {
            let max = usize::try_from(typmod - 4).unwrap_or(usize::MAX);
            if s.chars().count() <= max {
                return Ok(Value::Text(s));
            }
            let truncated: String = s.chars().take(max).collect();
            // Trailing spaces may be dropped silently even on assignment.
            if explicit || s.chars().skip(max).all(|c| c == ' ') {
                Ok(Value::Text(truncated))
            } else {
                Err(SqlError::new(code::STRING_DATA_RIGHT_TRUNCATION, format!("value too long for type character varying({max})")))
            }
        }
        (Value::Numeric(n), Type::Numeric) => {
            let t = typmod - 4;
            let precision = u32::try_from((t >> 16) & 0xffff).unwrap_or(0);
            let scale = u32::try_from(t & 0xffff).unwrap_or(0);
            Ok(Value::Numeric(n.apply_typmod(precision, scale)?))
        }
        (v, _) => Ok(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts() {
        assert_eq!(cast_context(&Type::Int4, &Type::Int8), Some(CoercionContext::Implicit));
        assert_eq!(cast_context(&Type::Int8, &Type::Int4), Some(CoercionContext::Assignment));
        assert_eq!(cast_context(&Type::Int4, &Type::Text), Some(CoercionContext::Assignment));
        assert_eq!(cast_context(&Type::Text, &Type::Int4), Some(CoercionContext::Explicit));
        assert_eq!(cast_context(&Type::Uuid, &Type::Int4), None);
    }

    #[test]
    fn conversions() {
        assert_eq!(cast_value(Value::Float8(2.5), &Type::Int4, -1).ok(), Some(Value::Int4(2)));
        assert_eq!(cast_value(Value::Float8(3.5), &Type::Int4, -1).ok(), Some(Value::Int4(4)));
        assert_eq!(cast_value(Value::Int8(1 << 40), &Type::Int4, -1).expect_err("range").message, "integer out of range");
        assert_eq!(cast_value(Value::Text("12".into()), &Type::Int8, -1).ok(), Some(Value::Int8(12)));
        assert_eq!(cast_value(Value::Bool(true), &Type::Text, -1).ok(), Some(Value::Text("true".into())));
        assert_eq!(cast_value(Value::Text("abcdef".into()), &Type::Text, varchar_typmod(3)).ok(), Some(Value::Text("abc".into())));
        assert_eq!(
            cast_value_ctx(Value::Text("abcdef".into()), &Type::Text, varchar_typmod(3), false).expect_err("too long").code,
            "22001"
        );
    }
}
