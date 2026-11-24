//! Text-format input and output functions (`typinput` / `typoutput`), matching Postgres'
//! output byte-for-byte for the settings pgmem reports (`DateStyle=ISO, MDY`, `TimeZone=UTC`,
//! `IntervalStyle=postgres`, `extra_float_digits=1`, `bytea_output=hex`).

use crate::datetime::{
    format_date, format_interval, format_time, format_timestamp, format_timestamptz, format_timetz, parse_date, parse_interval, parse_time,
    parse_timestamp, parse_timestamptz, parse_timetz,
};
use crate::json::Jsonb;
use crate::{SqlError, SqlResult, Type, Value, code};
use std::fmt::Write as _;

/// Renders a non-null value as Postgres text output for `ty`.
pub fn encode(v: &Value, ty: &Type) -> SqlResult<String> {
    Ok(match (v, ty) {
        (Value::Null, _) => return Err(SqlError::internal("text encode of NULL")),
        (Value::Bool(b), _) => (if *b { "t" } else { "f" }).to_owned(),
        (Value::Int2(n), _) => n.to_string(),
        (Value::Int4(n), _) => n.to_string(),
        (Value::Int8(n), _) => n.to_string(),
        (Value::Float4(f), _) => format_float4(*f),
        (Value::Float8(f), Type::Float4) => format_float4(*f as f32),
        (Value::Float8(f), _) => format_float8(*f),
        (Value::Numeric(n), _) => n.to_string(),
        (Value::Text(s), _) => s.clone(),
        (Value::Bytea(b), _) => format_bytea(b),
        (Value::Uuid(u), _) => format_uuid(u),
        (Value::Timestamptz(t), _) => format_timestamptz(*t),
        (Value::Timestamp(t), _) => format_timestamp(*t),
        (Value::Date(d), _) => format_date(*d),
        (Value::Time(t), _) => format_time(*t),
        (Value::TimeTz(t, z), _) => format_timetz(*t, *z),
        (Value::Interval(iv), _) => format_interval(iv),
        (Value::Jsonb(j), _) => j.to_string(),
        (Value::Array(items), ty) => {
            let elem_ty = ty.element().cloned().unwrap_or_else(|| v.natural_type().element().cloned().unwrap_or(Type::Text));
            format_array(items, &elem_ty)?
        }
    })
}

/// Parses Postgres text input for `ty`.
pub fn decode(s: &str, ty: &Type) -> SqlResult<Value> {
    match ty {
        Type::Unknown | Type::Text => Ok(Value::Text(s.to_owned())),
        Type::Bool => parse_bool(s).map(Value::Bool),
        Type::Int2 => parse_int(s, "smallint").and_then(|n| i16::try_from(n).map_err(|_| out_of_range(s, "smallint"))).map(Value::Int2),
        Type::Int4 => parse_int(s, "integer").and_then(|n| i32::try_from(n).map_err(|_| out_of_range(s, "integer"))).map(Value::Int4),
        Type::Int8 => parse_int(s, "bigint").map(Value::Int8),
        Type::Float4 => parse_float(s, "real").map(|f| Value::Float4(f as f32)),
        Type::Float8 => parse_float(s, "double precision").map(Value::Float8),
        Type::Numeric => crate::Numeric::parse(s).map(Value::Numeric),
        Type::Bytea => parse_bytea(s).map(Value::Bytea),
        Type::Uuid => parse_uuid(s).map(Value::Uuid),
        Type::Timestamptz => parse_timestamptz(s).map(Value::Timestamptz),
        Type::Timestamp => parse_timestamp(s).map(Value::Timestamp),
        Type::Date => parse_date(s).map(Value::Date),
        Type::Time => parse_time(s).map(Value::Time),
        Type::TimeTz => parse_timetz(s).map(|(t, z)| Value::TimeTz(t, z)),
        Type::Interval => parse_interval(s).map(Value::Interval),
        Type::Jsonb => Jsonb::parse(s).map(Value::Jsonb),
        Type::Array(elem) => parse_array(s, elem).map(Value::Array),
    }
}

fn out_of_range(s: &str, ty: &str) -> SqlError {
    SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("value \"{s}\" is out of range for type {ty}"))
}

pub fn parse_bool(s: &str) -> SqlResult<bool> {
    let t = s.trim().to_ascii_lowercase();
    let matches = |full: &str, min: usize| t.len() >= min && full.starts_with(t.as_str());
    if matches("true", 1) || matches("yes", 1) || t == "on" || t == "1" {
        Ok(true)
    } else if matches("false", 1) || matches("no", 1) || matches("off", 2) || t == "0" {
        Ok(false)
    } else {
        Err(SqlError::invalid_text("boolean", s))
    }
}

