//! Functions and operators of the bit string, money and network address types.

use super::{Resolved, no_operator, resolved, strict};
use crate::eval::EvalCtx;
use pgmem_types::netbits::{masked, max_bits};
use pgmem_types::{Numeric, SqlError, SqlResult, Type, Value, code};
use std::net::IpAddr;

fn is_bits(t: &Type) -> bool {
    matches!(t, Type::Bit | Type::VarBit)
}

fn is_net(t: &Type) -> bool {
    matches!(t, Type::Inet | Type::Cidr)
}

fn bits(v: &Value) -> SqlResult<&str> {
    match v {
        Value::Bit(b) => Ok(b),
        _ => Err(SqlError::internal("bit string expected")),
    }
}

fn inet(v: &Value) -> SqlResult<(IpAddr, u8)> {
    match v {
        Value::Inet(ip, b) => Ok((*ip, *b)),
        _ => Err(SqlError::internal("inet value expected")),
    }
}

fn money(v: &Value) -> SqlResult<i64> {
    match v {
        Value::Money(m) => Ok(*m),
        _ => Err(SqlError::internal("money value expected")),
    }
}

// ---------------------------------------------------------------------------------------------
// Bit strings
// ---------------------------------------------------------------------------------------------

fn bit_concat(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bit(format!("{}{}", bits(&a[0])?, bits(&a[1])?)))
}

fn bitwise(a: &[Value], name: &str, f: fn(bool, bool) -> bool) -> SqlResult<Value> {
    let (x, y) = (bits(&a[0])?, bits(&a[1])?);
    if x.len() != y.len() {
        return Err(SqlError::new(code::STRING_DATA_LENGTH_MISMATCH, format!("cannot {name} bit strings of different sizes")));
    }
    Ok(Value::Bit(x.bytes().zip(y.bytes()).map(|(p, q)| if f(p == b'1', q == b'1') { '1' } else { '0' }).collect()))
}

fn bit_and(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    bitwise(a, "AND", |p, q| p && q)
}
fn bit_or(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    bitwise(a, "OR", |p, q| p || q)
}
fn bit_xor(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    bitwise(a, "XOR", |p, q| p != q)
}

/// `<<` / `>>`: shifts keeping the length, zero-filling.
fn bit_shift(a: &[Value], left: bool) -> SqlResult<Value> {
    let b = bits(&a[0])?;
    let n = a[1].as_i64().unwrap_or(0);
    let left = if n < 0 { !left } else { left };
    let k = usize::try_from(n.unsigned_abs()).unwrap_or(usize::MAX).min(b.len());
    let zeros = "0".repeat(k);
    Ok(Value::Bit(if left { format!("{}{zeros}", &b[k..]) } else { format!("{zeros}{}", &b[..b.len() - k]) }))
}

fn bit_shl(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    bit_shift(a, true)
}
fn bit_shr(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    bit_shift(a, false)
}

fn bit_index(b: &str, n: &Value) -> SqlResult<usize> {
    let i = n.as_i64().unwrap_or(-1);
    usize::try_from(i).ok().filter(|i| *i < b.len()).ok_or_else(|| {
        SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, format!("bit index {i} out of valid range (0..{})", b.len().saturating_sub(1)))
    })
}

fn get_bit(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let b = bits(&a[0])?;
    let i = bit_index(b, &a[1])?;
    Ok(Value::Int4(i32::from(b.as_bytes()[i] == b'1')))
}

fn set_bit(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let b = bits(&a[0])?;
    let i = bit_index(b, &a[1])?;
    let v = match a[2].as_i64() {
        Some(0) => '0',
        Some(1) => '1',
        _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "new bit must be 0 or 1")),
    };
    let mut out: Vec<char> = b.chars().collect();
    out[i] = v;
    Ok(Value::Bit(out.into_iter().collect()))
}

fn len4(n: usize) -> Value {
    Value::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

// ---------------------------------------------------------------------------------------------
// money
// ---------------------------------------------------------------------------------------------

fn money_out_of_range() -> SqlError {
    SqlError::out_of_range("money")
}

fn money_add(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Money(money(&a[0])?.checked_add(money(&a[1])?).ok_or_else(money_out_of_range)?))
}

fn money_sub(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Money(money(&a[0])?.checked_sub(money(&a[1])?).ok_or_else(money_out_of_range)?))
}

fn as_numeric(v: &Value) -> SqlResult<Numeric> {
    match v {
        Value::Numeric(n) => Ok(n.clone()),
        Value::Float4(_) | Value::Float8(_) => Numeric::from_f64(v.as_f64().unwrap_or_default()),
        other => Ok(Numeric::from_i64(other.as_i64().unwrap_or_default())),
    }
}

