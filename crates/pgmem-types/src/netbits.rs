//! Bit strings (`varbit.c`), `money` (`cash.c`, lc_monetary C) and the network address
//! types `inet`, `cidr`, `macaddr` and `macaddr8` (`network.c`, `mac.c`, `mac8.c`).

use crate::{SqlError, SqlResult, code};
use std::cmp::Ordering;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn invalid(ty: &str, s: &str) -> SqlError {
    SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type {ty}: \"{s}\""))
}

// ---------------------------------------------------------------------------------------------
// Bit strings
// ---------------------------------------------------------------------------------------------

/// `bit_in` / `varbit_in`: binary digits, or `x` followed by hex digits.
pub fn parse_bits(s: &str) -> SqlResult<String> {
    let (hex, digits) = match s.as_bytes().first() {
        Some(b'x' | b'X') => (true, &s[1..]),
        Some(b'b' | b'B') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut out = String::with_capacity(digits.len() * if hex { 4 } else { 1 });
    for c in digits.chars() {
        if hex {
            let d = c
                .to_digit(16)
                .ok_or_else(|| SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("\"{c}\" is not a valid hexadecimal digit")))?;
            out.push_str(&format!("{d:04b}"));
        } else if c == '0' || c == '1' {
            out.push(c);
        } else {
            return Err(SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("\"{c}\" is not a valid binary digit")));
        }
    }
    Ok(out)
}

/// `bit_send`: the bit length, then the bits packed most significant first.
pub fn bits_to_binary(bits: &str) -> Vec<u8> {
    let mut out = i32::try_from(bits.len()).unwrap_or(i32::MAX).to_be_bytes().to_vec();
    for chunk in bits.as_bytes().chunks(8) {
        let mut byte = 0u8;
        for (i, b) in chunk.iter().enumerate() {
            if *b == b'1' {
                byte |= 0x80 >> i;
            }
        }
        out.push(byte);
    }
    out
}

pub fn bits_from_binary(b: &[u8]) -> SqlResult<String> {
    let bad = || SqlError::new(code::INVALID_BINARY_REPRESENTATION, "invalid length in external bit string");
    let len = b.get(..4).ok_or_else(bad)?;
    let n = usize::try_from(i32::from_be_bytes(len.try_into().map_err(|_| bad())?)).map_err(|_| bad())?;
    let data = &b[4..];
    if data.len() != n.div_ceil(8) {
        return Err(bad());
    }
    Ok((0..n).map(|i| if data[i / 8] & (0x80 >> (i % 8)) != 0 { '1' } else { '0' }).collect())
}

// ---------------------------------------------------------------------------------------------
// money
// ---------------------------------------------------------------------------------------------

/// `cash_in` for lc_monetary C: `[-]$1,234.56`, `(1.00)`, with at most two decimals
/// (more are rounded).
pub fn parse_money(s: &str) -> SqlResult<i64> {
    let err = || invalid("money", s);
    let mut t = s.trim();
    let mut negative = false;
    if let Some(inner) = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) {
        negative = true;
        t = inner.trim();
    }
    if let Some(rest) = t.strip_prefix('-') {
        negative = !negative;
        t = rest.trim_start();
    } else if let Some(rest) = t.strip_prefix('+') {
        t = rest.trim_start();
    }
    t = t.strip_prefix('$').unwrap_or(t).trim_start();
    if let Some(rest) = t.strip_prefix('-') {
        negative = !negative;
        t = rest;
    }
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    let int: String = int.chars().filter(|c| *c != ',').collect();
    if int.is_empty() && frac.is_empty() || !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(err());
    }
    let out_of_range = || SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("value \"{s}\" is out of range for type money"));
    let mut cents: i128 = if int.is_empty() { 0 } else { int.parse::<i128>().map_err(|_| out_of_range())? * 100 };
    let digits: Vec<u32> = frac.chars().filter_map(|c| c.to_digit(10)).collect();
    cents += i128::from(digits.first().copied().unwrap_or(0) * 10 + digits.get(1).copied().unwrap_or(0));
    if digits.get(2).is_some_and(|d| *d >= 5) {
        cents += 1;
    }
    if negative {
        cents = -cents;
    }
    i64::try_from(cents).map_err(|_| out_of_range())
}

/// `cash_out` for lc_monetary C: `$1,234.56`, `-$1.00`.
pub fn format_money(cents: i64) -> String {
    let neg = cents < 0;
    let abs = cents.unsigned_abs();
    let int = (abs / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{}${grouped}.{:02}", if neg { "-" } else { "" }, abs % 100)
}

// ---------------------------------------------------------------------------------------------
// inet / cidr
// ---------------------------------------------------------------------------------------------

pub fn max_bits(a: &IpAddr) -> u8 {
    if a.is_ipv4() { 32 } else { 128 }
}

fn addr_bits(a: &IpAddr) -> u128 {
    match a {
        IpAddr::V4(v) => u128::from(u32::from(*v)),
        IpAddr::V6(v) => u128::from(*v),
    }
}

/// The address with the bits right of the mask cleared (or set, for `broadcast`).
pub fn masked(a: &IpAddr, bits: u8, set: bool) -> IpAddr {
    let width = u32::from(max_bits(a));
    let host = u32::from(bits).min(width);
    let mask: u128 = if host == 0 { 0 } else { (!0u128) << (128 - host) >> (128 - width) };
    let full: u128 = if width == 128 { !0 } else { (1u128 << width) - 1 };
    let v = addr_bits(a);
    let out = if set { v | (full & !mask) } else { v & mask };
    match a {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from(u32::try_from(out).unwrap_or(0))),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from(out)),
    }
}

