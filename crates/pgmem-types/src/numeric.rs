//! `numeric`: arbitrary-precision decimal with Postgres' display-scale rules.
//!
//! A value is `mantissa / 10^scale` where `scale` is Postgres' `dscale` (the number of digits
//! shown after the decimal point), plus NaN. Arithmetic follows `numeric.c`: addition keeps the
//! larger scale, multiplication adds scales, and division picks its result scale with
//! `select_div_scale` and rounds half away from zero.

use crate::{SqlError, SqlResult, code};
use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};
use std::cmp::Ordering;
use std::fmt;

const NUMERIC_MIN_SIG_DIGITS: i64 = 16;
const NUMERIC_MAX_DISPLAY_SCALE: i64 = 1000;
const NUMERIC_MAX_PRECISION: u32 = 1000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Numeric {
    Value { mantissa: BigInt, scale: u32 },
    NaN,
}

fn pow10(n: u32) -> BigInt {
    BigInt::from(10u32).pow(n)
}

impl Numeric {
    pub fn zero() -> Numeric {
        Numeric::Value { mantissa: BigInt::zero(), scale: 0 }
    }

    pub fn from_i64(n: i64) -> Numeric {
        Numeric::Value { mantissa: BigInt::from(n), scale: 0 }
    }

    pub fn from_i128(n: i128) -> Numeric {
        Numeric::Value { mantissa: BigInt::from(n), scale: 0 }
    }

    pub fn is_nan(&self) -> bool {
        matches!(self, Numeric::NaN)
    }

    pub fn scale(&self) -> u32 {
        match self {
            Numeric::Value { scale, .. } => *scale,
            Numeric::NaN => 0,
        }
    }