/// Parses an integer the way `int8in` does (surrounding whitespace allowed, optional sign).
pub fn parse_int(s: &str, ty: &str) -> SqlResult<i64> {
    let t = s.trim();
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    let digits = digits.replace('_', "");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SqlError::invalid_text(ty, s));
    }
    let signed = if t.starts_with('-') { format!("-{digits}") } else { digits };
    signed.parse::<i64>().map_err(|_| out_of_range(s, ty))
}

pub fn parse_float(s: &str, ty: &str) -> SqlResult<f64> {
    let t = s.trim();
    match t.to_ascii_lowercase().as_str() {
        "nan" => return Ok(f64::NAN),
        "infinity" | "+infinity" | "inf" | "+inf" => return Ok(f64::INFINITY),
        "-infinity" | "-inf" => return Ok(f64::NEG_INFINITY),
        _ => {}
    }
    let f: f64 = t.parse().map_err(|_| SqlError::invalid_text(ty, s))?;
    if f.is_infinite() {
        return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("\"{s}\" is out of range for type {ty}")));
    }
    Ok(f)
}

/// Shortest round-trip output with Postgres' exponent rule: scientific notation when the
/// decimal exponent is < -4 or >= `max_fixed_exp` (15 for float8, 6 for float4).
fn format_shortest(sci: &str, max_fixed_exp: i32) -> String {
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let (neg, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if exp < -4 || exp >= max_fixed_exp {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.unsigned_abs());
    } else if exp < 0 {
        out.push_str("0.");
        for _ in 0..(-exp - 1) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let point = usize::try_from(exp).unwrap_or(0) + 1;
        if digits.len() <= point {
            out.push_str(&digits);
            for _ in digits.len()..point {
                out.push('0');
            }
        } else {
            out.push_str(&digits[..point]);
            out.push('.');
            out.push_str(&digits[point..]);
        }
    }
    out
}

pub fn format_float8(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    format_shortest(&format!("{f:e}"), 15)
}

pub fn format_float4(f: f32) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    format_shortest(&format!("{f:e}"), 6)
}

pub fn format_bytea(b: &[u8]) -> String {
    let mut out = String::with_capacity(2 + b.len() * 2);
    out.push_str("\\x");
    for byte in b {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `byteain`: the hex format (`\x0102`) or the traditional escape format (`a\000b`, `\\`).
pub fn parse_bytea(s: &str) -> SqlResult<Vec<u8>> {
    if let Some(hex) = s.strip_prefix("\\x").or_else(|| s.strip_prefix("\\X")) {
        let hex: Vec<u8> = hex.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        if !hex.len().is_multiple_of(2) {
            return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "invalid hexadecimal data: odd number of digits"));
        }
        return hex
            .chunks(2)
            .map(|pair| {
                let hi = hex_val(pair[0]);
                let lo = hex_val(pair[1]);
                match (hi, lo) {
                    (Some(h), Some(l)) => Ok(h << 4 | l),
                    _ => Err(SqlError::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid hexadecimal digit: \"{}\"", char::from(if hi.is_none() { pair[0] } else { pair[1] })),
                    )),
                }
            })
            .collect();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
        } else if bytes.get(i + 1) == Some(&b'\\') {
            out.push(b'\\');
            i += 2;
        } else if i + 4 <= bytes.len() && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) && bytes[i + 1] <= b'3' {
            out.push((bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0'));
            i += 4;
        } else {
            return Err(SqlError::invalid_text("bytea", s));
        }
    }
    Ok(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn format_uuid(u: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, byte) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `uuid_in`: 32 hex digits, optionally in braces, with an optional hyphen after any group of
/// four digits.
pub fn parse_uuid(s: &str) -> SqlResult<[u8; 16]> {
    let err = || SqlError::invalid_text("uuid", s);
    let t = s.trim();
    let t = t.strip_prefix('{').and_then(|r| r.strip_suffix('}')).unwrap_or(t);
    let mut out = [0u8; 16];
    let mut n = 0usize;
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' {
            if n == 0 || !n.is_multiple_of(2) || i + 1 == bytes.len() {
                return Err(err());
            }
            i += 1;
            continue;
        }
        let v = hex_val(bytes[i]).ok_or_else(err)?;
        if n >= 32 {
            return Err(err());
        }
        out[n / 2] |= if n.is_multiple_of(2) { v << 4 } else { v };
        n += 1;
        i += 1;
    }
    if n != 32 {
        return Err(err());
    }
    Ok(out)
}

fn format_array(items: &[Value], elem: &Type) -> SqlResult<String> {
    let mut out = String::from("{");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if item.is_null() {
            out.push_str("NULL");
            continue;
        }
        let s = encode(item, elem)?;
        if array_elem_needs_quotes(&s) {
            out.push('"');
            for c in s.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        } else {
            out.push_str(&s);
        }
    }
    out.push('}');
    Ok(out)
}

fn array_elem_needs_quotes(s: &str) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_ascii_whitespace())
}