fn money_from(n: &Numeric) -> SqlResult<Value> {
    Ok(Value::Money(n.round(0).to_i64().ok_or_else(money_out_of_range)?))
}

fn money_mul(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (m, k) = if matches!(a[0], Value::Money(_)) { (&a[0], &a[1]) } else { (&a[1], &a[0]) };
    money_from(&Numeric::from_i64(money(m)?).mul(&as_numeric(k)?))
}

fn money_div(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let d = as_numeric(&a[1])?;
    if d.is_zero() {
        return Err(SqlError::new(code::DIVISION_BY_ZERO, "division by zero"));
    }
    // cash_div_int truncates; the float and numeric variants round.
    let q = Numeric::from_i64(money(&a[0])?).div(&d)?;
    if a[1].as_i64().is_some() { Ok(Value::Money(q.trunc(0).to_i64().ok_or_else(money_out_of_range)?)) } else { money_from(&q) }
}

fn money_ratio(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let d = money(&a[1])?;
    if d == 0 {
        return Err(SqlError::new(code::DIVISION_BY_ZERO, "division by zero"));
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(Value::Float8(money(&a[0])? as f64 / d as f64))
}

// ---------------------------------------------------------------------------------------------
// inet / cidr
// ---------------------------------------------------------------------------------------------

fn host(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Text(inet(&a[0])?.0.to_string()))
}

fn masklen(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Int4(i32::from(inet(&a[0])?.1)))
}

fn family(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Int4(if inet(&a[0])?.0.is_ipv4() { 4 } else { 6 }))
}

fn network(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (ip, b) = inet(&a[0])?;
    Ok(Value::Inet(masked(&ip, b, false), b))
}

fn broadcast(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (ip, b) = inet(&a[0])?;
    Ok(Value::Inet(masked(&ip, b, true), b))
}

/// The netmask / hostmask as an address of full length.
fn mask_addr(ip: &IpAddr, bits: u8, host: bool) -> IpAddr {
    let all = match ip {
        IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::BROADCAST),
        IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::from(u128::MAX)),
    };
    let net = masked(&all, bits, false);
    if !host {
        return net;
    }
    match net {
        IpAddr::V4(v) => IpAddr::V4(std::net::Ipv4Addr::from(!u32::from(v))),
        IpAddr::V6(v) => IpAddr::V6(std::net::Ipv6Addr::from(!u128::from(v))),
    }
}

fn netmask(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (ip, b) = inet(&a[0])?;
    Ok(Value::Inet(mask_addr(&ip, b, false), max_bits(&ip)))
}

fn hostmask(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (ip, b) = inet(&a[0])?;
    Ok(Value::Inet(mask_addr(&ip, b, true), max_bits(&ip)))
}

