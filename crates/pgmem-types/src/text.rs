//! Text-format input and output functions (`typinput` / `typoutput`), matching Postgres'
//! output byte-for-byte for the settings pgmem reports (`DateStyle=ISO, MDY`, `TimeZone=UTC`,
//! `IntervalStyle=postgres`, `extra_float_digits=1`, `bytea_output=hex`).

use crate::datetime::{
    format_time, format_timetz, parse_date, parse_interval, parse_time, parse_timestamp, parse_timestamptz, parse_timetz,
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
        (Value::Float4(f), _) => float4_out(*f),
        (Value::Float8(f), Type::Float4) => float4_out(*f as f32),
        (Value::Float8(f), _) => float8_out(*f),
        (Value::Numeric(n), _) => n.to_string(),
        (Value::Text(s), _) => s.clone(),
        (Value::Bytea(b), _) => format_bytea(b),
        (Value::Uuid(u), _) => format_uuid(u),
        (Value::Timestamptz(t), _) => crate::datetime::timestamptz_out(*t),
        (Value::Timestamp(t), _) => crate::datetime::timestamp_out(*t),
        (Value::Date(d), _) => crate::datetime::date_out(*d),
        (Value::Time(t), _) => format_time(*t),
        (Value::TimeTz(t, z), _) => format_timetz(*t, *z),
        (Value::Interval(iv), _) => crate::datetime::interval_out(iv),
        (Value::Jsonb(j), _) => j.to_string(),
        (Value::Json(s), _) => s.clone(),
        (Value::Oid(o), _) => o.to_string(),
        (Value::Reg(_, _, name), _) => name.clone(),
        (Value::Enum(t, label), _) => crate::usertype::enum_label(*t, *label)?,
        (Value::Record(t, fields), _) => crate::usertype::format_record(*t, fields)?,
        (Value::Array(items), Type::Int2Vector | Type::OidVector) => {
            items.iter().map(|i| encode(i, &i.natural_type())).collect::<SqlResult<Vec<_>>>()?.join(" ")
        }
        (Value::Array(items), ty) => {
            let elem_ty = ty.element().cloned().unwrap_or_else(|| v.natural_type().element().cloned().unwrap_or(Type::Text));
            format_array(items, &elem_ty)?
        }
    })
}

/// Parses Postgres text input for `ty`.
pub fn decode(s: &str, ty: &Type) -> SqlResult<Value> {
    match ty {
        Type::Unknown | Type::Text | Type::Varchar | Type::Bpchar => Ok(Value::Text(s.to_owned())),
        Type::Void => Ok(Value::Text(String::new())),
        Type::Int2Vector => s
            .split_whitespace()
            .map(|p| parse_int(p, "smallint").and_then(|n| i16::try_from(n).map_err(|_| out_of_range(p, "smallint"))).map(Value::Int2))
            .collect::<SqlResult<Vec<_>>>()
            .map(Value::Array),
        Type::OidVector => s.split_whitespace().map(|p| parse_oid(p).map(Value::Oid)).collect::<SqlResult<Vec<_>>>().map(Value::Array),
        Type::Name => Ok(Value::Text(truncate_name(s))),
        // "char" keeps the first byte (an ASCII character for every catalog use).
        Type::Char => Ok(Value::Text(s.chars().next().map(String::from).unwrap_or_default())),
        Type::Json => {
            Jsonb::parse(s)?;
            Ok(Value::Json(s.to_owned()))
        }
        Type::Oid => parse_oid(s).map(Value::Oid),
        // reg* input needs the catalog; the executor resolves names (see pgmem-exec `reg`).
        Type::Reg(_) => parse_oid(s).map(|o| {
            Value::Reg(
                match ty {
                    Type::Reg(k) => *k,
                    _ => crate::RegKind::Class,
                },
                o,
                o.to_string(),
            )
        }),
        Type::Bool => parse_bool(s).map(Value::Bool),
        Type::Int2 => parse_int(s, "smallint").and_then(|n| i16::try_from(n).map_err(|_| out_of_range(s, "smallint"))).map(Value::Int2),
        Type::Int4 => parse_int(s, "integer").and_then(|n| i32::try_from(n).map_err(|_| out_of_range(s, "integer"))).map(Value::Int4),
        Type::Int8 => parse_int(s, "bigint").map(Value::Int8),
        Type::Float4 => parse_float4(s).map(Value::Float4),
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
        Type::JsonPath => crate::jsonpath::normalize(s).map(Value::Text),
        Type::Array(elem) => parse_array(s, elem).map(Value::Array),
        Type::Enum(oid) => crate::usertype::parse_enum(s, *oid),
        Type::Composite(oid) => crate::usertype::parse_record(s, *oid),
        // Domain input is the base type's; the executor checks the constraints.
        Type::Domain(_) => decode(s, &ty.base()),
        Type::Record => Err(SqlError::feature_not_supported("input of anonymous composite types is not implemented")),
    }
}

