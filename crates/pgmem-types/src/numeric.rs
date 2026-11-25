//! `numeric`: arbitrary-precision decimal with Postgres' display-scale rules.
//!
//! A value is `mantissa / 10^scale` where `scale` is Postgres' `dscale` (the number of digits
//! shown after the decimal point), plus NaN and ±Infinity (Postgres 14+). Arithmetic follows `numeric.c`: addition keeps the
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
/// `numeric.c`'s literal for log10(e): Postgres' scale estimates truncate products with it, so
/// the slightly shorter constant is kept rather than `f64::consts::LOG10_E`.
#[allow(clippy::approx_constant)]
const PG_LOG10_E: f64 = 0.434_294_481_903_252;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Numeric {
    Value { mantissa: BigInt, scale: u32 },
    NaN,
    PosInf,
    NegInf,
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

    pub fn is_infinite(&self) -> bool {
        matches!(self, Numeric::PosInf | Numeric::NegInf)
    }

    /// A finite value (neither NaN nor infinite).
    pub fn is_finite(&self) -> bool {
        matches!(self, Numeric::Value { .. })
    }

    fn inf(negative: bool) -> Numeric {
        if negative { Numeric::NegInf } else { Numeric::PosInf }
    }

    pub fn scale(&self) -> u32 {
        match self {
            Numeric::Value { scale, .. } => *scale,
            _ => 0,
        }
    }

    /// Parses Postgres numeric input: `123`, `-1.50`, `.5`, `1e3`, `NaN`.
    pub fn parse(input: &str) -> SqlResult<Numeric> {
        let err = || SqlError::invalid_text("numeric", input);
        let s = input.trim();
        if s.eq_ignore_ascii_case("nan") {
            return Ok(Numeric::NaN);
        }
        match s.to_ascii_lowercase().as_str() {
            "infinity" | "+infinity" | "inf" | "+inf" => return Ok(Numeric::PosInf),
            "-infinity" | "-inf" => return Ok(Numeric::NegInf),
            _ => {}
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
            return Ok(Numeric::inf(f < 0.0));
        }
        // Postgres rounds to DBL_DIG (15) significant digits.
        let s = format!("{f:.prec$e}", prec = 14);
        let n = Numeric::parse(&s)?;
        Ok(n.strip_trailing_zeros())
    }

    /// `trim_scale`: the value with trailing fractional zeros removed.
    pub fn trim_scale(&self) -> Numeric {
        self.clone().strip_trailing_zeros()
    }

    /// `min_scale`: the smallest scale that represents the value exactly.
    pub fn min_scale(&self) -> u32 {
        self.trim_scale().scale()
    }

    /// `gcd(numeric, numeric)`: computed on both values at their common scale.
    pub fn gcd(&self, other: &Numeric) -> Numeric {
        match self.align(other) {
            Some((a, b, s)) => Numeric::Value { mantissa: a.gcd(&b), scale: s },
            None => Numeric::NaN,
        }
    }

    /// `lcm(numeric, numeric)`.
    pub fn lcm(&self, other: &Numeric) -> Numeric {
        match self.align(other) {
            Some((a, b, s)) if a.is_zero() || b.is_zero() => Numeric::Value { mantissa: BigInt::zero(), scale: s },
            Some((a, b, s)) => Numeric::Value { mantissa: a.lcm(&b), scale: s },
            None => Numeric::NaN,
        }
    }

    /// `factorial(n)`.
    pub fn factorial(n: i64) -> SqlResult<Numeric> {
        if n < 0 {
            return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "factorial of a negative number is undefined"));
        }
        // 32177! is the largest factorial below numeric's 131072-digit limit.
        if n > 32177 {
            return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format"));
        }
        let mut acc = BigInt::one();
        for i in 2..=n {
            acc *= i;
        }
        Ok(Numeric::Value { mantissa: acc, scale: 0 })
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
            Numeric::PosInf => f64::INFINITY,
            Numeric::NegInf => f64::NEG_INFINITY,
            Numeric::Value { .. } => self.to_string().parse().unwrap_or(f64::NAN),
        }
    }

    /// Rounds to an integer (half away from zero) and converts, failing on overflow.
    pub fn to_i64(&self) -> Option<i64> {
        match self.round(0) {
            Numeric::Value { mantissa, .. } => mantissa.to_i64(),
            _ => None,
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
        use Numeric::{NaN, NegInf, PosInf};
        match (self, other) {
            (NaN, _) | (_, NaN) | (PosInf, NegInf) | (NegInf, PosInf) => NaN,
            (PosInf, _) | (_, PosInf) => PosInf,
            (NegInf, _) | (_, NegInf) => NegInf,
            _ => match self.align(other) {
                Some((a, b, s)) => Numeric::Value { mantissa: a + b, scale: s },
                None => NaN,
            },
        }
    }

    pub fn sub(&self, other: &Numeric) -> Numeric {
        self.add(&other.neg())
    }

    pub fn mul(&self, other: &Numeric) -> Numeric {
        match (self, other) {
            (Numeric::Value { mantissa: a, scale: sa }, Numeric::Value { mantissa: b, scale: sb }) => {
                Numeric::Value { mantissa: a * b, scale: sa + sb }
            }
            (Numeric::NaN, _) | (_, Numeric::NaN) => Numeric::NaN,
            // Infinity times zero is NaN; otherwise the signs multiply.
            _ if self.signum() == 0 || other.signum() == 0 => Numeric::NaN,
            _ => Numeric::inf(self.signum() * other.signum() < 0),
        }
    }

    pub fn neg(&self) -> Numeric {
        match self {
            Numeric::Value { mantissa, scale } => Numeric::Value { mantissa: -mantissa.clone(), scale: *scale },
            Numeric::NaN => Numeric::NaN,
            Numeric::PosInf => Numeric::NegInf,
            Numeric::NegInf => Numeric::PosInf,
        }
    }

    pub fn abs(&self) -> Numeric {
        match self {
            Numeric::Value { mantissa, scale } => Numeric::Value { mantissa: mantissa.abs(), scale: *scale },
            Numeric::NegInf => Numeric::PosInf,
            other => other.clone(),
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
            Numeric::PosInf => 1,
            Numeric::NegInf => -1,
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
        if let Some(v) = self.div_special(other) {
            return Ok(v);
        }
        let rscale = self.div_scale(other);
        Ok(self.div_to_scale(other, rscale))
    }

    /// Division involving an infinity (the divisor is known to be nonzero and neither is NaN):
    /// inf/inf is NaN, inf/x is a signed infinity, x/inf is zero.
    fn div_special(&self, other: &Numeric) -> Option<Numeric> {
        match (self.is_infinite(), other.is_infinite()) {
            (true, true) => Some(Numeric::NaN),
            (true, false) => Some(Numeric::inf(self.signum() * other.signum() < 0)),
            (false, true) => Some(Numeric::zero()),
            (false, false) => None,
        }
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
        if self.is_nan() || other.is_nan() {
            return Ok(Numeric::NaN);
        }
        if other.is_zero() {
            return Err(SqlError::division_by_zero());
        }
        if let Some(v) = self.div_special(other) {
            return Ok(v);
        }
        match self.align(other) {
            Some((a, b, _)) => Ok(Numeric::Value { mantissa: a / b, scale: 0 }),
            None => Ok(Numeric::NaN),
        }
    }

    /// `mod(a, b)`: sign follows the dividend.
    pub fn rem(&self, other: &Numeric) -> SqlResult<Numeric> {
        if self.is_nan() || other.is_nan() {
            return Ok(Numeric::NaN);
        }
        if other.is_zero() {
            return Err(SqlError::division_by_zero());
        }
        // mod(inf, x) is NaN; mod(x, inf) is x.
        if self.is_infinite() {
            return Ok(Numeric::NaN);
        }
        if other.is_infinite() {
            return Ok(self.clone());
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
        let Numeric::Value { mantissa, scale } = self else { return self.clone() };
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

    /// Applies a `numeric(precision, scale)` type modifier, as on assignment to a column. The
    /// scale may be negative (rounding left of the decimal point) or exceed the precision.
    pub fn apply_typmod(&self, precision: i32, scale: i32) -> SqlResult<Numeric> {
        if !(1..=NUMERIC_MAX_PRECISION as i32).contains(&precision) {
            return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("NUMERIC precision {precision} must be between 1 and 1000")));
        }
        if self.is_infinite() {
            return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow")
                .with_detail(format!("A field with precision {precision}, scale {scale} cannot hold an infinite value.")));
        }
        let rounded = self.round(scale);
        if let Numeric::Value { mantissa, scale: shown } = &rounded
            && !mantissa.is_zero()
        {
            // Digits left of the decimal point (a negative scale leaves `shown` at 0).
            let digits = i64::try_from(mantissa.abs().to_string().len()).unwrap_or(i64::MAX);
            let int_digits = digits - i64::from(*shown);
            let max_int = i64::from(precision) - i64::from(scale);
            if int_digits > max_int {
                return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "numeric field overflow").with_detail(format!(
                    "A field with precision {precision}, scale {scale} must round to an absolute value less than {}.",
                    if max_int == 0 { "1".to_owned() } else { format!("10^{max_int}") }
                )));
            }
        }
        Ok(rounded)
    }

    /// Postgres numeric ordering: -Infinity < numbers < Infinity < NaN (NaN equals itself).
    pub fn cmp_num(&self, other: &Numeric) -> Ordering {
        fn rank(n: &Numeric) -> u8 {
            match n {
                Numeric::NegInf => 0,
                Numeric::Value { .. } => 1,
                Numeric::PosInf => 2,
                Numeric::NaN => 3,
            }
        }
        match self.align(other) {
            Some((a, b, _)) => a.cmp(&b),
            None => rank(self).cmp(&rank(other)),
        }
    }

    /// Normalized form for hashing/grouping: equal values (1.0 and 1.00) share a key.
    pub fn normalized(&self) -> Numeric {
        self.clone().strip_trailing_zeros()
    }

    /// `sqrt(numeric)` with Postgres' result scale rule.
    pub fn sqrt(&self) -> SqlResult<Numeric> {
        if self.is_nan() || *self == Numeric::PosInf {
            return Ok(self.clone());
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
        self.sqrt_to_scale(rscale)
    }

    /// The square root rounded to `rscale` digits.
    pub fn sqrt_to_scale(&self, rscale: u32) -> SqlResult<Numeric> {
        if self.is_nan() || *self == Numeric::PosInf {
            return Ok(self.clone());
        }
        if self.signum() < 0 {
            return Err(SqlError::new("2201F", "cannot take square root of a negative number"));
        }
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
            let sign: u16 = match self {
                Numeric::PosInf => 0xD000,
                Numeric::NegInf => 0xF000,
                _ => 0xC000,
            };
            out.extend_from_slice(&0i16.to_be_bytes());
            out.extend_from_slice(&0i16.to_be_bytes());
            out.extend_from_slice(&sign.to_be_bytes());
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
        match sign {
            0xC000 => return Ok(Numeric::NaN),
            0xD000 => return Ok(Numeric::PosInf),
            0xF000 => return Ok(Numeric::NegInf),
            _ => {}
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

/// Transcendental functions (`exp`, `ln`, `log`, `power`) with `numeric.c`'s result-scale
/// rules. The values are computed in exact fixed-point arithmetic with guard digits and then
/// rounded, so they agree with Postgres digit for digit.
impl Numeric {
    /// `log10(|self|)`, approximately (from the leading digits), for scale estimates.
    fn approx_log10(&self) -> f64 {
        let Numeric::Value { mantissa, scale } = self else { return 0.0 };
        let digits = mantissa.abs().to_string();
        let lead: f64 = format!("0.{}", &digits[..digits.len().min(17)]).parse().unwrap_or(0.1);
        #[allow(clippy::cast_precision_loss)]
        let exp10 = (digits.len() as f64) - f64::from(*scale);
        lead.log10() + exp10
    }

    /// `estimate_ln_dweight`: the decimal weight of `ln(self)`.
    fn estimate_ln_dweight(&self) -> i64 {
        if self.signum() <= 0 {
            return 0;
        }
        let lo = Numeric::Value { mantissa: BigInt::from(9), scale: 1 };
        let hi = Numeric::Value { mantissa: BigInt::from(11), scale: 1 };
        if self.cmp_num(&lo).is_ge() && self.cmp_num(&hi).is_le() {
            let x = self.sub(&Numeric::from_i64(1));
            if x.is_zero() {
                return 0;
            }
            #[allow(clippy::cast_possible_truncation)]
            return x.approx_log10().floor() as i64;
        }
        let ln_var = self.approx_log10() * std::f64::consts::LN_10;
        #[allow(clippy::cast_possible_truncation)]
        let w = ln_var.abs().log10() as i64;
        w
    }

    /// The value times 10^p, rounded.
    fn fixed(&self, p: u32) -> BigInt {
        match self.rescale(i32::try_from(p).unwrap_or(i32::MAX), true) {
            Numeric::Value { mantissa, .. } => mantissa,
            _ => BigInt::zero(),
        }
    }

    /// `exp(numeric)`.
    pub fn exp(&self) -> SqlResult<Numeric> {
        match self {
            Numeric::NaN | Numeric::PosInf => return Ok(self.clone()),
            Numeric::NegInf => return Ok(Numeric::zero()),
            Numeric::Value { .. } => {}
        }
        let x = self.to_f64();
        #[allow(clippy::cast_possible_truncation)]
        let val = (x * PG_LOG10_E).clamp(-2000.0, 2000.0) as i64;
        let rscale = clamp_scale((NUMERIC_MIN_SIG_DIGITS - val).max(i64::from(self.scale())));
        exp_to_scale(self, rscale)
    }

    /// `ln(numeric)`.
    pub fn ln(&self) -> SqlResult<Numeric> {
        ln_domain(self)?;
        if !self.is_finite() {
            return Ok(self.clone());
        }
        let rscale = clamp_scale((NUMERIC_MIN_SIG_DIGITS - self.estimate_ln_dweight()).max(i64::from(self.scale())));
        Ok(ln_to_scale(self, rscale))
    }

    /// `log(base, numeric)`.
    pub fn log(base: &Numeric, x: &Numeric) -> SqlResult<Numeric> {
        ln_domain(base)?;
        ln_domain(x)?;
        if base.is_nan() || x.is_nan() {
            return Ok(Numeric::NaN);
        }
        if !base.is_finite() || !x.is_finite() {
            return match (base.is_finite(), x.is_finite()) {
                (false, false) => Ok(Numeric::NaN),
                (true, false) if base.cmp_num(&Numeric::from_i64(1)).is_eq() => {
                    Err(SqlError::new(code::DIVISION_BY_ZERO, "division by zero"))
                }
                (true, false) => Ok(if base.cmp_num(&Numeric::from_i64(1)).is_lt() { Numeric::NegInf } else { Numeric::PosInf }),
                _ => Ok(Numeric::Value { mantissa: BigInt::zero(), scale: 0 }),
            };
        }
        let (bw, xw) = (base.estimate_ln_dweight(), x.estimate_ln_dweight());
        let result_dweight = xw - bw;
        let rscale = clamp_scale((NUMERIC_MIN_SIG_DIGITS - result_dweight).max(i64::from(base.scale())).max(i64::from(x.scale())));
        let p = rscale + u32::try_from((result_dweight + 10).max(10)).unwrap_or(10) + 10;
        let lb = ln_to_scale(base, p);
        let lx = ln_to_scale(x, p);
        if lb.is_zero() {
            return Err(SqlError::new(code::DIVISION_BY_ZERO, "division by zero"));
        }
        Ok(lx.div_to_scale(&lb, rscale))
    }

    /// `power(numeric, numeric)` (`power_var` / `power_var_int`).
    pub fn power(&self, exp: &Numeric) -> SqlResult<Numeric> {
        let one = Numeric::from_i64(1);
        if self.is_nan() || exp.is_nan() {
            // 1 ^ NaN and NaN ^ 0 are 1.
            if self.cmp_num(&one).is_eq() && !self.is_nan() || exp.is_zero() && !exp.is_nan() {
                return Ok(one.rescale(0, true));
            }
            return Ok(Numeric::NaN);
        }
        if self.is_zero() && exp.signum() < 0 {
            return Err(SqlError::new("2201F", "zero raised to a negative power is undefined"));
        }
        let int_exp = exp.is_finite() && exp.trunc(0).cmp_num(exp).is_eq();
        if self.signum() < 0 && !int_exp && exp.is_finite() {
            return Err(SqlError::new("2201F", "a negative number raised to a non-integer power yields a complex result"));
        }
        if !self.is_finite() || !exp.is_finite() {
            return Ok(power_infinite(self, exp));
        }
        if int_exp && let Some(n) = exp.to_i64().and_then(|n| i32::try_from(n).ok()) {
            return power_int(self, n, exp.scale());
        }
        if self.is_zero() {
            return Ok(Numeric::Value { mantissa: BigInt::zero(), scale: 16 });
        }
        // result = exp(exp * ln(base)); estimate its weight at low precision first.
        let ln_dweight = self.estimate_ln_dweight();
        let local = u32::try_from((8 - ln_dweight).max(0)).unwrap_or(0);
        let approx = ln_to_scale(self, local).mul(exp);
        let val = approx.to_f64();
        if val.abs() > 2000.0 * 3.01 {
            if val > 0.0 {
                return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format"));
            }
            return Ok(Numeric::Value { mantissa: BigInt::zero(), scale: 1000 });
        }
        #[allow(clippy::cast_possible_truncation)]
        let val = (val * PG_LOG10_E) as i64;
        let rscale = clamp_scale((NUMERIC_MIN_SIG_DIGITS - val).max(i64::from(self.scale())).max(i64::from(exp.scale())));
        let sig = (i64::from(rscale) + val).max(0);
        let ln_scale = u32::try_from((sig - ln_dweight + 8).max(0)).unwrap_or(0) + 8;
        let product = ln_to_scale(self, ln_scale).mul(exp);
        exp_to_scale(&product, rscale)
    }
}

fn clamp_scale(s: i64) -> u32 {
    u32::try_from(s.clamp(0, NUMERIC_MAX_DISPLAY_SCALE)).unwrap_or(16)
}

fn ln_domain(x: &Numeric) -> SqlResult<()> {
    if x.is_zero() {
        return Err(SqlError::new("2201E", "cannot take logarithm of zero"));
    }
    if x.signum() < 0 || *x == Numeric::NegInf {
        return Err(SqlError::new("2201E", "cannot take logarithm of a negative number"));
    }
    Ok(())
}

/// `exp(x)` rounded to `rscale` digits.
fn exp_to_scale(x: &Numeric, rscale: u32) -> SqlResult<Numeric> {
    let xf = x.to_f64();
    if xf.abs() >= 6000.0 {
        if xf > 0.0 {
            return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format"));
        }
        return Ok(Numeric::Value { mantissa: BigInt::zero(), scale: rscale });
    }
    #[allow(clippy::cast_possible_truncation)]
    let dweight = (xf.abs() * PG_LOG10_E) as u32 + 1;
    let neg = x.signum() < 0;
    let ax = x.abs();
    // Relative precision for the result: rscale digits after the point of a value with up to
    // `dweight` digits before it (or, for exp of a negative number, that many leading zeros).
    let p = rscale + dweight + 20;
    let e = exp_fixed(&ax.fixed(p), p);
    let r = if neg {
        Numeric::from_i64(1).div_to_scale(&Numeric::Value { mantissa: e, scale: p }, rscale)
    } else {
        Numeric::Value { mantissa: e, scale: p }.rescale(i32::try_from(rscale).unwrap_or(0), true)
    };
    Ok(r)
}

/// `exp` of a non-negative fixed-point value `x / 10^p`, as a fixed-point value.
fn exp_fixed(x: &BigInt, p: u32) -> BigInt {
    let one = pow10(p);
    let half = &one / 2;
    let mut y = x.clone();
    let mut k = 0;
    while y.abs() > half {
        y /= 2;
        k += 1;
    }
    let mut sum = one.clone();
    let mut term = one.clone();
    let mut n = 1u32;
    loop {
        term = term * &y / (&one * n);
        if term.is_zero() {
            break;
        }
        sum += &term;
        n += 1;
    }
    for _ in 0..k {
        sum = &sum * &sum / &one;
    }
    sum
}

/// `ln(x)` (x > 0, finite) rounded to `rscale` digits.
fn ln_to_scale(x: &Numeric, rscale: u32) -> Numeric {
    // x = m * 10^e with m in [1, 10): ln x = ln m + e ln 10.
    #[allow(clippy::cast_possible_truncation)]
    let e = x.approx_log10().floor() as i64;
    let p = rscale + 20 + u32::try_from(e.unsigned_abs().to_string().len()).unwrap_or(0);
    let m = match x {
        Numeric::Value { mantissa, scale } => {
            let s = i64::from(*scale) + e;
            // m = mantissa / 10^s, fixed at p digits.
            let shift = i64::from(p) - s;
            if shift >= 0 {
                mantissa * pow10(u32::try_from(shift).unwrap_or(0))
            } else {
                round_div(mantissa, &pow10(u32::try_from(-shift).unwrap_or(0)))
            }
        }
        _ => BigInt::zero(),
    };
    let mut l = ln_fixed(&m, p);
    if e != 0 {
        l += ln_fixed(&(pow10(p) * 10), p) * e;
    }
    Numeric::Value { mantissa: l, scale: p }.rescale(i32::try_from(rscale).unwrap_or(0), true)
}

/// `ln` of a positive fixed-point value `x / 10^p` near [0.1, 10].
fn ln_fixed(x: &BigInt, p: u32) -> BigInt {
    let one = pow10(p);
    let lo = &one * 9 / 10;
    let hi = &one * 11 / 10;
    let mut y = x.clone();
    let mut k = 0u32;
    while y < lo || y > hi {
        y = (&y * &one).sqrt();
        k += 1;
    }
    // ln y = 2 atanh((y - 1) / (y + 1))
    let z = (&y - &one) * &one / (&y + &one);
    let z2 = &z * &z / &one;
    let mut sum = z.clone();
    let mut term = z;
    let mut n = 1u32;
    loop {
        term = term * &z2 / &one;
        n += 2;
        let t = &term / n;
        if t.is_zero() {
            break;
        }
        sum += t;
    }
    sum * 2 * BigInt::from(2).pow(k)
}

/// `power_var_int`: an integer power, exact then rounded to Postgres' result scale.
fn power_int(base: &Numeric, n: i32, exp_dscale: u32) -> SqlResult<Numeric> {
    let f = if base.is_zero() { 0.0 } else { f64::from(n) * base.approx_log10_pg() };
    if f > 131_072.0 * 4.0 * 1.01 {
        return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value overflows numeric format"));
    }
    if f + 1.0 < -1000.0 * 1.01 {
        return Ok(Numeric::Value { mantissa: BigInt::zero(), scale: 1000 });
    }
    #[allow(clippy::cast_possible_truncation)]
    let rscale = clamp_scale((NUMERIC_MIN_SIG_DIGITS - f as i64).max(i64::from(base.scale())).max(i64::from(exp_dscale)));
    let rs = i32::try_from(rscale).unwrap_or(0);
    if n == 0 {
        return Ok(Numeric::from_i64(1).rescale(rs, true));
    }
    let Numeric::Value { mantissa, scale } = base else { return Ok(Numeric::NaN) };
    let abs_n = n.unsigned_abs();
    let pow = Numeric::Value { mantissa: mantissa.pow(abs_n), scale: scale * abs_n };
    if n > 0 { Ok(pow.rescale(rs, true)) } else { Ok(Numeric::from_i64(1).div_to_scale(&pow, rscale)) }
}

impl Numeric {
    /// `power_var_int`'s weight estimate: `log10(f) + p` from up to 16 leading digits.
    fn approx_log10_pg(&self) -> f64 {
        self.abs().approx_log10()
    }
}

/// `power` with an infinite operand (Postgres 14 rules).
fn power_infinite(base: &Numeric, exp: &Numeric) -> Numeric {
    let one = Numeric::from_i64(1);
    let zero = Numeric::zero();
    if exp.is_zero() || base.cmp_num(&one).is_eq() {
        return one;
    }
    match exp {
        Numeric::PosInf | Numeric::NegInf => {
            let mag = base.abs().cmp_num(&one);
            let pos = *exp == Numeric::PosInf;
            if mag.is_eq() {
                return one;
            }
            if (mag.is_gt()) == pos { Numeric::PosInf } else { zero }
        }
        _ => {
            // base is ±Infinity, exp finite and nonzero.
            if exp.signum() < 0 {
                return zero;
            }
            let odd = exp.trunc(0).cmp_num(exp).is_eq() && exp.rem(&Numeric::from_i64(2)).is_ok_and(|r| !r.is_zero());
            if *base == Numeric::NegInf && odd { Numeric::NegInf } else { Numeric::PosInf }
        }
    }
}

impl fmt::Display for Numeric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Numeric::NaN => f.write_str("NaN"),
            Numeric::PosInf => f.write_str("Infinity"),
            Numeric::NegInf => f.write_str("-Infinity"),
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
        assert_eq!(n("12345").apply_typmod(5, -2).expect("ok").to_string(), "12300");
        assert_eq!(n("0.0123").apply_typmod(3, 4).expect("ok").to_string(), "0.0123");
        assert_eq!(n("0.123").apply_typmod(3, 4).expect_err("overflow").code, "22003");
        assert_eq!(n("Infinity").apply_typmod(5, 2).expect_err("overflow").code, "22003");
    }

    #[test]
    fn infinities() {
        assert_eq!(n("inf").add(&n("1")).to_string(), "Infinity");
        assert_eq!(n("inf").add(&n("-inf")).to_string(), "NaN");
        assert_eq!(n("-inf").mul(&n("-2")).to_string(), "Infinity");
        assert_eq!(n("inf").mul(&n("0")).to_string(), "NaN");
        assert_eq!(n("1").div(&n("inf")).expect("div").to_string(), "0");
        assert_eq!(n("inf").cmp_num(&n("1e100")), Ordering::Greater);
        assert_eq!(n("NaN").cmp_num(&n("inf")), Ordering::Greater);
        assert_eq!(n("-inf").cmp_num(&n("-1e100")), Ordering::Less);
    }

    #[test]
    fn binary_round_trip() {
        for s in ["0", "1", "-1.50", "12345.6789", "0.0001", "100000000", "3.14159265358979", "NaN", "0.000", "Infinity", "-Infinity"] {
            let v = n(s);
            let back = Numeric::from_binary(&v.to_binary()).expect("decode");
            assert_eq!(back.to_string(), v.to_string(), "{s}");
        }
    }
}