/// `array_in` for one-dimensional arrays: `{1,2,NULL}`, `{"a b","c\"d"}`, optional
/// `[lo:hi]=` dimension decoration is accepted and ignored.
pub fn parse_array(s: &str, elem: &Type) -> SqlResult<Vec<Value>> {
    let err = || SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("malformed array literal: \"{s}\""));
    let mut t = s.trim();
    if t.starts_with('[') {
        let eq = t.find('=').ok_or_else(err)?;
        t = t[eq + 1..].trim();
    }
    let body = t.strip_prefix('{').and_then(|r| r.strip_suffix('}')).ok_or_else(err)?;
    let mut out = Vec::new();
    if body.trim().is_empty() {
        return Ok(out);
    }
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    loop {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '{' {
            return Err(SqlError::feature_not_supported("multidimensional arrays are not supported"));
        }
        let mut buf = String::new();
        let mut quoted = false;
        if i < chars.len() && chars[i] == '"' {
            quoted = true;
            i += 1;
            loop {
                let c = *chars.get(i).ok_or_else(err)?;
                i += 1;
                match c {
                    '"' => break,
                    '\\' => {
                        buf.push(*chars.get(i).ok_or_else(err)?);
                        i += 1;
                    }
                    c => buf.push(c),
                }
            }
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
        } else {
            while i < chars.len() && chars[i] != ',' {
                if chars[i] == '\\' {
                    i += 1;
                    buf.push(*chars.get(i).ok_or_else(err)?);
                } else if chars[i] == '"' || chars[i] == '{' || chars[i] == '}' {
                    return Err(err());
                } else {
                    buf.push(chars[i]);
                }
                i += 1;
            }
            let trimmed = buf.trim_end().len();
            buf.truncate(trimmed);
        }
        if !quoted && buf.eq_ignore_ascii_case("null") {
            out.push(Value::Null);
        } else {
            out.push(decode(&buf, elem)?);
        }
        match chars.get(i) {
            None => break,
            Some(',') => i += 1,
            Some(_) => return Err(err()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_match_postgres() {
        assert_eq!(format_float8(1.5), "1.5");
        assert_eq!(format_float8(100.0), "100");
        assert_eq!(format_float8(1e100), "1e+100");
        assert_eq!(format_float8(0.000_012_34), "1.234e-05");
        assert_eq!(format_float8(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(format_float8(123_456_789_012_345.0), "123456789012345");
        assert_eq!(format_float8(1_234_567_890_123_456.0), "1.234567890123456e+15");
        assert_eq!(format_float4(1.1), "1.1");
        assert_eq!(format_float4(1_234_567.0), "1.234567e+06");
        assert_eq!(format_float8(f64::NAN), "NaN");
    }

    #[test]
    fn ints() {
        assert_eq!(decode(" 42 ", &Type::Int4).ok(), Some(Value::Int4(42)));
        let e = decode("abc", &Type::Int4).expect_err("bad");
        assert_eq!(e.code, "22P02");
        assert_eq!(e.message, "invalid input syntax for type integer: \"abc\"");
        assert_eq!(decode("3000000000", &Type::Int4).expect_err("range").code, "22003");
    }

    #[test]
    fn bools() {
        for t in ["t", "TRUE", "yes", "on", "1", "y"] {
            assert_eq!(parse_bool(t).ok(), Some(true), "{t}");
        }
        for f in ["f", "false", "no", "off", "0", "n"] {
            assert_eq!(parse_bool(f).ok(), Some(false), "{f}");
        }
        assert!(parse_bool("o").is_err());
    }

    #[test]
    fn bytea_and_uuid() {
        assert_eq!(parse_bytea("\\x00ff").ok(), Some(vec![0, 255]));
        assert_eq!(parse_bytea("a\\000b\\\\").ok(), Some(vec![b'a', 0, b'b', b'\\']));
        assert_eq!(format_bytea(&[0xde, 0xad]), "\\xdead");
        let u = parse_uuid("{A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11}").expect("uuid");
        assert_eq!(format_uuid(&u), "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
        assert!(parse_uuid("a0eebc99").is_err());
    }

    #[test]
    fn arrays() {
        let v = parse_array("{1, 2,NULL}", &Type::Int4).expect("array");
        assert_eq!(v, vec![Value::Int4(1), Value::Int4(2), Value::Null]);
        let v = parse_array(r#"{"a b","c\"d",x}"#, &Type::Text).expect("array");
        assert_eq!(v, vec![Value::Text("a b".into()), Value::Text("c\"d".into()), Value::Text("x".into())]);
        let out = format_array(&v, &Type::Text).expect("format");
        assert_eq!(out, r#"{"a b","c\"d",x}"#);
        assert_eq!(format_array(&[Value::Text(String::new()), Value::Null], &Type::Text).expect("f"), r#"{"",NULL}"#);
    }
}