/// `name` input: at most 63 bytes, cut at a character boundary.
pub fn truncate_name(s: &str) -> String {
    let mut end = s.len().min(63);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// `oidin`: an unsigned 32-bit integer; negative input wraps, as in Postgres.
pub fn parse_oid(s: &str) -> SqlResult<u32> {
    let n = parse_int(s, "oid")?;
    if !(-2_147_483_648..=4_294_967_295).contains(&n) {
        return Err(out_of_range(s, "oid"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(n as u32)
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
    // Non-decimal integer input (Postgres 16): 0x1F, 0o17, 0b101.
    let radix = match digits.get(..2).map(str::to_ascii_lowercase).as_deref() {
        Some("0x") => 16,
        Some("0o") => 8,
        Some("0b") => 2,
        _ => 10,
    };
    if radix != 10 {
        let body = digits[2..].replace('_', "");
        if body.is_empty() || !body.chars().all(|c| c.is_digit(radix)) {
            return Err(SqlError::invalid_text(ty, s));
        }
        let n = i128::from_str_radix(&body, radix).map_err(|_| out_of_range(s, ty))?;
        let n = if t.starts_with('-') { -n } else { n };
        return i64::try_from(n).map_err(|_| out_of_range(s, ty));
    }
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
    if f.is_infinite() || (f == 0.0 && nonzero_mantissa(t)) {
        return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("\"{s}\" is out of range for type {ty}")));
    }
    Ok(f)
}

/// `float4in`: parsed as single precision, so overflow and underflow are judged for `real`
/// (strtof's ERANGE), not after rounding a double.
pub fn parse_float4(s: &str) -> SqlResult<f32> {
    let f = parse_float(s, "real")?;
    if f.is_nan() || f.is_infinite() {
        #[allow(clippy::cast_possible_truncation)]
        return Ok(f as f32);
    }
    let t = s.trim();
    let g: f32 = t.parse().map_err(|_| SqlError::invalid_text("real", s))?;
    if g.is_infinite() || (g == 0.0 && nonzero_mantissa(t)) {
        return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("\"{s}\" is out of range for type real")));
    }
    Ok(g)
}

/// True if a numeric literal's digits (before any exponent) are not all zero: a zero result
/// then means the value underflowed.
fn nonzero_mantissa(t: &str) -> bool {
    t.split(['e', 'E']).next().unwrap_or("").bytes().any(|b| (b'1'..=b'9').contains(&b))
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

/// `float8out` under `extra_float_digits`: shortest round-trip when positive, else
/// `%.{15 + extra_float_digits}g`.
pub fn float8_out(f: f64) -> String {
    let efd = crate::context::output_style().extra_float_digits;
    if efd > 0 || !f.is_finite() || f == 0.0 {
        return format_float8(f);
    }
    format_precision(&format!("{:.*e}", usize::try_from((14 + efd).max(0)).unwrap_or(0), f), (15 + efd).max(1))
}

/// `float4out` under `extra_float_digits` (`%.{6 + extra_float_digits}g` when not positive).
pub fn float4_out(f: f32) -> String {
    let efd = crate::context::output_style().extra_float_digits;
    if efd > 0 || !f.is_finite() || f == 0.0 {
        return format_float4(f);
    }
    format_precision(&format!("{:.*e}", usize::try_from((5 + efd).max(0)).unwrap_or(0), f), (6 + efd).max(1))
}

/// `%g`: a `{:.Ne}` rendering with trailing zeros dropped, in fixed notation unless the
/// exponent is < -4 or >= the precision.
fn format_precision(sci: &str, precision: i32) -> String {
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci, "0"));
    let mantissa = if mantissa.contains('.') { mantissa.trim_end_matches('0').trim_end_matches('.') } else { mantissa };
    format_shortest(&format!("{mantissa}e{exp}"), precision)
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

/// `byteaout` under the session's `bytea_output`.
pub fn format_bytea(b: &[u8]) -> String {
    if crate::context::bytea_escape() { format_bytea_escape(b) } else { format_bytea_hex(b) }
}

/// The escape output format: printable ASCII as is, `\\` for a backslash, `\ooo` octal
/// for everything else.
pub fn format_bytea_escape(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for &byte in b {
        match byte {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => {
                let _ = write!(out, "\\{byte:03o}");
            }
        }
    }
    out
}

