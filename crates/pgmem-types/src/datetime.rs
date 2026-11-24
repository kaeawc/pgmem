//! Date/time support: `timestamptz` (microseconds since the Postgres epoch, 2000-01-01 UTC) and
//! `interval` (Postgres' months / days / microseconds triple). The session time zone is always
//! UTC, which is what pgmem reports in the `TimeZone` startup parameter.

use crate::calendar::{self, Tm};
use crate::{SqlError, SqlResult, code};
use std::cmp::Ordering;
use std::fmt::Write as _;

pub const USECS_PER_SEC: i64 = 1_000_000;
pub const USECS_PER_MINUTE: i64 = 60 * USECS_PER_SEC;
pub const USECS_PER_HOUR: i64 = 60 * USECS_PER_MINUTE;
pub const USECS_PER_DAY: i64 = 24 * USECS_PER_HOUR;
/// Seconds between 1970-01-01 and 2000-01-01.
pub const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;
/// `infinity` / `-infinity` timestamps (Postgres' DT_NOEND / DT_NOBEGIN).
pub const TS_INFINITY: i64 = i64::MAX;
pub const TS_NEG_INFINITY: i64 = i64::MIN;
/// Postgres' timestamp range: 4713 BC to 294276 AD.
pub const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
pub const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;

/// The broken-down UTC time of a finite timestamp.
pub fn to_tm(micros: i64) -> Option<Tm> {
    if micros == TS_INFINITY || micros == TS_NEG_INFINITY {
        return None;
    }
    Some(Tm::from_micros(micros))
}

/// Microseconds for a broken-down time, failing outside Postgres' timestamp range.
pub fn from_tm(tm: &Tm) -> SqlResult<i64> {
    let t = tm.to_micros();
    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&t) {
        return Err(SqlError::new(code::DATETIME_FIELD_OVERFLOW, "timestamp out of range"));
    }
    Ok(t)
}