/// An IPv4 `cidr` written with fewer than four octets (`10.1`) and its classful default
/// mask (`inet_cidr_pton_ipv4`).
fn parse_short_v4(s: &str) -> Option<(Ipv4Addr, Option<u8>)> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        octets[i] = p.parse().ok()?;
    }
    let given = u8::try_from(parts.len()).ok()?;
    let first = octets[0];
    let mut bits: u8 = if first >= 240 {
        32
    } else if first >= 224 {
        8
    } else if first >= 192 {
        24
    } else if first >= 128 {
        16
    } else {
        8
    };
    if bits < given * 8 {
        bits = given * 8;
    }
    if bits == 8 && first == 224 {
        bits = 4;
    }
    Some((Ipv4Addr::from(octets), Some(bits)))
}

/// `inet_in` / `cidr_in`.
pub fn parse_inet(s: &str, cidr: bool) -> SqlResult<(IpAddr, u8)> {
    let ty = if cidr { "cidr" } else { "inet" };
    let t = s.trim();
    let (addr, mask) = match t.split_once('/') {
        Some((a, m)) => (a, Some(m)),
        None => (t, None),
    };
    let (ip, default_bits) = match addr.parse::<IpAddr>() {
        Ok(ip) => (ip, None),
        Err(_) if cidr => match parse_short_v4(addr) {
            Some((v4, b)) => (IpAddr::V4(v4), b),
            None => return Err(invalid(ty, s)),
        },
        Err(_) => return Err(invalid(ty, s)),
    };
    let max = max_bits(&ip);
    let bits = match mask {
        Some(m) => m.parse::<u8>().ok().filter(|b| *b <= max).ok_or_else(|| invalid(ty, s))?,
        None if cidr && ip.is_ipv4() => match default_bits {
            Some(b) => b,
            None => parse_short_v4(addr).and_then(|(_, b)| b).unwrap_or(max),
        },
        None => max,
    };
    if cidr && masked(&ip, bits, false) != ip {
        return Err(SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("invalid cidr value: \"{s}\""))
            .with_detail("Value has bits set to right of mask."));
    }
    Ok((ip, bits))
}

/// `inet_out` (the mask only when narrower than the address) / `cidr_out` (always).
pub fn format_inet(ip: &IpAddr, bits: u8, cidr: bool) -> String {
    if !cidr && bits == max_bits(ip) { ip.to_string() } else { format!("{ip}/{bits}") }
}

/// `network_cmp`: family, then the common masked prefix, then the mask length, then the
/// whole address.
pub fn compare_inet(a: (&IpAddr, u8), b: (&IpAddr, u8)) -> Ordering {
    let fam = |x: &IpAddr| u8::from(x.is_ipv6());
    let c = fam(a.0).cmp(&fam(b.0));
    if c != Ordering::Equal {
        return c;
    }
    let common = a.1.min(b.1);
    let c = addr_bits(&masked(a.0, common, false)).cmp(&addr_bits(&masked(b.0, common, false)));
    if c != Ordering::Equal {
        return c;
    }
    a.1.cmp(&b.1).then_with(|| addr_bits(a.0).cmp(&addr_bits(b.0)))
}

/// `inet_send`: family (2 = IPv4, 3 = IPv6), mask length, is-cidr, address length, address.
pub fn inet_to_binary(ip: &IpAddr, bits: u8, cidr: bool) -> Vec<u8> {
    let mut out = vec![if ip.is_ipv4() { 2 } else { 3 }, bits, u8::from(cidr)];
    match ip {
        IpAddr::V4(v) => {
            out.push(4);
            out.extend_from_slice(&v.octets());
        }
        IpAddr::V6(v) => {
            out.push(16);
            out.extend_from_slice(&v.octets());
        }
    }
    out
}

pub fn inet_from_binary(b: &[u8]) -> SqlResult<(IpAddr, u8)> {
    let bad = || SqlError::new(code::INVALID_BINARY_REPRESENTATION, "invalid address family in external \"inet\" value");
    let [family, bits, _cidr, n, rest @ ..] = b else { return Err(bad()) };
    let ip = match (family, n) {
        (2, 4) => IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(rest).map_err(|_| bad())?)),
        (3, 16) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(rest).map_err(|_| bad())?)),
        _ => return Err(bad()),
    };
    Ok((ip, *bits))
}

// ---------------------------------------------------------------------------------------------
// macaddr / macaddr8
// ---------------------------------------------------------------------------------------------

/// `macaddr_in` / `macaddr8_in`: hex digit pairs, optionally separated by `:`, `-` or `.`
/// in the usual groupings.
pub fn parse_mac(s: &str, len: usize) -> SqlResult<Vec<u8>> {
    let ty = if len == 6 { "macaddr" } else { "macaddr8" };
    let t = s.trim();
    let digits: String = t.chars().filter(|c| !matches!(c, ':' | '-' | '.')).collect();
    let seps: Vec<char> = t.chars().filter(|c| matches!(c, ':' | '-' | '.')).collect();
    if seps.windows(2).any(|w| w[0] != w[1]) || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(invalid(ty, s));
    }
    let n = digits.len() / 2;
    // macaddr8 also accepts a 6-byte address (FF:FE inserted in the middle).
    if !digits.len().is_multiple_of(2) || !(n == len || (len == 8 && n == 6)) {
        return Err(invalid(ty, s));
    }
    let mut out: Vec<u8> = (0..n).map(|i| u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).unwrap_or(0)).collect();
    if len == 8 && n == 6 {
        out.splice(3..3, [0xff, 0xfe]);
    }
    Ok(out)
}

pub fn format_mac(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}