pub fn format_bytea_hex(b: &[u8]) -> String {
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
            return Err(SqlError::new(code::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type bytea"));
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
    let mut out = String::new();
    write_array(&mut out, items, elem)?;
    Ok(out)
}

/// `array_out`; nested arrays are inner dimensions (`{{1,2},{3,4}}`).
fn write_array(out: &mut String, items: &[Value], elem: &Type) -> SqlResult<()> {
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            Value::Null => out.push_str("NULL"),
            Value::Array(inner) if !matches!(elem, Type::Array(_)) => write_array(out, inner, elem)?,
            _ => {
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
        }
    }
    out.push('}');
    Ok(())
}

fn array_elem_needs_quotes(s: &str) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars().any(|c| matches!(c, '{' | '}' | ',' | '"' | '\\') || c.is_ascii_whitespace())
}

/// `array_in`: `{1,2,NULL}`, `{"a b","c\"d"}`, `{{1,2},{3,4}}` (rectangular; nested arrays
/// are returned as inner dimensions). An optional `[lo:hi]=` dimension decoration is accepted
/// and ignored.
pub fn parse_array(s: &str, elem: &Type) -> SqlResult<Vec<Value>> {
    let err = || SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("malformed array literal: \"{s}\""));
    let mut t = s.trim();
    if t.starts_with('[') {
        let eq = t.find('=').ok_or_else(err)?;
        t = t[eq + 1..].trim();
    }
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    let v = parse_array_level(&chars, &mut i, elem, &err)?;
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    if i != chars.len() {
        return Err(err());
    }
    let Value::Array(items) = v else { return Err(err()) };
    if !rectangular(&items) {
        return Err(err().with_detail("Multidimensional arrays must have sub-arrays with matching dimensions."));
    }
    // `{{}}` is an empty array.
    if items.iter().all(|x| matches!(x, Value::Array(a) if a.is_empty())) && !items.is_empty() {
        return Ok(Vec::new());
    }
    Ok(items)
}

/// True if every nested level has sub-arrays of one length (and no level mixes arrays with
/// scalars).
pub fn rectangular(items: &[Value]) -> bool {
    let arrays = items.iter().filter(|x| matches!(x, Value::Array(_))).count();
    if arrays == 0 {
        return true;
    }
    if arrays != items.len() {
        return false;
    }
    let subs: Vec<&Vec<Value>> = items.iter().filter_map(|x| if let Value::Array(a) = x { Some(a) } else { None }).collect();
    let len = subs[0].len();
    subs.iter().all(|a| a.len() == len && rectangular(a))
        && subs.iter().map(|a| a.iter().filter(|x| matches!(x, Value::Array(_))).count()).collect::<std::collections::HashSet<_>>().len()
            <= 1
}

fn parse_array_level(chars: &[char], i: &mut usize, elem: &Type, err: &dyn Fn() -> SqlError) -> SqlResult<Value> {
    let skip_ws = |i: &mut usize| {
        while *i < chars.len() && chars[*i].is_whitespace() {
            *i += 1;
        }
    };
    skip_ws(i);
    if chars.get(*i) != Some(&'{') {
        return Err(err());
    }
    *i += 1;
    let mut out = Vec::new();
    skip_ws(i);
    if chars.get(*i) == Some(&'}') {
        *i += 1;
        return Ok(Value::Array(out));
    }
    loop {
        skip_ws(i);
        if chars.get(*i) == Some(&'{') {
            out.push(parse_array_level(chars, i, elem, err)?);
        } else {
            let mut buf = String::new();
            let mut quoted = false;
            if chars.get(*i) == Some(&'"') {
                quoted = true;
                *i += 1;
                loop {
                    let c = *chars.get(*i).ok_or_else(err)?;
                    *i += 1;
                    match c {
                        '"' => break,
                        '\\' => {
                            buf.push(*chars.get(*i).ok_or_else(err)?);
                            *i += 1;
                        }
                        c => buf.push(c),
                    }
                }
            } else {
                while *i < chars.len() && chars[*i] != ',' && chars[*i] != '}' {
                    if chars[*i] == '\\' {
                        *i += 1;
                        buf.push(*chars.get(*i).ok_or_else(err)?);
                    } else if chars[*i] == '"' || chars[*i] == '{' {
                        return Err(err());
                    } else {
                        buf.push(chars[*i]);
                    }
                    *i += 1;
                }
                let trimmed = buf.trim_end().len();
                buf.truncate(trimmed);
                if buf.is_empty() {
                    return Err(err());
                }
            }
            if !quoted && buf.eq_ignore_ascii_case("null") {
                out.push(Value::Null);
            } else {
                out.push(decode(&buf, elem)?);
            }
        }
        skip_ws(i);
        match chars.get(*i) {
            Some(',') => *i += 1,
            Some('}') => {
                *i += 1;
                return Ok(Value::Array(out));
            }
            _ => return Err(err()),
        }
    }
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