/// Formats a timestamptz the way Postgres does with `DateStyle = ISO`, in the session's time
/// zone: `2024-07-15 08:00:00-04`, with the fraction trimmed of trailing zeros and the offset
/// minutes / seconds shown only when nonzero.
pub fn format_timestamptz(micros: i64) -> String {
    if micros == TS_INFINITY || micros == TS_NEG_INFINITY {
        return if micros == TS_NEG_INFINITY { "-infinity".into() } else { "infinity".into() };
    }
    let (local, offset) = crate::context::with_zone(|z| (z.local_of_utc(micros), z.offset_at(micros)));
    let mut out = format_local(local);
    let suffix = crate::zone::format_offset(offset);
    match out.strip_suffix(" BC") {
        Some(base) => out = format!("{base}{suffix} BC"),
        None => out.push_str(&suffix),
    }
    out
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff][ BC]` of a local (zone-less) timestamp.
fn format_local(micros: i64) -> String {
    let tm = Tm::from_micros(micros);
    let mut out = String::with_capacity(32);
    let (y, bc) = if tm.year <= 0 { (1 - tm.year, true) } else { (tm.year, false) };
    let _ = write!(out, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, tm.month, tm.day, tm.hour, tm.minute, tm.second);
    push_fraction(&mut out, i64::from(tm.micros));
    if bc {
        out.push_str(" BC");
    }
    out
}

fn push_fraction(out: &mut String, micros: i64) {
    if micros != 0 {
        let frac = format!("{micros:06}");
        out.push('.');
        out.push_str(frac.trim_end_matches('0'));
    }
}

/// Parses `YYYY-MM-DD` (year may have more than 4 digits) into (year, month, day).
fn parse_ymd(s: &str) -> Option<(i32, u32, u32)> {
    let mut parts = s.splitn(3, '-');
    let y: i32 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    calendar::valid_date(y, m, d).then_some((y, m, d))
}

/// Parses timestamptz input. Accepts the ISO forms Postgres accepts in practice: a date, an
/// optional `T` or space, a time with optional fraction, and an optional zone (`Z`, `UTC`,
/// `+HH`, `+HH:MM`, `+HHMM`). A missing zone means the session zone (UTC). Also `epoch`,
/// `infinity`, `-infinity`.
pub fn parse_timestamptz(input: &str) -> SqlResult<i64> {
    let s = input.trim();
    let err =
        || SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type timestamp with time zone: \"{input}\""));
    match s.to_ascii_lowercase().as_str() {
        "infinity" | "+infinity" => return Ok(TS_INFINITY),
        "-infinity" => return Ok(TS_NEG_INFINITY),
        "epoch" => return Ok(-PG_EPOCH_UNIX_SECS * USECS_PER_SEC),
        _ => {}
    }
    let (body, bc) = match s.strip_suffix(" BC").or_else(|| s.strip_suffix(" bc")) {
        Some(b) => (b.trim(), true),
        None => (s, false),
    };
    // The date runs up to the first space or 'T' after the day.
    let date_end = body.find([' ', 'T', 't']).unwrap_or(body.len());
    let (y, m, d) = parse_ymd(&body[..date_end]).ok_or_else(err)?;
    let year = if bc { 1 - y } else { y };
    let midnight = calendar::days_from_ymd(year, m, d) * USECS_PER_DAY;
    let rest = body[date_end..].trim_start_matches(['T', 't', ' ']);
    if rest.is_empty() {
        return Ok(midnight);
    }
    // Split the time from the zone: the zone starts at the first '+', '-', 'Z' or alphabetic
    // character after the time digits.
    let zone_start = rest.find(|c: char| c == '+' || c == '-' || c.is_ascii_alphabetic()).unwrap_or(rest.len());
    let (time_part, zone_part) = rest.split_at(zone_start);
    let time = parse_time_of_day(time_part.trim()).ok_or_else(err)?;
    let offset_secs = parse_zone(zone_part.trim()).ok_or_else(err)?;
    let t = midnight + time - offset_secs * USECS_PER_SEC;
    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&t) {
        return Err(SqlError::new(code::DATETIME_FIELD_OVERFLOW, format!("timestamp out of range: \"{input}\"")));
    }
    Ok(t)
}

/// Parses `HH:MM[:SS[.ffffff]]` into microseconds since midnight. `24:00:00` is allowed.
pub fn parse_time_of_day(s: &str) -> Option<i64> {
    let mut parts = s.split(':');
    let h: i64 = parts.next()?.trim().parse().ok()?;
    let m: i64 = parts.next()?.trim().parse().ok()?;
    let sec_part = parts.next().unwrap_or("0");
    if parts.next().is_some() {
        return None;
    }
    let (secs, frac) = split_seconds(sec_part)?;
    if !(0..=24).contains(&h) || !(0..60).contains(&m) || !(0..=60).contains(&secs) {
        return None;
    }
    let total = h * USECS_PER_HOUR + m * USECS_PER_MINUTE + secs * USECS_PER_SEC + frac;
    if total > USECS_PER_DAY {
        return None;
    }
    Some(total)
}

/// Splits `SS[.ffffff]` into whole seconds and microseconds (rounded to 6 digits).
fn split_seconds(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    match s.split_once('.') {
        None => Some((s.parse().ok()?, 0)),
        Some((whole, frac)) => {
            let secs: i64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((secs, fraction_to_micros(frac)))
        }
    }
}

/// Converts the digits after a decimal point to microseconds, rounding half up at 6 digits.
fn fraction_to_micros(frac: &str) -> i64 {
    let mut digits: Vec<u8> = frac.bytes().map(|b| b - b'0').collect();
    let round_up = digits.len() > 6 && digits[6] >= 5;
    digits.resize(6, 0);
    let mut v = digits.iter().fold(0i64, |acc, d| acc * 10 + i64::from(*d));
    if round_up {
        v += 1;
    }
    v
}

/// Parses a zone suffix to an offset east of UTC, in seconds. Empty = UTC (session zone).
fn parse_zone(z: &str) -> Option<i64> {
    if z.is_empty() || z.eq_ignore_ascii_case("z") || z.eq_ignore_ascii_case("utc") || z.eq_ignore_ascii_case("gmt") {
        return Some(0);
    }
    let (sign, digits) = match z.as_bytes().first()? {
        b'+' => (1, &z[1..]),
        b'-' => (-1, &z[1..]),
        _ => return None,
    };
    let (h, m, s) = if let Some((h, rest)) = digits.split_once(':') {
        let (m, s) = rest.split_once(':').unwrap_or((rest, "0"));
        (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?, s.parse::<i64>().ok()?)
    } else {
        match digits.len() {
            1 | 2 => (digits.parse().ok()?, 0, 0),
            4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?, 0),
            6 => (digits[..2].parse().ok()?, digits[2..4].parse().ok()?, digits[4..].parse().ok()?),
            _ => return None,
        }
    };
    if h > 15 || m > 59 || s > 59 {
        return None;
    }
    Some(sign * (h * 3600 + m * 60 + s))
}

pub const DATE_INFINITY: i32 = i32::MAX;
pub const DATE_NEG_INFINITY: i32 = i32::MIN;

/// The (year, month, day) of a finite date.
pub fn date_to_ymd(days: i32) -> Option<(i32, u32, u32)> {
    if days == DATE_INFINITY || days == DATE_NEG_INFINITY {
        return None;
    }
    Some(calendar::ymd_from_days(i64::from(days)))
}

/// Days since 2000-01-01 for a valid date.
pub fn date_from_ymd(year: i32, month: u32, day: u32) -> SqlResult<i32> {
    i32::try_from(calendar::days_from_ymd(year, month, day)).map_err(|_| SqlError::new(code::DATETIME_FIELD_OVERFLOW, "date out of range"))
}

pub fn format_date(days: i32) -> String {
    match date_to_ymd(days) {
        None => {
            if days == DATE_NEG_INFINITY {
                "-infinity".into()
            } else {
                "infinity".into()
            }
        }
        Some((year, m, d)) => {
            let (y, bc) = if year <= 0 { (1 - year, true) } else { (year, false) };
            let mut out = format!("{y:04}-{m:02}-{d:02}");
            if bc {
                out.push_str(" BC");
            }
            out
        }
    }
}

pub fn parse_date(input: &str) -> SqlResult<i32> {
    let s = input.trim();
    match s.to_ascii_lowercase().as_str() {
        "infinity" | "+infinity" => return Ok(DATE_INFINITY),
        "-infinity" => return Ok(DATE_NEG_INFINITY),
        "epoch" => return Ok(-10957),
        _ => {}
    }
    // A full timestamp is accepted too; the time part is discarded.
    let ts = parse_timestamp(s)
        .map_err(|_| SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type date: \"{input}\"")))?;
    Ok(i32::try_from(ts.div_euclid(USECS_PER_DAY)).unwrap_or(DATE_INFINITY))
}

/// `timestamp without time zone` output: `2024-07-15 12:00:00[.ffffff]`.
pub fn format_timestamp(micros: i64) -> String {
    if micros == TS_INFINITY || micros == TS_NEG_INFINITY {
        return if micros == TS_NEG_INFINITY { "-infinity".into() } else { "infinity".into() };
    }
    format_local(micros)
}

/// `timestamp` input: like timestamptz, but an explicit zone is ignored.
pub fn parse_timestamp(input: &str) -> SqlResult<i64> {
    let s = input.trim();
    let err = || SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type timestamp: \"{input}\""));
    let lower = s.to_ascii_lowercase();
    if matches!(lower.as_str(), "infinity" | "+infinity" | "-infinity" | "epoch") {
        return parse_timestamptz(s);
    }
    let date_end = s.find([' ', 'T', 't']).unwrap_or(s.len());
    if date_end < 8 {
        return Err(err());
    }
    let rest = s[date_end..].trim_start_matches(['T', 't', ' ']);
    let zone_start = rest.find(|c: char| c == '+' || (c == '-') || c.is_ascii_alphabetic()).unwrap_or(rest.len());
    let without_zone = format!("{}{}", &s[..date_end], if rest.is_empty() { String::new() } else { format!(" {}", &rest[..zone_start]) });
    let bc = lower.ends_with(" bc");
    let base = if bc { format!("{without_zone} BC") } else { without_zone };
    parse_timestamptz(base.trim()).map_err(|_| err())
}

pub fn format_time(micros: i64) -> String {
    let h = micros / USECS_PER_HOUR;
    let m = (micros % USECS_PER_HOUR) / USECS_PER_MINUTE;
    let sec = (micros % USECS_PER_MINUTE) / USECS_PER_SEC;
    let mut out = format!("{h:02}:{m:02}:{sec:02}");
    push_fraction(&mut out, micros % USECS_PER_SEC);
    out
}

pub fn parse_time(input: &str) -> SqlResult<i64> {
    let s = input.trim();
    // Accept and drop a trailing zone; a leading date is accepted too.
    let core = if s.len() > 10 && s.as_bytes().get(4) == Some(&b'-') { s[10..].trim_start_matches(['T', ' ']) } else { s };
    let end = core.find(|c: char| c == '+' || c == '-' || c.is_ascii_alphabetic()).unwrap_or(core.len());
    parse_time_of_day(core[..end].trim())
        .ok_or_else(|| SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type time: \"{input}\"")))
}

pub fn format_timetz(micros: i64, zone_east_secs: i32) -> String {
    let mut out = format_time(micros);
    let sign = if zone_east_secs < 0 { '-' } else { '+' };
    let abs = zone_east_secs.unsigned_abs();
    let _ = write!(out, "{sign}{:02}", abs / 3600);
    if !abs.is_multiple_of(3600) {
        let _ = write!(out, ":{:02}", (abs % 3600) / 60);
    }
    out
}

pub fn parse_timetz(input: &str) -> SqlResult<(i64, i32)> {
    let s = input.trim();
    let err = || SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type time with time zone: \"{input}\""));
    let end = s.find(|c: char| c == '+' || c == '-' || c.is_ascii_alphabetic()).unwrap_or(s.len());
    let t = parse_time_of_day(s[..end].trim()).ok_or_else(err)?;
    let z = parse_zone(s[end..].trim()).ok_or_else(err)?;
    Ok((t, i32::try_from(z).map_err(|_| err())?))
}

/// Postgres `interval`: months, days and microseconds are kept separately because a month is
/// not a fixed number of days and a day is not always 24 hours.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    pub const ZERO: Interval = Interval { months: 0, days: 0, micros: 0 };

    pub fn new(months: i32, days: i32, micros: i64) -> Interval {
        Interval { months, days, micros }
    }

    pub fn from_micros(micros: i64) -> Interval {
        Interval { months: 0, days: 0, micros }
    }

    /// The comparison key Postgres uses (`interval_cmp_value`): months count as 30 days and
    /// days as 24 hours.
    pub fn total_micros_i128(&self) -> i128 {
        i128::from(self.months) * 30 * i128::from(USECS_PER_DAY)
            + i128::from(self.days) * i128::from(USECS_PER_DAY)
            + i128::from(self.micros)
    }

    pub fn checked_add(&self, other: &Interval) -> SqlResult<Interval> {
        let overflow = || SqlError::out_of_range("interval");
        Ok(Interval {
            months: self.months.checked_add(other.months).ok_or_else(overflow)?,
            days: self.days.checked_add(other.days).ok_or_else(overflow)?,
            micros: self.micros.checked_add(other.micros).ok_or_else(overflow)?,
        })
    }

    pub fn negate(&self) -> SqlResult<Interval> {
        let overflow = || SqlError::out_of_range("interval");
        Ok(Interval {
            months: self.months.checked_neg().ok_or_else(overflow)?,
            days: self.days.checked_neg().ok_or_else(overflow)?,
            micros: self.micros.checked_neg().ok_or_else(overflow)?,
        })
    }

    /// `interval * float8`, following Postgres' `interval_mul` cascade of fractional parts.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    pub fn mul_f64(&self, factor: f64) -> SqlResult<Interval> {
        let months_f = f64::from(self.months) * factor;
        let months = months_f.trunc();
        let mut days_f = f64::from(self.days) * factor;
        // Fractional months spill into days (30 days per month).
        days_f += (months_f - months) * 30.0;
        let days = days_f.trunc();
        let mut micros_f = self.micros as f64 * factor;
        micros_f += (days_f - days) * USECS_PER_DAY as f64;
        if !months.is_finite() || months.abs() > f64::from(i32::MAX) || days.abs() > f64::from(i32::MAX) || !micros_f.is_finite() {
            return Err(SqlError::out_of_range("interval"));
        }
        Ok(Interval { months: months as i32, days: days as i32, micros: micros_f.round() as i64 })
    }

    /// `justify_hours`-free conversion to a plain duration in microseconds (months = 30 days).
    pub fn approx_micros(&self) -> i128 {
        self.total_micros_i128()
    }
}

impl PartialOrd for Interval {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Interval {
    fn cmp(&self, other: &Self) -> Ordering {
        self.total_micros_i128().cmp(&other.total_micros_i128())
    }
}

/// Formats an interval in Postgres' default `IntervalStyle = postgres`:
/// `1 year 2 mons 3 days 04:05:06.789`, `-1 days +02:00:00`, `00:00:00`.
pub fn format_interval(iv: &Interval) -> String {
    let mut out = String::new();
    let years = iv.months / 12;
    let mons = iv.months % 12;
    let mut is_before = false;
    let mut is_zero = true;
    let add = |out: &mut String, value: i32, unit: &str, is_before: &mut bool, is_zero: &mut bool| {
        if value == 0 {
            return;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        let sign = if *is_before && value > 0 { "+" } else { "" };
        let plural = if value == 1 { "" } else { "s" };
        let _ = write!(out, "{sign}{value} {unit}{plural}");
        *is_before = value < 0;
        *is_zero = false;
    };
    add(&mut out, years, "year", &mut is_before, &mut is_zero);
    add(&mut out, mons, "mon", &mut is_before, &mut is_zero);
    add(&mut out, iv.days, "day", &mut is_before, &mut is_zero);
    if is_zero || iv.micros != 0 {
        let neg = iv.micros < 0;
        let abs = iv.micros.unsigned_abs();
        let usecs_per_hour = USECS_PER_HOUR.unsigned_abs();
        let hours = abs / usecs_per_hour;
        let rem = abs % usecs_per_hour;
        let minutes = rem / USECS_PER_MINUTE.unsigned_abs();
        let rem = rem % USECS_PER_MINUTE.unsigned_abs();
        let secs = rem / USECS_PER_SEC.unsigned_abs();
        let frac = rem % USECS_PER_SEC.unsigned_abs();
        if !out.is_empty() {
            out.push(' ');
        }
        let sign = if neg {
            "-"
        } else if is_before {
            "+"
        } else {
            ""
        };
        let _ = write!(out, "{sign}{hours:02}:{minutes:02}:{secs:02}");
        #[allow(clippy::cast_possible_wrap)]
        push_fraction(&mut out, frac as i64);
    }
    out
}

/// Parses interval input in the forms Postgres accepts in practice: unit lists
/// (`1 day 2 hours`, `3 mons`, `1.5 hours`, `-2 days`), clock times (`01:02:03`, `-00:00:01.5`),
/// mixes of both (`1 day 02:00:00`), the `@ ... ago` verbose style and ISO-8601 durations
/// (`P1Y2M3DT4H5M6S`).
pub fn parse_interval(input: &str) -> SqlResult<Interval> {
    let err = || SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid input syntax for type interval: \"{input}\""));
    let mut s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err(err());
    }
    if let Some(rest) = s.strip_prefix('p') {
        return parse_iso_interval(rest).ok_or_else(err);
    }
    if let Some(rest) = s.strip_prefix('@') {
        s = rest.trim().to_owned();
    }
    let mut ago = false;
    if let Some(rest) = s.strip_suffix("ago") {
        ago = true;
        s = rest.trim().to_owned();
    }
    let mut acc = IntervalAcc::default();
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if tok.contains(':') {
            let (neg, body) = match tok.as_bytes().first() {
                Some(b'-') => (true, &tok[1..]),
                Some(b'+') => (false, &tok[1..]),
                _ => (false, tok),
            };
            let micros = parse_clock(body).ok_or_else(err)?;
            #[allow(clippy::cast_precision_loss)]
            let m = micros as f64;
            acc.micros += if neg { -m } else { m };
            i += 1;
            continue;
        }
        // A number, optionally glued to its unit (`10d`, `3hours`).
        let split = tok.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(tok.len());
        let (num_str, glued_unit) = tok.split_at(split);
        let num: f64 = num_str.parse().map_err(|_| err())?;
        let unit = if glued_unit.is_empty() {
            i += 1;
            match tokens.get(i) {
                Some(u) => *u,
                // A bare trailing number is seconds.
                None => "second",
            }
        } else {
            glued_unit
        };
        acc.add_unit(num, unit).ok_or_else(err)?;
        i += 1;
    }
    let iv = acc.finish().ok_or_else(err)?;
    if ago { iv.negate() } else { Ok(iv) }
}

/// `[H]H:MM[:SS[.f]]` → microseconds. Hours may exceed 24.
fn parse_clock(s: &str) -> Option<i64> {
    let mut parts = s.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let (secs, frac) = match parts.next() {
        Some(p) => split_seconds(p)?,
        None => (0, 0),
    };
    if parts.next().is_some() || m >= 60 || secs >= 60 {
        return None;
    }
    Some(h * USECS_PER_HOUR + m * USECS_PER_MINUTE + secs * USECS_PER_SEC + frac)
}

#[derive(Default)]
struct IntervalAcc {
    months: f64,
    days: f64,
    micros: f64,
}

impl IntervalAcc {
    #[allow(clippy::cast_precision_loss)]
    fn add_unit(&mut self, n: f64, unit: &str) -> Option<()> {
        let unit = unit.trim_end_matches(',');
        match unit {
            "microsecond" | "microseconds" | "us" | "usec" | "usecs" | "useconds" => self.micros += n,
            "millisecond" | "milliseconds" | "ms" | "msec" | "msecs" | "mseconds" => self.micros += n * 1000.0,
            "second" | "seconds" | "s" | "sec" | "secs" => self.micros += n * USECS_PER_SEC as f64,
            "minute" | "minutes" | "m" | "min" | "mins" => self.micros += n * USECS_PER_MINUTE as f64,
            "hour" | "hours" | "h" | "hr" | "hrs" => self.micros += n * USECS_PER_HOUR as f64,
            "day" | "days" | "d" => self.days += n,
            "week" | "weeks" | "w" => self.days += n * 7.0,
            "month" | "months" | "mon" | "mons" => self.months += n,
            "year" | "years" | "y" | "yr" | "yrs" => self.months += n * 12.0,
            "decade" | "decades" => self.months += n * 120.0,
            "century" | "centuries" => self.months += n * 1200.0,
            "millennium" | "millennia" | "millenniums" => self.months += n * 12000.0,
            _ => return None,
        }
        Some(())
    }

    /// Cascades fractional months into days and fractional days into microseconds, as
    /// Postgres' `AdjustFractDays` / `AdjustFractSeconds` do.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn finish(self) -> Option<Interval> {
        let months = self.months.trunc();
        let days_f = self.days + (self.months - months) * 30.0;
        let days = days_f.trunc();
        let micros = self.micros + (days_f - days) * USECS_PER_DAY as f64;
        if months.abs() > f64::from(i32::MAX) || days.abs() > f64::from(i32::MAX) || !micros.is_finite() {
            return None;
        }
        Some(Interval { months: months as i32, days: days as i32, micros: micros.round() as i64 })
    }
}

/// ISO-8601 duration body after the leading `P`.
fn parse_iso_interval(s: &str) -> Option<Interval> {
    let mut acc = IntervalAcc::default();
    let mut in_time = false;
    let mut num = String::new();
    for c in s.chars() {
        match c {
            't' => in_time = true,
            '0'..='9' | '.' | '-' => num.push(c),
            unit => {
                let n: f64 = num.parse().ok()?;
                num.clear();
                let u = match (unit, in_time) {
                    ('y', false) => "year",
                    ('m', false) => "month",
                    ('w', false) => "week",
                    ('d', false) => "day",
                    ('h', true) => "hour",
                    ('m', true) => "minute",
                    ('s', true) => "second",
                    _ => return None,
                };
                acc.add_unit(n, u)?;
            }
        }
    }
    if !num.is_empty() {
        return None;
    }
    acc.finish()
}

/// `timestamptz + interval`: months are added on the calendar (clamping the day of month),
/// then days, then microseconds — Postgres' `timestamptz_pl_interval` in a UTC session.
pub fn add_interval(ts: i64, iv: &Interval) -> SqlResult<i64> {
    if ts == TS_INFINITY || ts == TS_NEG_INFINITY {
        return Ok(ts);
    }
    let overflow = || SqlError::new(code::DATETIME_FIELD_OVERFLOW, "timestamp out of range");
    let mut t = ts;
    if iv.months != 0 {
        let mut tm = Tm::from_micros(t);
        let total = i64::from(tm.year) * 12 + i64::from(tm.month) - 1 + i64::from(iv.months);
        tm.year = i32::try_from(total.div_euclid(12)).map_err(|_| overflow())?;
        tm.month = u32::try_from(total.rem_euclid(12)).map_err(|_| overflow())? + 1;
        tm.day = tm.day.min(calendar::days_in_month(tm.year, tm.month));
        t = from_tm(&tm)?;
    }
    if iv.days != 0 {
        t = t.checked_add(i64::from(iv.days) * USECS_PER_DAY).ok_or_else(overflow)?;
    }
    let out = t.checked_add(iv.micros).ok_or_else(overflow)?;
    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&out) {
        return Err(overflow());
    }
    Ok(out)
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    calendar::days_in_month(year, month)
}

/// `timestamptz - timestamptz`: Postgres returns days + time (no months), justified so that the
/// microsecond part is under a day (`interval_justify_hours`).
pub fn sub_timestamps(a: i64, b: i64) -> SqlResult<Interval> {
    let diff = a.checked_sub(b).ok_or_else(|| SqlError::out_of_range("interval"))?;
    let days = diff / USECS_PER_DAY;
    let micros = diff % USECS_PER_DAY;
    Ok(Interval { months: 0, days: i32::try_from(days).map_err(|_| SqlError::out_of_range("interval"))?, micros })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamptz_round_trip() {
        let t = parse_timestamptz("2024-07-15 12:00:00+00").expect("parse");
        assert_eq!(format_timestamptz(t), "2024-07-15 12:00:00+00");
        let t = parse_timestamptz("2024-07-15T12:00:00.5Z").expect("parse");
        assert_eq!(format_timestamptz(t), "2024-07-15 12:00:00.5+00");
        let t = parse_timestamptz("2024-07-15 14:30:00+02:30").expect("parse");
        assert_eq!(format_timestamptz(t), "2024-07-15 12:00:00+00");
        assert_eq!(parse_timestamptz("2000-01-01").ok(), Some(0));
    }

    #[test]
    fn interval_parse_and_format() {
        let cases = [
            ("1 day", "1 day"),
            ("2 days 3 hours 15 minutes", "2 days 03:15:00"),
            ("5 hours", "05:00:00"),
            ("1 year 2 months", "1 year 2 mons"),
            ("-1 day", "-1 days"),
            ("1.5 hours", "01:30:00"),
            ("00:00:01.5", "00:00:01.5"),
            ("@ 1 day ago", "-1 days"),
            ("P1Y2M3DT4H5M6S", "1 year 2 mons 3 days 04:05:06"),
            ("0 seconds", "00:00:00"),
            ("7 days", "7 days"),
        ];
        for (input, want) in cases {
            let iv = parse_interval(input).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(format_interval(&iv), want, "{input}");
        }
        assert_eq!(format_interval(&Interval::new(0, -1, 2 * USECS_PER_HOUR)), "-1 days +02:00:00");
    }

    #[test]
    fn add_months_clamps_day() {
        let jan31 = parse_timestamptz("2024-01-31 00:00:00+00").expect("parse");
        let feb = add_interval(jan31, &Interval::new(1, 0, 0)).expect("add");
        assert_eq!(format_timestamptz(feb), "2024-02-29 00:00:00+00");
    }
}

#[cfg(test)]
mod date_tests {
    use super::*;

    #[test]
    fn dates_times() {
        let d = parse_date("2026-05-04").expect("date");
        assert_eq!(format_date(d), "2026-05-04");
        assert_eq!(parse_date("2000-01-01").ok(), Some(0));
        let t = parse_timestamp("2026-05-04 12:00:00.25").expect("ts");
        assert_eq!(format_timestamp(t), "2026-05-04 12:00:00.25");
        assert_eq!(format_timestamp(parse_timestamp("2026-05-04T12:00:00+05").expect("ts")), "2026-05-04 12:00:00");
        assert_eq!(format_time(parse_time("13:45:30").expect("time")), "13:45:30");
        let (t, z) = parse_timetz("13:45:30+05:30").expect("timetz");
        assert_eq!(format_timetz(t, z), "13:45:30+05:30");
    }
}