fn set_masklen(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (ip, _) = inet(&a[0])?;
    let n = a[1].as_i64().unwrap_or(-1);
    let max = max_bits(&ip);
    let bits = if n == -1 {
        max
    } else {
        u8::try_from(n)
            .ok()
            .filter(|b| *b <= max)
            .ok_or_else(|| SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid mask length: {n}")))?
    };
    Ok(Value::Inet(ip, bits))
}

fn inet_text(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (ip, b) = inet(&a[0])?;
    Ok(Value::Text(format!("{ip}/{b}")))
}

/// Whether network `outer` contains `inner` (`<<=` when `eq`, `<<` otherwise).
fn net_contains(outer: (IpAddr, u8), inner: (IpAddr, u8), eq: bool) -> bool {
    outer.0.is_ipv4() == inner.0.is_ipv4()
        && (inner.1 > outer.1 || (eq && inner.1 == outer.1))
        && masked(&inner.0, outer.1, false) == masked(&outer.0, outer.1, false)
}

fn sub_le(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(net_contains(inet(&a[1])?, inet(&a[0])?, true)))
}
fn sub_lt(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(net_contains(inet(&a[1])?, inet(&a[0])?, false)))
}
fn sup_ge(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(net_contains(inet(&a[0])?, inet(&a[1])?, true)))
}
fn sup_gt(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(net_contains(inet(&a[0])?, inet(&a[1])?, false)))
}
fn net_overlap(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (x, y) = (inet(&a[0])?, inet(&a[1])?);
    Ok(Value::Bool(net_contains(x, y, true) || net_contains(y, x, true)))
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let Some(first) = args.first() else { return Ok(None) };
    if is_bits(first) {
        let f = match (name, args.len()) {
            ("length" | "bit_length", 1) => {
                return resolved(strict("length", |a, _| Ok(len4(bits(&a[0])?.len()))), vec![first.clone()], Type::Int4);
            }
            ("octet_length", 1) => {
                return resolved(strict("octet_length", |a, _| Ok(len4(bits(&a[0])?.len().div_ceil(8)))), vec![first.clone()], Type::Int4);
            }
            ("bit_count", 1) => {
                let f = strict("bit_count", |a, _| Ok(Value::Int8(i64::try_from(bits(&a[0])?.matches('1').count()).unwrap_or(0))));
                return resolved(f, vec![first.clone()], Type::Int8);
            }
            ("get_bit", 2) => return resolved(strict("get_bit", get_bit), vec![first.clone(), Type::Int4], Type::Int4),
            ("set_bit", 3) => strict("set_bit", set_bit),
            _ => return Ok(None),
        };
        return resolved(f, vec![first.clone(), Type::Int4, Type::Int4], first.clone());
    }
    if !is_net(first) || args.len() > 2 {
        return Ok(None);
    }
    let one = |f, ret: Type| resolved(f, vec![first.clone()], ret);
    match (name, args.len()) {
        ("host", 1) => one(strict("host", host), Type::Text),
        ("text", 1) => one(strict("text", inet_text), Type::Text),
        ("abbrev", 1) => one(strict("abbrev", |a, _| Ok(Value::Text(pgmem_types::text::encode(&a[0], &Type::Inet)?))), Type::Text),
        ("masklen", 1) => one(strict("masklen", masklen), Type::Int4),
        ("family", 1) => one(strict("family", family), Type::Int4),
        ("network", 1) => one(strict("network", network), Type::Cidr),
        ("broadcast", 1) => one(strict("broadcast", broadcast), Type::Inet),
        ("netmask", 1) => one(strict("netmask", netmask), Type::Inet),
        ("hostmask", 1) => one(strict("hostmask", hostmask), Type::Inet),
        ("set_masklen", 2) => resolved(strict("set_masklen", set_masklen), vec![first.clone(), Type::Int4], first.clone()),
        _ => Ok(None),
    }
}

pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    let other = |t: &Type, of: &Type| if *t == Type::Unknown { of.clone() } else { t.clone() };
    if is_bits(l) || is_bits(r) {
        let (lt, rt) = (other(l, r), other(r, l));
        let f = match op {
            "||" if is_bits(&lt) && is_bits(&rt) => return resolved(strict("||", bit_concat), vec![lt, rt], Type::VarBit),
            "&" => strict("&", bit_and),
            "|" => strict("|", bit_or),
            "#" => strict("#", bit_xor),
            "<<" if is_bits(l) => return resolved(strict("<<", bit_shl), vec![l.clone(), Type::Int4], l.clone()),
            ">>" if is_bits(l) => return resolved(strict(">>", bit_shr), vec![l.clone(), Type::Int4], l.clone()),
            _ => return Ok(None),
        };
        if !(is_bits(&lt) && is_bits(&rt)) {
            return Err(no_operator(op, Some(l), r));
        }
        return resolved(f, vec![lt.clone(), rt], lt);
    }
    if *l == Type::Money || *r == Type::Money {
        let (lt, rt) = (other(l, r), other(r, l));
        return match (op, &lt, &rt) {
            ("+", Type::Money, Type::Money) => resolved(strict("+", money_add), vec![lt.clone(), rt], Type::Money),
            ("-", Type::Money, Type::Money) => resolved(strict("-", money_sub), vec![lt.clone(), rt], Type::Money),
            ("/", Type::Money, Type::Money) => resolved(strict("/", money_ratio), vec![lt, rt], Type::Float8),
            ("*", Type::Money, n) | ("*", n, Type::Money) if n.is_numeric() => {
                resolved(strict("*", money_mul), vec![lt.clone(), rt.clone()], Type::Money)
            }
            ("/", Type::Money, n) if n.is_numeric() => resolved(strict("/", money_div), vec![lt.clone(), rt.clone()], Type::Money),
            _ => Ok(None),
        };
    }
    if is_net(l) || is_net(r) {
        let (lt, rt) = (other(l, r), other(r, l));
        if !(is_net(&lt) && is_net(&rt)) {
            return Ok(None);
        }
        let f = match op {
            "<<=" => strict("<<=", sub_le),
            "<<" => strict("<<", sub_lt),
            ">>=" => strict(">>=", sup_ge),
            ">>" => strict(">>", sup_gt),
            "&&" => strict("&&", net_overlap),
            _ => return Ok(None),
        };
        return resolved(f, vec![Type::Inet, Type::Inet], Type::Bool);
    }
    Ok(None)
}