    /// Parses Postgres numeric input: `123`, `-1.50`, `.5`, `1e3`, `NaN`.
    pub fn parse(input: &str) -> SqlResult<Numeric> {
        let err = || SqlError::invalid_text("numeric", input);
        let s = input.trim();
        if s.eq_ignore_ascii_case("nan") {
            return Ok(Numeric::NaN);
        }
        if s.eq_ignore_ascii_case("infinity") || s.eq_ignore_ascii_case("-infinity") || s.eq_ignore_ascii_case("+infinity") {
            return Err(SqlError::feature_not_supported("numeric infinity is not supported by pgmem"));
        }
        let (neg, body) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let body = body.replace('_', "");
        let (num_part, exp) = match body.find(['e', 'E']) {
            Some(i) => (&body[..i], body[i + 1..].parse::<i64>().map_err(|_| err())?),
            None => (body.as_str(), 0),
        };
        let (int_part, frac_part) = match num_part.split_once('.') {
            Some((i, f)) => (i, f),
            None => (num_part, ""),
        };
        if (int_part.is_empty() && frac_part.is_empty())
            || !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(err());
        }
        let digits = format!("{int_part}{frac_part}");
        let mut mantissa = BigInt::parse_bytes(if digits.is_empty() { b"0" } else { digits.as_bytes() }, 10).ok_or_else(err)?;
        let mut scale = i64::try_from(frac_part.len()).map_err(|_| err())? - exp;
        if scale < 0 {
            mantissa *= pow10(u32::try_from(-scale).map_err(|_| err())?);
            scale = 0;
        }
        if scale > NUMERIC_MAX_DISPLAY_SCALE * 16 {
            return Err(SqlError::out_of_range("numeric"));
        }
        if neg {
            mantissa = -mantissa;
        }
        Ok(Numeric::Value { mantissa, scale: u32::try_from(scale).map_err(|_| err())? })
    }

    /// Converts from f64 the way `float8_numeric` does (via the shortest exact decimal text).
    pub fn from_f64(f: f64) -> SqlResult<Numeric> {
        if f.is_nan() {
            return Ok(Numeric::NaN);
        }
        if f.is_infinite() {
            return Err(SqlError::feature_not_supported("cannot convert infinity to numeric"));
        }
        // Postgres rounds to DBL_DIG (15) significant digits.
        let s = format!("{f:.prec$e}", prec = 14);
        let n = Numeric::parse(&s)?;
        Ok(n.strip_trailing_zeros())
    }

    fn strip_trailing_zeros(self) -> Numeric {
        match self {
            Numeric::Value { mut mantissa, mut scale } => {
                let ten = BigInt::from(10);
                while scale > 0 && (&mantissa % &ten).is_zero() {
                    mantissa /= &ten;
                    scale -= 1;
                }
                Numeric::Value { mantissa, scale }
            }
            nan => nan,
        }
    }

    pub fn to_f64(&self) -> f64 {
        match self {
            Numeric::NaN => f64::NAN,
            Numeric::Value { .. } => self.to_string().parse().unwrap_or(f64::NAN),
        }
    }

    /// Rounds to an integer (half away from zero) and converts, failing on overflow.
    pub fn to_i64(&self) -> Option<i64> {
        match self.round(0) {
            Numeric::Value { mantissa, .. } => mantissa.to_i64(),
            Numeric::NaN => None,
        }
    }

    fn align(&self, other: &Numeric) -> Option<(BigInt, BigInt, u32)> {
        match (self, other) {
            (Numeric::Value { mantissa: a, scale: sa }, Numeric::Value { mantissa: b, scale: sb }) => {
                let s = (*sa).max(*sb);
                Some((a * pow10(s - sa), b * pow10(s - sb), s))
            }
            _ => None,
        }
    }

    pub fn add(&self, other: &Numeric) -> Numeric {
        match self.align(other) {
            Some((a, b, s)) => Numeric::Value { mantissa: a + b, scale: s },
            None => Numeric::NaN,
        }
    }

    pub fn sub(&self, other: &Numeric) -> Numeric {
        match self.align(other) {
            Some((a, b, s)) => Numeric::Value { mantissa: a - b, scale: s },
            None => Numeric::NaN,
        }
    }

    pub fn mul(&self, other: &Numeric) -> Numeric {
        match (self, other) {
            (Numeric::Value { mantissa: a, scale: sa }, Numeric::Value { mantissa: b, scale: sb }) => {
                Numeric::Value { mantissa: a * b, scale: sa + sb }
            }
            _ => Numeric::NaN,
        }
    }

    pub fn neg(&self) -> Numeric {
        match self {
            Numeric::Value { mantissa, scale } => Numeric::Value { mantissa: -mantissa.clone(), scale: *scale },
            Numeric::NaN => Numeric::NaN,
        }
    }

    pub fn abs(&self) -> Numeric {
        match self {
            Numeric::Value { mantissa, scale } => Numeric::Value { mantissa: mantissa.abs(), scale: *scale },
            Numeric::NaN => Numeric::NaN,
        }
    }

    pub fn is_zero(&self) -> bool {
        matches!(self, Numeric::Value { mantissa, .. } if mantissa.is_zero())
    }

    pub fn signum(&self) -> i32 {
        match self {
            Numeric::Value { mantissa, .. } => match mantissa.sign() {
                Sign::Minus => -1,
                Sign::NoSign => 0,
                Sign::Plus => 1,
            },
            Numeric::NaN => 0,
        }
    }

    /// Base-10000 weight and first digit of a nonzero value (Postgres' NumericVar layout).
    fn weight_and_first_digit(&self) -> (i64, i64) {
        let Numeric::Value { mantissa, scale } = self else { return (0, 0) };
        if mantissa.is_zero() {
            return (0, 0);
        }
        let digits = i64::try_from(mantissa.abs().to_string().len()).unwrap_or(0);
        let e10 = digits - 1 - i64::from(*scale);
        let weight = Integer::div_floor(&e10, &4);
        // first digit = floor(|v| / 10000^weight)
        let shift = i64::from(*scale) + 4 * weight;
        let abs = mantissa.abs();
        let first =
            if shift >= 0 { abs / pow10(u32::try_from(shift).unwrap_or(0)) } else { abs * pow10(u32::try_from(-shift).unwrap_or(0)) };
        (weight, first.to_i64().unwrap_or(0))
    }

    /// Postgres' `select_div_scale`.
    fn div_scale(&self, other: &Numeric) -> u32 {
        let (w1, f1) = self.weight_and_first_digit();
        let (w2, f2) = other.weight_and_first_digit();
        let mut qweight = w1 - w2;
        if f1 <= f2 {
            qweight -= 1;
        }
        let rscale = (NUMERIC_MIN_SIG_DIGITS - qweight * 4)
            .max(i64::from(self.scale()))
            .max(i64::from(other.scale()))
            .clamp(0, NUMERIC_MAX_DISPLAY_SCALE);
        u32::try_from(rscale).unwrap_or(0)
    }

    pub fn div(&self, other: &Numeric) -> SqlResult<Numeric> {
        if self.is_nan() || other.is_nan() {
            return Ok(Numeric::NaN);
        }
        if other.is_zero() {
            return Err(SqlError::division_by_zero());
        }
        let rscale = self.div_scale(other);
        Ok(self.div_to_scale(other, rscale))
    }

    /// Exact division rounded half away from zero to `rscale` digits.
    pub fn div_to_scale(&self, other: &Numeric, rscale: u32) -> Numeric {
        let (Numeric::Value { mantissa: a, scale: sa }, Numeric::Value { mantissa: b, scale: sb }) = (self, other) else {
            return Numeric::NaN;
        };
        // result = (a / 10^sa) / (b / 10^sb) = a * 10^(sb + rscale - sa) / b, in units of 10^-rscale
        let exp = i64::from(*sb) + i64::from(rscale) - i64::from(*sa);
        let (num, den) = if exp >= 0 {
            (a * pow10(u32::try_from(exp).unwrap_or(0)), b.clone())
        } else {
            (a.clone(), b * pow10(u32::try_from(-exp).unwrap_or(0)))
        };
        Numeric::Value { mantissa: round_div(&num, &den), scale: rscale }
    }

    /// Truncating integer division (`div(a, b)`).
    pub fn div_trunc(&self, other: &Numeric) -> SqlResult<Numeric> {
        if other.is_zero() {
            return Err(SqlError::division_by_zero());
        }
        match self.align(other) {
            Some((a, b, _)) => Ok(Numeric::Value { mantissa: a / b, scale: 0 }),
            None => Ok(Numeric::NaN),
        }
    }

    /// `mod(a, b)`: sign follows the dividend.
    pub fn rem(&self, other: &Numeric) -> SqlResult<Numeric> {
        if other.is_zero() {
            return Err(SqlError::division_by_zero());
        }
        match self.align(other) {
            Some((a, b, s)) => Ok(Numeric::Value { mantissa: a % b, scale: s }),
            None => Ok(Numeric::NaN),
        }
    }

    /// `round(x, s)` (half away from zero). Negative `s` rounds left of the decimal point.
    pub fn round(&self, s: i32) -> Numeric {
        self.rescale(s, true)
    }

    /// `trunc(x, s)`.
    pub fn trunc(&self, s: i32) -> Numeric {
        self.rescale(s, false)
    }

    fn rescale(&self, s: i32, round: bool) -> Numeric {
        let Numeric::Value { mantissa, scale } = self else { return Numeric::NaN };
        let target = i64::from(s);
        let cur = i64::from(*scale);
        if target >= cur {
            let add = u32::try_from(target - cur).unwrap_or(0);
            return Numeric::Value { mantissa: mantissa * pow10(add), scale: u32::try_from(target.max(0)).unwrap_or(0) };
        }
        let drop = u32::try_from(cur - target).unwrap_or(0);
        let den = pow10(drop);
        let q = if round { round_div(mantissa, &den) } else { mantissa / &den };
        if target >= 0 {
            Numeric::Value { mantissa: q, scale: u32::try_from(target).unwrap_or(0) }
        } else {
            // Negative scale: shift back so the value keeps its magnitude, with scale 0.
            Numeric::Value { mantissa: q * pow10(u32::try_from(-target).unwrap_or(0)), scale: 0 }
        }
    }

    pub fn ceil(&self) -> Numeric {
        let t = self.trunc(0);
        if self.signum() > 0 && t.cmp_num(self) != Ordering::Equal { t.add(&Numeric::from_i64(1)) } else { t }
    }

    pub fn floor(&self) -> Numeric {
        let t = self.trunc(0);
        if self.signum() < 0 && t.cmp_num(self) != Ordering::Equal { t.sub(&Numeric::from_i64(1)) } else { t }
    }

    /// Applies a `numeric(precision, scale)` type modifier, as on assignment to a column.
    pub fn apply_typmod(&self, precision: u32, scale: u32) -> SqlResult<Numeric> {
        if precision > NUMERIC_MAX_PRECISION {
            return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("NUMERIC precision {precision} must be between 1 and 1000")));
        }
        let rounded = self.round(i32::try_from(scale).unwrap_or(0));
        if let Numeric::Value { mantissa, .. } = &rounded
            && !mantissa.is_zero()
        {
            let digits = u32::try_from(mantissa.abs().to_string().len()).unwrap_or(u32::MAX);
            let int_digits = digits.saturating_sub(scale);
            if int_digits > precision - scale {
                let max_int = precision - scale;
                return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow").with_detail(format!(
                    "A field with precision {precision}, scale {scale} must round to an absolute value less than {}.",
                    if max_int == 0 { "1".to_owned() } else { format!("10^{max_int}") }
                )));
            }
        }
        Ok(rounded)
    }

    /// Postgres numeric ordering: NaN is greater than every number and equal to itself.
    pub fn cmp_num(&self, other: &Numeric) -> Ordering {
        match (self, other) {
            (Numeric::NaN, Numeric::NaN) => Ordering::Equal,
            (Numeric::NaN, _) => Ordering::Greater,
            (_, Numeric::NaN) => Ordering::Less,
            _ => match self.align(other) {
                Some((a, b, _)) => a.cmp(&b),
                None => Ordering::Equal,
            },
        }
    }

    /// Normalized form for hashing/grouping: equal values (1.0 and 1.00) share a key.
    pub fn normalized(&self) -> Numeric {
        self.clone().strip_trailing_zeros()
    }

    /// `sqrt(numeric)` with Postgres' result scale rule.
    pub fn sqrt(&self) -> SqlResult<Numeric> {
        if self.is_nan() {
            return Ok(Numeric::NaN);
        }
        if self.signum() < 0 {
            return Err(SqlError::new("2201F", "cannot take square root of a negative number"));
        }
        if self.is_zero() {
            return Ok(Numeric::Value { mantissa: BigInt::zero(), scale: self.scale().max(16) });
        }
        let (weight, _) = self.weight_and_first_digit();
        // sweight = (weight + 1) * DEC_DIGITS / 2 - 1; rscale = NUMERIC_MIN_SIG_DIGITS - sweight
        let sweight = (weight + 1) * 4 / 2 - 1;
        let rscale = u32::try_from((NUMERIC_MIN_SIG_DIGITS - sweight).max(i64::from(self.scale())).clamp(0, NUMERIC_MAX_DISPLAY_SCALE))
            .unwrap_or(16);
        let Numeric::Value { mantissa, scale } = self else { return Ok(Numeric::NaN) };
        // sqrt(m / 10^s) at rscale+1 digits: isqrt(m * 10^(2(rscale+1) - s)) / 10^(rscale+1)
        let work = rscale + 1;
        let exp = i64::from(2 * work) - i64::from(*scale);
        let n =
            if exp >= 0 { mantissa * pow10(u32::try_from(exp).unwrap_or(0)) } else { mantissa / pow10(u32::try_from(-exp).unwrap_or(0)) };
        let root = n.sqrt();
        Ok(Numeric::Value { mantissa: root, scale: work }.round(i32::try_from(rscale).unwrap_or(16)))
    }

    /// Binary wire format (`numeric_send`): base-10000 digits.
    pub fn to_binary(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let Numeric::Value { mantissa, scale } = self else {
            out.extend_from_slice(&0i16.to_be_bytes());
            out.extend_from_slice(&0i16.to_be_bytes());
            out.extend_from_slice(&0xC000u16.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
            return out;
        };
        let neg = mantissa.sign() == Sign::Minus;
        let abs = mantissa.abs().to_string();
        // Split into integer and fraction decimal digits, pad to groups of 4.
        let s = *scale as usize;
        let (int_digits, frac_digits) = if abs.len() > s {
            (abs[..abs.len() - s].to_owned(), abs[abs.len() - s..].to_owned())
        } else {
            ("0".to_owned(), format!("{abs:0>s$}"))
        };
        let int_trim = int_digits.trim_start_matches('0');
        let int_pad = (4 - int_trim.len() % 4) % 4;
        let int_str = format!("{}{int_trim}", "0".repeat(int_pad));
        let frac_pad = (4 - frac_digits.len() % 4) % 4;
        let frac_str = format!("{frac_digits}{}", "0".repeat(frac_pad));
        let mut groups: Vec<i16> = Vec::new();
        for chunk in int_str.as_bytes().chunks(4).chain(frac_str.as_bytes().chunks(4)) {
            groups.push(std::str::from_utf8(chunk).ok().and_then(|c| c.parse().ok()).unwrap_or(0));
        }
        let int_groups = int_str.len() / 4;
        let mut weight = i64::try_from(int_groups).unwrap_or(0) - 1;
        // Strip leading zero groups (adjusting weight) and trailing zero groups.
        let mut start = 0;
        while start < groups.len() && groups[start] == 0 {
            start += 1;
            weight -= 1;
        }
        let mut end = groups.len();
        while end > start && groups[end - 1] == 0 {
            end -= 1;
        }
        let digits = &groups[start..end];
        if digits.is_empty() {
            weight = 0;
        }
        out.extend_from_slice(&i16::try_from(digits.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(&i16::try_from(weight).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(&(if neg { 0x4000u16 } else { 0 }).to_be_bytes());
        out.extend_from_slice(&u16::try_from(*scale).unwrap_or(0).to_be_bytes());
        for d in digits {
            out.extend_from_slice(&d.to_be_bytes());
        }
        out
    }

    /// `numeric_recv`.
    pub fn from_binary(b: &[u8]) -> SqlResult<Numeric> {
        let bad = || SqlError::new(code::INVALID_BINARY_REPRESENTATION, "invalid numeric binary format");
        let rd = |i: usize| -> SqlResult<u16> { Ok(u16::from_be_bytes(b.get(i..i + 2).ok_or_else(bad)?.try_into().map_err(|_| bad())?)) };
        let ndigits = rd(0)? as usize;
        #[allow(clippy::cast_possible_wrap)]
        let weight = rd(2)? as i16;
        let sign = rd(4)?;
        let dscale = rd(6)?;
        if sign == 0xC000 {
            return Ok(Numeric::NaN);
        }
        let mut mantissa = BigInt::zero();
        for i in 0..ndigits {
            mantissa = mantissa * 10000 + BigInt::from(rd(8 + 2 * i)?);
        }
        // value = digits * 10000^(weight - ndigits + 1)
        let exp10 = (i64::from(weight) - i64::try_from(ndigits).unwrap_or(0) + 1) * 4;
        let scale = i64::from(dscale);
        // mantissa_at_scale = value * 10^scale
        let shift = exp10 + scale;
        let mut m = if shift >= 0 {
            mantissa * pow10(u32::try_from(shift).unwrap_or(0))
        } else {
            round_div(&mantissa, &pow10(u32::try_from(-shift).unwrap_or(0)))
        };
        if sign == 0x4000 {
            m = -m;
        }
        Ok(Numeric::Value { mantissa: m, scale: u32::from(dscale) })
    }
}

/// Integer division rounding half away from zero.
fn round_div(num: &BigInt, den: &BigInt) -> BigInt {
    let (q, r) = num.div_rem(den);
    let twice = r.abs() * 2;
    if twice >= den.abs() {
        if (num.sign() == Sign::Minus) != (den.sign() == Sign::Minus) { q - BigInt::one() } else { q + BigInt::one() }
    } else {
        q
    }
}

impl fmt::Display for Numeric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Numeric::NaN => f.write_str("NaN"),
            Numeric::Value { mantissa, scale } => {
                let neg = mantissa.sign() == Sign::Minus;
                let abs = mantissa.abs().to_string();
                let s = *scale as usize;
                let body = if s == 0 {
                    abs
                } else if abs.len() > s {
                    format!("{}.{}", &abs[..abs.len() - s], &abs[abs.len() - s..])
                } else {
                    format!("0.{abs:0>s$}")
                };
                if neg { write!(f, "-{body}") } else { f.write_str(&body) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Numeric {
        Numeric::parse(s).expect("parse")
    }

    #[test]
    fn display_keeps_scale() {
        assert_eq!(n("1.50").to_string(), "1.50");
        assert_eq!(n("-0.05").to_string(), "-0.05");
        assert_eq!(n("1e3").to_string(), "1000");
        assert_eq!(n(".5").to_string(), "0.5");
    }

    #[test]
    fn division_matches_postgres() {
        assert_eq!(n("1").div(&n("3")).expect("div").to_string(), "0.33333333333333333333");
        assert_eq!(n("3").div(&n("2")).expect("div").to_string(), "1.5000000000000000");
        assert_eq!(n("10").div(&n("4")).expect("div").to_string(), "2.5000000000000000");
        assert_eq!(n("2").div(&n("3")).expect("div").to_string(), "0.66666666666666666667");
        assert_eq!(n("1000000").div(&n("3")).expect("div").to_string(), "333333.333333333333");
        assert!(n("1").div(&n("0")).is_err());
    }

    #[test]
    fn arithmetic_scales() {
        assert_eq!(n("1.5").add(&n("2.25")).to_string(), "3.75");
        assert_eq!(n("1.5").mul(&n("2.25")).to_string(), "3.375");
        assert_eq!(n("2.5").round(0).to_string(), "3");
        assert_eq!(n("-2.5").round(0).to_string(), "-3");
        assert_eq!(n("1234.5678").round(-2).to_string(), "1200");
        assert_eq!(n("1.239").trunc(2).to_string(), "1.23");
        assert_eq!(n("2").sqrt().expect("sqrt").to_string(), "1.414213562373095");
    }

    #[test]
    fn typmod() {
        assert_eq!(n("3.14159").apply_typmod(5, 2).expect("ok").to_string(), "3.14");
        assert_eq!(n("1000").apply_typmod(5, 2).expect_err("overflow").code, "22003");
    }

    #[test]
    fn binary_round_trip() {
        for s in ["0", "1", "-1.50", "12345.6789", "0.0001", "100000000", "3.14159265358979", "NaN", "0.000"] {
            let v = n(s);
            let back = Numeric::from_binary(&v.to_binary()).expect("decode");
            assert_eq!(back.to_string(), v.to_string(), "{s}");
        }
    }
}
