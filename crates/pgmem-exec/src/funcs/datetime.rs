//! Date/time functions and operators. The session time zone is UTC.

use super::{Resolved, lenient, no_function, no_operator, resolved, strict};
use crate::cast::{date_to_micros, micros_to_date};
use crate::eval::EvalCtx;
use pgmem_types::calendar::{self, Tm};
use pgmem_types::datetime::{
    self as dt, Interval, TS_INFINITY, TS_NEG_INFINITY, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC, add_interval,
    sub_timestamps,
};
use pgmem_types::{Numeric, SqlError, SqlResult, Type, Value, code};

fn is_ts(t: &Type) -> bool {
    matches!(t, Type::Timestamptz | Type::Timestamp | Type::Date)
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    match name {
        "now" | "current_timestamp" | "transaction_timestamp" if n == 0 => resolved(lenient("now", now_fn), vec![], Type::Timestamptz),
        "statement_timestamp" if n == 0 => resolved(lenient("statement_timestamp", stmt_ts_fn), vec![], Type::Timestamptz),
        "clock_timestamp" if n == 0 => resolved(lenient("clock_timestamp", clock_ts_fn), vec![], Type::Timestamptz),
        "localtimestamp" if n == 0 => resolved(
            lenient("localtimestamp", |a, c| {
                now_fn(a, c).map(|v| match v {
                    Value::Timestamptz(t) => Value::Timestamp(t),
                    o => o,
                })
            }),
            vec![],
            Type::Timestamp,
        ),
        "current_date" if n == 0 => resolved(
            lenient("current_date", |a, c| {
                now_fn(a, c).map(|v| match v {
                    Value::Timestamptz(t) => Value::Date(micros_to_date(t)),
                    o => o,
                })
            }),
            vec![],
            Type::Date,
        ),
        "current_time" if n == 0 => resolved(
            lenient("current_time", |a, c| {
                now_fn(a, c).map(|v| match v {
                    Value::Timestamptz(t) => Value::TimeTz(t.rem_euclid(USECS_PER_DAY), 0),
                    o => o,
                })
            }),
            vec![],
            Type::TimeTz,
        ),
        "localtime" if n == 0 => resolved(
            lenient("localtime", |a, c| {
                now_fn(a, c).map(|v| match v {
                    Value::Timestamptz(t) => Value::Time(t.rem_euclid(USECS_PER_DAY)),
                    o => o,
                })
            }),
            vec![],
            Type::Time,
        ),
        "date_part" | "extract" if n == 2 => {
            let src = match &args[1] {
                Type::Unknown => Type::Timestamptz,
                t @ (Type::Timestamptz | Type::Timestamp | Type::Date | Type::Interval | Type::Time | Type::TimeTz) => t.clone(),
                _ => return Err(err()),
            };
            if name == "extract" {
                resolved(strict("extract", extract_fn), vec![Type::Text, src], Type::Numeric)
            } else {
                resolved(strict("date_part", date_part_fn), vec![Type::Text, src], Type::Float8)
            }
        }
        "date_trunc" if n == 2 || n == 3 => {
            let src = match &args[1] {
                Type::Unknown => Type::Timestamptz,
                Type::Date => Type::Timestamptz,
                t @ (Type::Timestamptz | Type::Timestamp | Type::Interval) => t.clone(),
                _ => return Err(err()),
            };
            let mut targets = vec![Type::Text, src.clone()];
            if n == 3 {
                targets.push(Type::Text);
            }
            resolved(strict("date_trunc", date_trunc_fn), targets, src)
        }
        "age" => match args {
            [a] if is_ts(a) || *a == Type::Unknown => {
                let t = if *a == Type::Timestamp { Type::Timestamp } else { Type::Timestamptz };
                resolved(lenient("age", age1_fn), vec![t], Type::Interval)
            }
            [a, b] if (is_ts(a) || *a == Type::Unknown) && (is_ts(b) || *b == Type::Unknown) => {
                let t = super::common_type(a, b).filter(|t| *t != Type::Date).unwrap_or(Type::Timestamptz);
                let t = if t == Type::Text { Type::Timestamptz } else { t };
                resolved(strict("age", age2_fn), vec![t.clone(), t], Type::Interval)
            }
            _ => Err(err()),
        },
        "make_date" if n == 3 => resolved(strict("make_date", make_date_fn), vec![Type::Int4; 3], Type::Date),
        "make_time" if n == 3 => resolved(strict("make_time", make_time_fn), vec![Type::Int4, Type::Int4, Type::Float8], Type::Time),
        "make_timestamp" if n == 6 => resolved(
            strict("make_timestamp", make_timestamp_fn),
            vec![Type::Int4, Type::Int4, Type::Int4, Type::Int4, Type::Int4, Type::Float8],
            Type::Timestamp,
        ),
        "make_timestamptz" if n == 6 || n == 7 => {
            let mut targets = vec![Type::Int4, Type::Int4, Type::Int4, Type::Int4, Type::Int4, Type::Float8];
            if n == 7 {
                targets.push(Type::Text);
            }
            resolved(strict("make_timestamptz", make_timestamptz_fn), targets, Type::Timestamptz)
        }
        "make_interval" if n <= 7 => {
            let mut targets = vec![Type::Int4; n];
            if n == 7 {
                targets[6] = Type::Float8;
            }
            resolved(strict("make_interval", make_interval_fn), targets, Type::Interval)
        }
        "to_timestamp" => match args {
            [_] => resolved(strict("to_timestamp", to_timestamp_fn), vec![Type::Float8], Type::Timestamptz),
            [_, _] => resolved(strict("to_timestamp", to_timestamp_fmt_fn), vec![Type::Text, Type::Text], Type::Timestamptz),
            _ => Err(err()),
        },
        "to_date" if n == 2 => resolved(strict("to_date", to_date_fn), vec![Type::Text, Type::Text], Type::Date),
        "to_char" if n == 2 => {
            let src = match &args[0] {
                Type::Unknown => Type::Timestamptz,
                t => t.clone(),
            };
            if !(is_ts(&src) || src == Type::Interval || src.is_numeric()) {
                return Err(err());
            }
            let src = if src == Type::Date { Type::Timestamp } else { src };
            resolved(strict("to_char", to_char_fn), vec![src, Type::Text], Type::Text)
        }
        "justify_days" | "justify_hours" | "justify_interval" if n == 1 => {
            let f = match name {
                "justify_days" => strict("justify_days", |a, _| Ok(Value::Interval(justify_days(iv(&a[0]))))),
                "justify_hours" => strict("justify_hours", |a, _| Ok(Value::Interval(justify_hours(iv(&a[0]))))),
                _ => strict("justify_interval", |a, _| Ok(Value::Interval(justify_days(justify_hours(iv(&a[0])))))),
            };
            resolved(f, vec![Type::Interval], Type::Interval)
        }
        "isfinite" if n == 1 => {
            let t = if args[0] == Type::Unknown { Type::Timestamptz } else { args[0].clone() };
            resolved(
                strict("isfinite", |a, _| {
                    Ok(Value::Bool(match &a[0] {
                        Value::Timestamptz(t) | Value::Timestamp(t) => *t != TS_INFINITY && *t != TS_NEG_INFINITY,
                        Value::Date(d) => *d != dt::DATE_INFINITY && *d != dt::DATE_NEG_INFINITY,
                        _ => true,
                    }))
                }),
                vec![t],
                Type::Bool,
            )
        }
        "timezone" if n == 2 => resolve_at_time_zone(&args[1], &args[0]).map(Some),
        _ => Ok(None),
    }
}

/// `expr AT TIME ZONE zone` (also `timezone(zone, expr)`).
pub fn resolve_at_time_zone(src: &Type, _zone: &Type) -> SqlResult<Resolved> {
    match src {
        Type::Timestamptz | Type::Unknown => {
            Ok(Resolved { func: strict("timezone", tz_from_timestamptz), args: vec![Type::Text, Type::Timestamptz], ret: Type::Timestamp })
        }
        Type::Timestamp | Type::Date => {
            Ok(Resolved { func: strict("timezone", tz_from_timestamp), args: vec![Type::Text, Type::Timestamp], ret: Type::Timestamptz })
        }
        other => Err(no_function("timezone", &[Type::Text, other.clone()])),
    }
}

/// Offset east of UTC (seconds) for a zone name: `UTC`, `Z`, `GMT`, fixed offsets like
/// `+05:30` / `-08`, and POSIX-style `UTC+3` (which Postgres treats as 3 hours *west*).
fn zone_offset(zone: &str) -> SqlResult<i64> {
    let z = zone.trim();
    let upper = z.to_ascii_uppercase();
    if matches!(upper.as_str(), "UTC" | "Z" | "GMT" | "ETC/UTC" | "UCT" | "ZULU" | "UNIVERSAL") {
        return Ok(0);
    }
    let parse_hm = |s: &str| -> Option<i64> {
        let (sign, body) = match s.as_bytes().first()? {
            b'+' => (1, &s[1..]),
            b'-' => (-1, &s[1..]),
            _ => (1, s),
        };
        let (h, m) = body.split_once(':').unwrap_or((body, "0"));
        Some(sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60))
    };
    if let Some(rest) = upper.strip_prefix("UTC").or_else(|| upper.strip_prefix("GMT")) {
        // POSIX convention: positive means west of Greenwich.
        return parse_hm(rest).map(|o| -o).ok_or_else(|| bad_zone(zone));
    }
    if z.starts_with(['+', '-']) {
        // ISO-8601 convention for explicit offsets: positive means east.
        return parse_hm(z).ok_or_else(|| bad_zone(zone));
    }
    Err(bad_zone(zone))
}

fn bad_zone(zone: &str) -> SqlError {
    SqlError::new(code::INVALID_PARAMETER_VALUE, format!("time zone \"{zone}\" not recognized"))
        .with_hint("pgmem supports UTC and fixed-offset time zones only.")
}

fn tz_from_timestamptz(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let off = zone_offset(a[0].as_str().unwrap_or(""))?;
    match &a[1] {
        Value::Timestamptz(t) => Ok(Value::Timestamp(t + off * USECS_PER_SEC)),
        other => Err(SqlError::internal(format!("timezone() of {}", other.kind()))),
    }
}

fn tz_from_timestamp(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let off = zone_offset(a[0].as_str().unwrap_or(""))?;
    match &a[1] {
        Value::Timestamp(t) => Ok(Value::Timestamptz(t - off * USECS_PER_SEC)),
        other => Err(SqlError::internal(format!("timezone() of {}", other.kind()))),
    }
}

fn now_fn(_: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Timestamptz(ctx.env.txn_start))
}

fn stmt_ts_fn(_: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Timestamptz(ctx.env.stmt_start))
}

fn clock_ts_fn(_: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Timestamptz(ctx.env.clock.now_micros()))
}

fn iv(v: &Value) -> Interval {
    match v {
        Value::Interval(i) => *i,
        _ => Interval::ZERO,
    }
}

fn tm(micros: i64) -> SqlResult<Tm> {
    dt::to_tm(micros).ok_or_else(|| SqlError::new(code::DATETIME_FIELD_OVERFLOW, "timestamp out of range"))
}

/// Normalizes a timestamp-ish value to (micros, is_date).
fn ts_micros(v: &Value) -> Option<i64> {
    match v {
        Value::Timestamptz(t) | Value::Timestamp(t) => Some(*t),
        Value::Date(d) => Some(date_to_micros(*d)),
        _ => None,
    }
}

/// Postgres field value for EXTRACT, as a numeric with the scale Postgres uses.
fn extract_numeric(field: &str, v: &Value) -> SqlResult<Numeric> {
    let f = field.to_ascii_lowercase();
    let f = f.trim();
    if let Value::Interval(i) = v {
        return extract_interval(f, i);
    }
    if let Value::Time(t) | Value::TimeTz(t, _) = v {
        let t = *t;
        return Ok(match f {
            "hour" | "hours" => Numeric::from_i64(t / USECS_PER_HOUR),
            "minute" | "minutes" => Numeric::from_i64((t % USECS_PER_HOUR) / USECS_PER_MINUTE),
            "second" | "seconds" => micros_numeric(t % USECS_PER_MINUTE, 6),
            "milliseconds" => micros_numeric((t % USECS_PER_MINUTE) * 1000, 9).round(3),
            "microseconds" => Numeric::from_i64(t % USECS_PER_MINUTE),
            "epoch" => micros_numeric(t, 6),
            "timezone" | "timezone_hour" | "timezone_minute" => Numeric::from_i64(match v {
                Value::TimeTz(_, z) => i64::from(*z),
                _ => return Err(unit_not_supported(f, "time without time zone")),
            }),
            _ => return Err(unit_not_supported(f, "time without time zone")),
        });
    }
    let micros = ts_micros(v).ok_or_else(|| SqlError::internal("extract from non-datetime"))?;
    let is_date = matches!(v, Value::Date(_));
    let is_tz = matches!(v, Value::Timestamptz(_));
    if micros == TS_INFINITY || micros == TS_NEG_INFINITY {
        return Err(SqlError::feature_not_supported("extract from infinite timestamps is not supported by pgmem"));
    }
    let d = tm(micros)?;
    let date_unit = |n: i64| Numeric::from_i64(n);
    let sec_micros = i64::from(d.second) * USECS_PER_SEC + i64::from(d.micros);
    let year = i64::from(d.year);
    Ok(match f {
        "year" | "years" => date_unit(if year <= 0 { year - 1 } else { year }),
        "month" | "months" => date_unit(i64::from(d.month)),
        "day" | "days" => date_unit(i64::from(d.day)),
        "hour" | "hours" if !is_date => date_unit(i64::from(d.hour)),
        "minute" | "minutes" if !is_date => date_unit(i64::from(d.minute)),
        "second" | "seconds" if !is_date => micros_numeric(sec_micros, 6),
        "milliseconds" if !is_date => micros_numeric(sec_micros * 1000, 9).round(3),
        "microseconds" if !is_date => date_unit(sec_micros),
        "dow" => date_unit(i64::from(d.dow())),
        "isodow" => date_unit(i64::from(d.isodow())),
        "doy" => date_unit(i64::from(d.doy())),
        "week" => date_unit(i64::from(d.iso_week().1)),
        "isoyear" => {
            let y = i64::from(d.iso_week().0);
            date_unit(if y <= 0 { y - 1 } else { y })
        }
        "quarter" => date_unit(i64::from((d.month - 1) / 3 + 1)),
        "decade" => date_unit(if year > 0 { year / 10 } else { -((8 - (year - 1)) / 10) }),
        "century" => date_unit(if year > 0 { (year + 99) / 100 } else { -((99 - (year - 1)) / 100) }),
        "millennium" => date_unit(if year > 0 { (year + 999) / 1000 } else { -((999 - (year - 1)) / 1000) }),
        "julian" => {
            let jd = Numeric::from_i64(d.julian());
            if is_date {
                jd
            } else {
                let time = micros.rem_euclid(USECS_PER_DAY);
                jd.add(&Numeric::from_i64(time).div(&Numeric::from_i64(USECS_PER_DAY))?)
            }
        }
        "epoch" => {
            let unix = micros + dt::PG_EPOCH_UNIX_SECS * USECS_PER_SEC;
            if is_date { Numeric::from_i64(unix / USECS_PER_SEC) } else { micros_numeric(unix, 6) }
        }
        "timezone" | "timezone_hour" | "timezone_minute" if is_tz => Numeric::from_i64(0),
        _ => {
            return Err(unit_not_supported(
                f,
                if is_date {
                    "date"
                } else if is_tz {
                    "timestamp with time zone"
                } else {
                    "timestamp without time zone"
                },
            ));
        }
    })
}

/// Known units that don't apply to the type get 0A000 "not supported"; unknown words get
/// 22023 "not recognized", as in Postgres.
fn unit_not_supported(f: &str, ty: &str) -> SqlError {
    const KNOWN: &[&str] = &[
        "microseconds",
        "milliseconds",
        "second",
        "seconds",
        "minute",
        "minutes",
        "hour",
        "hours",
        "day",
        "days",
        "week",
        "month",
        "months",
        "quarter",
        "year",
        "years",
        "decade",
        "century",
        "millennium",
        "dow",
        "isodow",
        "doy",
        "epoch",
        "isoyear",
        "julian",
        "timezone",
        "timezone_hour",
        "timezone_minute",
    ];
    if KNOWN.contains(&f) {
        SqlError::new(code::FEATURE_NOT_SUPPORTED, format!("unit \"{f}\" not supported for type {ty}"))
    } else {
        SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unit \"{f}\" not recognized for type {ty}"))
    }
}

/// `micros / 10^6` as numeric with the given display scale.
fn micros_numeric(micros: i64, scale: u32) -> Numeric {
    Numeric::from_i64(micros).div_to_scale(&Numeric::from_i64(1_000_000), scale)
}

fn extract_interval(f: &str, i: &Interval) -> SqlResult<Numeric> {
    let time = i.micros;
    Ok(match f {
        "year" | "years" => Numeric::from_i64(i64::from(i.months / 12)),
        "month" | "months" => Numeric::from_i64(i64::from(i.months % 12)),
        "day" | "days" => Numeric::from_i64(i64::from(i.days)),
        "hour" | "hours" => Numeric::from_i64(time / USECS_PER_HOUR),
        "minute" | "minutes" => Numeric::from_i64((time % USECS_PER_HOUR) / USECS_PER_MINUTE),
        "second" | "seconds" => micros_numeric(time % USECS_PER_MINUTE, 6),
        "milliseconds" => micros_numeric((time % USECS_PER_MINUTE) * 1000, 9).round(3),
        "microseconds" => Numeric::from_i64(time % USECS_PER_MINUTE),
        "quarter" => Numeric::from_i64(i64::from((i.months % 12) / 3 + 1)),
        "decade" => Numeric::from_i64(i64::from(i.months / 120)),
        "century" => Numeric::from_i64(i64::from(i.months / 1200)),
        "millennium" => Numeric::from_i64(i64::from(i.months / 12000)),
        "epoch" => {
            // Postgres: months count as 30 days... plus a quarter day per year (365.25-day years).
            let years = i64::from(i.months / 12);
            let months = i64::from(i.months % 12);
            let secs_from_months = years * 31_557_600 + months * 30 * 86_400;
            micros_numeric((secs_from_months + i64::from(i.days) * 86_400) * USECS_PER_SEC + time, 6)
        }
        _ => return Err(unit_not_supported(f, "interval")),
    })
}

fn extract_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Numeric(extract_numeric(a[0].as_str().unwrap_or(""), &a[1])?))
}

fn date_part_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Float8(extract_numeric(a[0].as_str().unwrap_or(""), &a[1])?.to_f64()))
}

fn date_trunc_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let field = a[0].as_str().unwrap_or("").to_ascii_lowercase();
    if let Value::Interval(i) = &a[1] {
        let mut out = *i;
        let t = i.micros;
        match field.as_str() {
            "microseconds" => {}
            "milliseconds" => out.micros = t - t % 1000,
            "second" => out.micros = t - t % USECS_PER_SEC,
            "minute" => out.micros = t - t % USECS_PER_MINUTE,
            "hour" => out.micros = t - t % USECS_PER_HOUR,
            "day" => out.micros = 0,
            "month" => {
                out.micros = 0;
                out.days = 0;
            }
            "quarter" => {
                out = Interval::new(i.months - i.months % 3, 0, 0);
            }
            "year" => out = Interval::new(i.months - i.months % 12, 0, 0),
            "decade" => out = Interval::new(i.months - i.months % 120, 0, 0),
            "century" => out = Interval::new(i.months - i.months % 1200, 0, 0),
            "millennium" => out = Interval::new(i.months - i.months % 12000, 0, 0),
            other => return Err(unit_not_supported(other, "interval")),
        }
        return Ok(Value::Interval(out));
    }
    let micros = ts_micros(&a[1]).ok_or_else(|| SqlError::internal("date_trunc of non-timestamp"))?;
    if micros == TS_INFINITY || micros == TS_NEG_INFINITY {
        return Ok(a[1].clone());
    }
    let off = match a.get(2) {
        Some(z) => zone_offset(z.as_str().unwrap_or(""))? * USECS_PER_SEC,
        None => 0,
    };
    let d = tm(micros + off)?;
    let day_start = |year: i32, month: u32, day: u32| Tm { year, month, day, hour: 0, minute: 0, second: 0, micros: 0 };
    let truncated: Tm = match field.as_str() {
        "microseconds" => d,
        "milliseconds" => Tm { micros: d.micros / 1000 * 1000, ..d },
        "second" => Tm { micros: 0, ..d },
        "minute" => Tm { second: 0, micros: 0, ..d },
        "hour" => Tm { minute: 0, second: 0, micros: 0, ..d },
        "day" => day_start(d.year, d.month, d.day),
        "week" => {
            let monday = calendar::days_from_ymd(d.year, d.month, d.day) - i64::from(d.isodow() - 1);
            let (y, m, dd) = calendar::ymd_from_days(monday);
            day_start(y, m, dd)
        }
        "month" => day_start(d.year, d.month, 1),
        "quarter" => day_start(d.year, (d.month - 1) / 3 * 3 + 1, 1),
        "year" => day_start(d.year, 1, 1),
        "decade" => day_start(d.year.div_euclid(10) * 10, 1, 1),
        "century" => day_start((d.year - 1).div_euclid(100) * 100 + 1, 1, 1),
        "millennium" => day_start((d.year - 1).div_euclid(1000) * 1000 + 1, 1, 1),
        other => {
            return Err(SqlError::new(
                code::INVALID_PARAMETER_VALUE,
                format!("unit \"{other}\" not recognized for type timestamp with time zone"),
            ));
        }
    };
    let t = dt::from_tm(&truncated)? - off;
    Ok(match &a[1] {
        Value::Timestamp(_) => Value::Timestamp(t),
        _ => Value::Timestamptz(t),
    })
}

fn days_in(y: i32, m: u32) -> i64 {
    i64::from(calendar::days_in_month(y, m))
}

/// Postgres' `timestamp_age`: field-wise difference with borrowing.
fn age(t1: i64, t2: i64) -> SqlResult<Interval> {
    let (d1, d2) = (tm(t1)?, tm(t2)?);
    let fields = |d: &Tm| {
        [
            i64::from(d.year),
            i64::from(d.month),
            i64::from(d.day),
            i64::from(d.hour),
            i64::from(d.minute),
            i64::from(d.second),
            i64::from(d.micros),
        ]
    };
    let (f1, f2) = (fields(&d1), fields(&d2));
    let mut tm: Vec<i64> = f1.iter().zip(&f2).map(|(a, b)| a - b).collect();
    let neg = t1 < t2;
    if neg {
        tm.iter_mut().for_each(|x| *x = -*x);
    }
    let [year, mon, mday, hour, min, sec, usec] = &mut tm[..] else { return Err(SqlError::internal("age fields")) };
    while *usec < 0 {
        *usec += USECS_PER_SEC;
        *sec -= 1;
    }
    while *sec < 0 {
        *sec += 60;
        *min -= 1;
    }
    while *min < 0 {
        *min += 60;
        *hour -= 1;
    }
    while *hour < 0 {
        *hour += 24;
        *mday -= 1;
    }
    while *mday < 0 {
        let (y, m) = if neg { (d1.year, d1.month) } else { (d2.year, d2.month) };
        *mday += days_in(y, m);
        *mon -= 1;
    }
    while *mon < 0 {
        *mon += 12;
        *year -= 1;
    }
    let sign = if neg { -1 } else { 1 };
    let months = sign * (*year * 12 + *mon);
    let days = sign * *mday;
    let micros = sign * (*hour * USECS_PER_HOUR + *min * USECS_PER_MINUTE + *sec * USECS_PER_SEC + *usec);
    Ok(Interval {
        months: i32::try_from(months).map_err(|_| SqlError::out_of_range("interval"))?,
        days: i32::try_from(days).map_err(|_| SqlError::out_of_range("interval"))?,
        micros,
    })
}

fn age2_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (Some(x), Some(y)) = (ts_micros(&a[0]), ts_micros(&a[1])) else { return Err(SqlError::internal("age of non-timestamp")) };
    Ok(Value::Interval(age(x, y)?))
}

/// `age(ts)`: from midnight of the current date.
fn age1_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let today = date_to_micros(micros_to_date(ctx.env.txn_start));
    let x = ts_micros(&a[0]).ok_or_else(|| SqlError::internal("age of non-timestamp"))?;
    Ok(Value::Interval(age(today, x)?))
}

fn i32_arg(v: &Value) -> i32 {
    i32::try_from(v.as_i64().unwrap_or(0)).unwrap_or(0)
}

/// Days since 2000-01-01 for `make_date` arguments (negative years are BC).
fn make_date(y: i32, m: i32, d: i32) -> SqlResult<i32> {
    let bad = || SqlError::new(code::DATETIME_FIELD_OVERFLOW, format!("date field value out of range: {y}-{m:02}-{d:02}"));
    let year = if y < 0 { y + 1 } else { y };
    let (Ok(month), Ok(day)) = (u32::try_from(m), u32::try_from(d)) else { return Err(bad()) };
    if y == 0 || !calendar::valid_date(year, month, day) {
        return Err(bad());
    }
    dt::date_from_ymd(year, month, day)
}

fn make_time(h: i32, m: i32, s: f64) -> SqlResult<i64> {
    if !(0..=23).contains(&h) || !(0..=59).contains(&m) || !(0.0..60.0).contains(&s) {
        return Err(SqlError::new(code::DATETIME_FIELD_OVERFLOW, format!("time field value out of range: {h}:{m:02}:{s:02}")));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(i64::from(h) * USECS_PER_HOUR + i64::from(m) * USECS_PER_MINUTE + (s * 1_000_000.0).round() as i64)
}

fn make_date_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Date(make_date(i32_arg(&a[0]), i32_arg(&a[1]), i32_arg(&a[2]))?))
}

fn make_time_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Time(make_time(i32_arg(&a[0]), i32_arg(&a[1]), a[2].as_f64().unwrap_or(0.0))?))
}

fn make_ts(a: &[Value]) -> SqlResult<i64> {
    let d = make_date(i32_arg(&a[0]), i32_arg(&a[1]), i32_arg(&a[2]))?;
    let t = make_time(i32_arg(&a[3]), i32_arg(&a[4]), a[5].as_f64().unwrap_or(0.0))?;
    Ok(date_to_micros(d) + t)
}

fn make_timestamp_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Timestamp(make_ts(a)?))
}

fn make_timestamptz_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let off = match a.get(6) {
        Some(z) => zone_offset(z.as_str().unwrap_or(""))?,
        None => 0,
    };
    Ok(Value::Timestamptz(make_ts(a)? - off * USECS_PER_SEC))
}

fn make_interval_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let get = |i: usize| a.get(i).and_then(Value::as_i64).unwrap_or(0);
    let months = get(0) * 12 + get(1);
    let days = get(2) * 7 + get(3);
    #[allow(clippy::cast_possible_truncation)]
    let secs = a.get(6).and_then(Value::as_f64).map_or(0, |s| (s * 1_000_000.0).round() as i64);
    let micros = get(4) * USECS_PER_HOUR + get(5) * USECS_PER_MINUTE + secs;
    Ok(Value::Interval(Interval {
        months: i32::try_from(months).map_err(|_| SqlError::out_of_range("interval"))?,
        days: i32::try_from(days).map_err(|_| SqlError::out_of_range("interval"))?,
        micros,
    }))
}

fn to_timestamp_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let secs = a[0].as_f64().unwrap_or(f64::NAN);
    if secs.is_nan() {
        return Err(SqlError::new(code::DATETIME_FIELD_OVERFLOW, "timestamp cannot be NaN"));
    }
    if secs.is_infinite() {
        return Ok(Value::Timestamptz(if secs > 0.0 { TS_INFINITY } else { TS_NEG_INFINITY }));
    }
    #[allow(clippy::cast_possible_truncation)]
    let micros = (secs * 1_000_000.0).round() as i64 - dt::PG_EPOCH_UNIX_SECS * USECS_PER_SEC;
    Ok(Value::Timestamptz(micros))
}

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const ROMAN: [&str; 12] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X", "XI", "XII"];

/// Template patterns in matching priority (longest first), as in Postgres' formatting.c.
const PATTERNS: &[&str] = &[
    "A.D.", "a.d.", "A.M.", "a.m.", "B.C.", "b.c.", "P.M.", "p.m.", "Y,YYY", "y,yyy", "HH24", "hh24", "HH12", "hh12", "IDDD", "iddd",
    "IYYY", "iyyy", "MONTH", "Month", "month", "SSSSS", "sssss", "SSSS", "ssss", "YYYY", "yyyy", "DDD", "ddd", "IYY", "iyy", "MON", "Mon",
    "mon", "TZH", "tzh", "TZM", "tzm", "YYY", "yyy", "DAY", "Day", "day", "AD", "ad", "AM", "am", "BC", "bc", "CC", "cc", "DD", "dd", "DY",
    "Dy", "dy", "HH", "hh", "ID", "id", "IW", "iw", "IY", "iy", "MI", "mi", "MM", "mm", "MS", "ms", "OF", "of", "PM", "pm", "RM", "rm",
    "SS", "ss", "TZ", "tz", "US", "us", "WW", "ww", "YY", "yy", "D", "d", "I", "i", "J", "j", "Q", "q", "W", "w", "Y", "y",
];

fn ordinal_suffix(n: i64, upper: bool) -> &'static str {
    let s = match (n % 100, n % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    if upper {
        match s {
            "st" => "ST",
            "nd" => "ND",
            "rd" => "RD",
            _ => "TH",
        }
    } else {
        s
    }
}

/// Applies Postgres' case convention for a name pattern (`MONTH` / `Month` / `month`).
fn cased(name: &str, pattern: &str) -> String {
    if pattern.chars().all(|c| !c.is_ascii_lowercase()) {
        name.to_uppercase()
    } else if pattern.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        name.to_owned()
    } else {
        name.to_lowercase()
    }
}

/// `to_char(timestamp, template)` (UTC session).
fn format_template(t: &Tm, fmt: &str) -> String {
    let mut out = String::new();
    let mut rest = fmt;
    while !rest.is_empty() {
        if let Some(q) = rest.strip_prefix('"') {
            let mut chars = q.char_indices();
            let mut end = q.len();
            while let Some((i, c)) = chars.next() {
                if c == '\\' {
                    if let Some((_, n)) = chars.next() {
                        out.push(n);
                    }
                    continue;
                }
                if c == '"' {
                    end = i;
                    break;
                }
                out.push(c);
            }
            rest = q.get(end + 1..).unwrap_or("");
            continue;
        }
        let mut fill = true;
        let mut body = rest;
        loop {
            if let Some(b) = body.strip_prefix("FM").or_else(|| body.strip_prefix("fm")) {
                fill = false;
                body = b;
            } else if let Some(b) = body.strip_prefix("FX").or_else(|| body.strip_prefix("fx")) {
                body = b;
            } else {
                break;
            }
        }
        let Some(pat) = PATTERNS.iter().find(|p| body.starts_with(**p)) else {
            let ch = rest.chars().next().unwrap_or(' ');
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
            continue;
        };
        let mut after = &body[pat.len()..];
        let (th, th_upper) = if let Some(a) = after.strip_prefix("TH") {
            after = a;
            (true, true)
        } else if let Some(a) = after.strip_prefix("th") {
            after = a;
            (true, false)
        } else {
            (false, false)
        };
        let year = i64::from(if t.year <= 0 { 1 - t.year } else { t.year });
        let num = |v: i64, width: usize| if fill { format!("{v:0width$}") } else { v.to_string() };
        let hour12 = i64::from(if t.hour.is_multiple_of(12) { 12 } else { t.hour % 12 });
        let upper = pat.chars().all(|c| !c.is_ascii_lowercase());
        let (piece, value): (String, Option<i64>) = match pat.to_ascii_uppercase().as_str() {
            "HH24" => (num(i64::from(t.hour), 2), Some(i64::from(t.hour))),
            "HH12" | "HH" => (num(hour12, 2), Some(hour12)),
            "MI" => (num(i64::from(t.minute), 2), Some(i64::from(t.minute))),
            "SS" => (num(i64::from(t.second), 2), Some(i64::from(t.second))),
            "MS" => (num(i64::from(t.micros / 1000), 3), None),
            "US" => (num(i64::from(t.micros), 6), None),
            "SSSS" | "SSSSS" => {
                let v = i64::from(t.hour) * 3600 + i64::from(t.minute) * 60 + i64::from(t.second);
                (v.to_string(), Some(v))
            }
            "AM" | "PM" => ((if t.hour < 12 { "AM" } else { "PM" }).to_owned(), None),
            "A.M." | "P.M." => ((if t.hour < 12 { "A.M." } else { "P.M." }).to_owned(), None),
            "AD" | "BC" => ((if t.year <= 0 { "BC" } else { "AD" }).to_owned(), None),
            "A.D." | "B.C." => ((if t.year <= 0 { "B.C." } else { "A.D." }).to_owned(), None),
            "Y,YYY" => (format!("{},{:03}", year / 1000, year % 1000), Some(year)),
            "YYYY" => (num(year, 4), Some(year)),
            "YYY" => (num(year % 1000, 3), Some(year % 1000)),
            "YY" => (num(year % 100, 2), Some(year % 100)),
            "Y" => ((year % 10).to_string(), Some(year % 10)),
            "IYYY" => (num(i64::from(t.iso_week().0), 4), Some(i64::from(t.iso_week().0))),
            "IYY" => (num(i64::from(t.iso_week().0) % 1000, 3), None),
            "IY" => (num(i64::from(t.iso_week().0) % 100, 2), None),
            "I" => ((i64::from(t.iso_week().0) % 10).to_string(), None),
            "MONTH" => {
                let name = MONTHS[(t.month - 1) as usize];
                (if fill { format!("{:<9}", cased(name, pat)) } else { cased(name, pat) }, None)
            }
            "MON" => (cased(&MONTHS[(t.month - 1) as usize][..3], pat), None),
            "MM" => (num(i64::from(t.month), 2), Some(i64::from(t.month))),
            "DAY" => {
                let name = DAYS[t.dow() as usize];
                (if fill { format!("{:<9}", cased(name, pat)) } else { cased(name, pat) }, None)
            }
            "DY" => (cased(&DAYS[t.dow() as usize][..3], pat), None),
            "DDD" => (num(i64::from(t.doy()), 3), Some(i64::from(t.doy()))),
            "IDDD" => {
                let (iy, iw) = t.iso_week();
                let _ = iy;
                let v = (i64::from(iw) - 1) * 7 + i64::from(t.isodow());
                (num(v, 3), Some(v))
            }
            "DD" => (num(i64::from(t.day), 2), Some(i64::from(t.day))),
            "D" => ((t.dow() + 1).to_string(), Some(i64::from(t.dow() + 1))),
            "ID" => (t.isodow().to_string(), Some(i64::from(t.isodow()))),
            "W" => (((t.day - 1) / 7 + 1).to_string(), Some(i64::from((t.day - 1) / 7 + 1))),
            "WW" => (num(i64::from((t.doy() - 1) / 7 + 1), 2), Some(i64::from((t.doy() - 1) / 7 + 1))),
            "IW" => (num(i64::from(t.iso_week().1), 2), Some(i64::from(t.iso_week().1))),
            "CC" => {
                let c = (year + 99) / 100;
                (num(c, 2), Some(c))
            }
            "J" => (t.julian().to_string(), Some(t.julian())),
            "Q" => (((t.month - 1) / 3 + 1).to_string(), Some(i64::from((t.month - 1) / 3 + 1))),
            "RM" => {
                let r = ROMAN[(t.month - 1) as usize];
                let r = if fill { format!("{r:<4}") } else { r.to_owned() };
                (if upper { r } else { r.to_lowercase() }, None)
            }
            "TZ" => ((if upper { "UTC" } else { "utc" }).to_owned(), None),
            "TZH" => ("+00".to_owned(), None),
            "TZM" => ("00".to_owned(), None),
            "OF" => ("+00".to_owned(), None),
            _ => (pat.to_string(), None),
        };
        let piece = match pat.as_bytes()[0] {
            b'a' | b'p' | b'b' => piece.to_lowercase(),
            _ => piece,
        };
        out.push_str(&piece);
        if th && let Some(v) = value {
            out.push_str(ordinal_suffix(v, th_upper));
        }
        rest = after;
    }
    out
}

fn to_char_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let fmt = a[1].as_str().unwrap_or("");
    match &a[0] {
        Value::Timestamptz(t) | Value::Timestamp(t) => Ok(Value::Text(format_template(&tm(*t)?, fmt))),
        Value::Interval(i) => {
            // Postgres formats intervals field-wise; hours may exceed 24.
            let base = Tm {
                year: i.months / 12,
                month: u32::try_from(i.months % 12).unwrap_or(0).max(1),
                day: u32::try_from(i.days).unwrap_or(0),
                ..Tm::from_micros(i.micros.rem_euclid(USECS_PER_DAY))
            };
            Ok(Value::Text(format_template(&Tm { hour: u32::try_from(i.micros / USECS_PER_HOUR).unwrap_or(0), ..base }, fmt)))
        }
        v => {
            // Numeric formatting: the common `FM999...`, `9990.00` shapes.
            let n = v.as_f64().unwrap_or(0.0);
            let decimals = fmt.split_once(['.', 'D']).map_or(0, |(_, f)| f.chars().filter(|c| *c == '9' || *c == '0').count());
            let s = format!("{n:.decimals$}");
            Ok(Value::Text(if fmt.starts_with("FM") { s } else { format!(" {s}") }))
        }
    }
}

/// Parses `src` according to a Postgres template (the patterns that carry date/time fields).
fn parse_template(src: &str, fmt: &str) -> SqlResult<Tm> {
    let bad = || SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid value \"{src}\" for \"{fmt}\""));
    let mut t = Tm { year: 1, month: 1, day: 1, hour: 0, minute: 0, second: 0, micros: 0 };
    let mut pm: Option<bool> = None;
    let mut s = src.trim_start();
    let mut f = fmt;
    let take_num = |s: &mut &str, max: usize| -> Option<i64> {
        let n = s.chars().take(max).take_while(char::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        let v = s[..n].parse().ok();
        *s = &s[n..];
        v
    };
    while !f.is_empty() {
        let body = f.strip_prefix("FM").unwrap_or(f);
        if let Some(pat) = PATTERNS.iter().find(|p| body.starts_with(**p)) {
            f = &body[pat.len()..];
            match pat.to_ascii_uppercase().as_str() {
                "YYYY" => t.year = i32::try_from(take_num(&mut s, 4).ok_or_else(bad)?).map_err(|_| bad())?,
                "YY" => t.year = 2000 + i32::try_from(take_num(&mut s, 2).ok_or_else(bad)?).map_err(|_| bad())?,
                "MM" => t.month = u32::try_from(take_num(&mut s, 2).ok_or_else(bad)?).map_err(|_| bad())?,
                "DD" => t.day = u32::try_from(take_num(&mut s, 2).ok_or_else(bad)?).map_err(|_| bad())?,
                "HH24" | "HH12" | "HH" => t.hour = u32::try_from(take_num(&mut s, 2).ok_or_else(bad)?).map_err(|_| bad())?,
                "MI" => t.minute = u32::try_from(take_num(&mut s, 2).ok_or_else(bad)?).map_err(|_| bad())?,
                "SS" => t.second = u32::try_from(take_num(&mut s, 2).ok_or_else(bad)?).map_err(|_| bad())?,
                "MS" => t.micros = u32::try_from(take_num(&mut s, 3).ok_or_else(bad)? * 1000).map_err(|_| bad())?,
                "US" => t.micros = u32::try_from(take_num(&mut s, 6).ok_or_else(bad)?).map_err(|_| bad())?,
                "MON" | "MONTH" => {
                    let idx = MONTHS.iter().position(|m| s.len() >= 3 && s[..3].eq_ignore_ascii_case(&m[..3])).ok_or_else(bad)?;
                    let full = MONTHS[idx];
                    s = if s.len() >= full.len() && s[..full.len()].eq_ignore_ascii_case(full) { &s[full.len()..] } else { &s[3..] };
                    t.month = u32::try_from(idx + 1).unwrap_or(1);
                }
                "AM" | "PM" => {
                    let up = s.get(..2).map(str::to_ascii_uppercase);
                    pm = Some(up.as_deref() == Some("PM"));
                    s = s.get(2..).unwrap_or("");
                }
                _ => {
                    return Err(SqlError::feature_not_supported(format!(
                        "template pattern \"{pat}\" is not supported by pgmem for parsing"
                    )));
                }
            }
        } else {
            let fc = f.chars().next().unwrap_or(' ');
            f = &f[fc.len_utf8()..];
            // Separators in the template match any run of non-alphanumeric input characters.
            s = s.trim_start_matches(|c: char| !c.is_alphanumeric());
            let _ = fc;
        }
    }
    if let Some(is_pm) = pm {
        t.hour = t.hour % 12 + if is_pm { 12 } else { 0 };
    }
    if !calendar::valid_date(t.year, t.month, t.day) || t.hour > 23 || t.minute > 59 || t.second > 59 {
        return Err(SqlError::new(code::DATETIME_FIELD_OVERFLOW, format!("date/time field value out of range: \"{src}\"")));
    }
    Ok(t)
}

fn to_timestamp_fmt_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let t = parse_template(a[0].as_str().unwrap_or(""), a[1].as_str().unwrap_or(""))?;
    Ok(Value::Timestamptz(dt::from_tm(&t)?))
}

fn to_date_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let t = parse_template(a[0].as_str().unwrap_or(""), a[1].as_str().unwrap_or(""))?;
    Ok(Value::Date(dt::date_from_ymd(t.year, t.month, t.day)?))
}

fn justify_hours(i: Interval) -> Interval {
    let mut out = i;
    out.days += i32::try_from(i.micros / USECS_PER_DAY).unwrap_or(0);
    out.micros %= USECS_PER_DAY;
    if out.days > 0 && out.micros < 0 {
        out.micros += USECS_PER_DAY;
        out.days -= 1;
    } else if out.days < 0 && out.micros > 0 {
        out.micros -= USECS_PER_DAY;
        out.days += 1;
    }
    out
}

fn justify_days(i: Interval) -> Interval {
    let mut out = i;
    out.months += i.days / 30;
    out.days %= 30;
    if out.months > 0 && out.days < 0 {
        out.days += 30;
        out.months -= 1;
    } else if out.months < 0 && out.days > 0 {
        out.days -= 30;
        out.months += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------------------------

fn ts_plus_interval(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (ts, i) = match (&a[0], &a[1]) {
        (Value::Interval(i), t) => (t, *i),
        (t, Value::Interval(i)) => (t, *i),
        _ => return Err(SqlError::internal("timestamp + interval")),
    };
    Ok(match ts {
        Value::Timestamptz(t) => Value::Timestamptz(add_interval(*t, &i)?),
        Value::Timestamp(t) => Value::Timestamp(add_interval(*t, &i)?),
        Value::Date(d) => Value::Timestamp(add_interval(date_to_micros(*d), &i)?),
        Value::Time(t) => Value::Time((t + i.micros).rem_euclid(USECS_PER_DAY)),
        other => return Err(SqlError::internal(format!("{} + interval", other.kind()))),
    })
}

fn ts_minus_interval(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let Value::Interval(i) = &a[1] else { return Err(SqlError::internal("timestamp - interval")) };
    ts_plus_interval(&[a[0].clone(), Value::Interval(i.negate()?)], ctx)
}

fn ts_minus_ts(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    match (&a[0], &a[1]) {
        (Value::Time(x), Value::Time(y)) => Ok(Value::Interval(Interval::from_micros(x - y))),
        (x, y) => {
            let (Some(x), Some(y)) = (ts_micros(x), ts_micros(y)) else { return Err(SqlError::internal("timestamp - timestamp")) };
            if [x, y].iter().any(|t| *t == TS_INFINITY || *t == TS_NEG_INFINITY) {
                return Err(SqlError::new(code::DATETIME_FIELD_OVERFLOW, "cannot subtract infinite timestamps"));
            }
            Ok(Value::Interval(sub_timestamps(x, y)?))
        }
    }
}

fn date_plus_int(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (d, n) = match (&a[0], &a[1]) {
        (Value::Date(d), n) | (n, Value::Date(d)) => (*d, n.as_i64().unwrap_or(0)),
        _ => return Err(SqlError::internal("date + integer")),
    };
    Ok(Value::Date(
        d.checked_add(i32::try_from(n).map_err(|_| SqlError::new(code::DATETIME_FIELD_OVERFLOW, "date out of range"))?)
            .ok_or_else(|| SqlError::new(code::DATETIME_FIELD_OVERFLOW, "date out of range"))?,
    ))
}

fn date_minus_int(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    match (&a[0], &a[1]) {
        (Value::Date(d), n) => Ok(Value::Date(d - i32::try_from(n.as_i64().unwrap_or(0)).unwrap_or(0))),
        _ => Err(SqlError::internal("date - integer")),
    }
}

fn date_minus_date(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    match (&a[0], &a[1]) {
        (Value::Date(x), Value::Date(y)) => Ok(Value::Int4(x - y)),
        _ => Err(SqlError::internal("date - date")),
    }
}

fn date_plus_time(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    match (&a[0], &a[1]) {
        (Value::Date(d), Value::Time(t)) | (Value::Time(t), Value::Date(d)) => Ok(Value::Timestamp(date_to_micros(*d) + t)),
        _ => Err(SqlError::internal("date + time")),
    }
}

fn interval_add(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Interval(iv(&a[0]).checked_add(&iv(&a[1]))?))
}

fn interval_sub(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Interval(iv(&a[0]).checked_add(&iv(&a[1]).negate()?)?))
}

fn interval_mul(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (i, f) = match (&a[0], &a[1]) {
        (Value::Interval(i), f) | (f, Value::Interval(i)) => (*i, f.as_f64().unwrap_or(0.0)),
        _ => return Err(SqlError::internal("interval * number")),
    };
    Ok(Value::Interval(i.mul_f64(f)?))
}

fn interval_div(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let f = a[1].as_f64().unwrap_or(0.0);
    if f == 0.0 {
        return Err(SqlError::division_by_zero());
    }
    Ok(Value::Interval(iv(&a[0]).mul_f64(1.0 / f)?))
}

pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    use Type::*;
    let res = |name: &'static str, f: crate::plan::ScalarFn, lt: Type, rt: Type, ret: Type| resolved(strict(name, f), vec![lt, rt], ret);
    let dt_like = |t: &Type| matches!(t, Timestamptz | Timestamp | Date | Time | TimeTz | Interval);
    if !(dt_like(l) || dt_like(r)) {
        return Ok(None);
    }
    match (op, l, r) {
        ("+", Timestamptz, Interval | Unknown) | ("+", Unknown, Interval) => res("+", ts_plus_interval, Timestamptz, Interval, Timestamptz),
        ("+", Interval, Timestamptz) => res("+", ts_plus_interval, Interval, Timestamptz, Timestamptz),
        ("+", Timestamp, Interval | Unknown) => res("+", ts_plus_interval, Timestamp, Interval, Timestamp),
        ("+", Interval, Timestamp) => res("+", ts_plus_interval, Interval, Timestamp, Timestamp),
        ("+", Date, Interval) | ("+", Interval, Date) => res("+", ts_plus_interval, l.clone(), r.clone(), Timestamp),
        ("+", Date, t) | ("+", t, Date) if t.is_integer() || *t == Unknown => {
            if *l == Date {
                res("+", date_plus_int, Date, Int4, Date)
            } else {
                res("+", date_plus_int, Int4, Date, Date)
            }
        }
        ("+", Date, Time) | ("+", Time, Date) => res("+", date_plus_time, l.clone(), r.clone(), Timestamp),
        ("+", Time, Interval) => res("+", ts_plus_interval, Time, Interval, Time),
        ("+", Interval, Interval | Unknown) => res("+", interval_add, Interval, Interval, Interval),
        ("-", Timestamptz, Interval) => res("-", ts_minus_interval, Timestamptz, Interval, Timestamptz),
        ("-", Timestamp, Interval) => res("-", ts_minus_interval, Timestamp, Interval, Timestamp),
        ("-", Date, Interval) => res("-", ts_minus_interval, Date, Interval, Timestamp),
        ("-", Time, Interval) => res("-", ts_minus_interval, Time, Interval, Time),
        ("-", Date, t) if t.is_integer() => res("-", date_minus_int, Date, Int4, Date),
        ("-", Date, Date | Unknown) => res("-", date_minus_date, Date, Date, Int4),
        ("-", Time, Time) => res("-", ts_minus_ts, Time, Time, Interval),
        ("-", Interval, Interval | Unknown) => res("-", interval_sub, Interval, Interval, Interval),
        ("-", a, b) if matches!(a, Timestamptz | Timestamp | Date | Unknown) && matches!(b, Timestamptz | Timestamp | Date | Unknown) => {
            let t = super::common_type(a, b).unwrap_or(Timestamptz);
            let t = if t == Text || t == Date { Timestamp } else { t };
            res("-", ts_minus_ts, t.clone(), t, Interval)
        }
        ("*", Interval, t) if t.is_numeric() || *t == Unknown => res("*", interval_mul, Interval, Float8, Interval),
        ("*", t, Interval) if t.is_numeric() || *t == Unknown => res("*", interval_mul, Float8, Interval, Interval),
        ("/", Interval, t) if t.is_numeric() || *t == Unknown => res("/", interval_div, Interval, Float8, Interval),
        ("+" | "-" | "*" | "/", _, _) => Err(no_operator(op, Some(l), r)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_matches_postgres() {
        let ts = |s: &str| dt::parse_timestamp(s).expect("ts");
        let i = age(ts("2026-01-02 00:00:00"), ts("2026-01-01 00:00:00")).expect("age");
        assert_eq!(dt::format_interval(&i), "1 day");
        let i = age(ts("2024-03-01 00:00:00"), ts("2024-01-31 12:00:00")).expect("age");
        assert_eq!(dt::format_interval(&i), "1 mon 12:00:00");
        let i = age(ts("2026-01-01 00:00:00"), ts("2026-01-02 00:00:00")).expect("age");
        assert_eq!(dt::format_interval(&i), "-1 days");
        let i = age(ts("2001-04-10 00:00:00"), ts("1957-06-13 00:00:00")).expect("age");
        let _ = &i;
        assert_eq!(dt::format_interval(&i), "43 years 9 mons 27 days");
    }

    #[test]
    fn to_char_matches_postgres() {
        let t = |s: &str| tm(dt::parse_timestamptz(s).expect("ts")).expect("tm");
        assert_eq!(format_template(&t("2024-07-05 14:03:09.123456+00"), "YYYY-MM-DD HH24:MI:SS.US"), "2024-07-05 14:03:09.123456");
        assert_eq!(format_template(&t("2024-07-05 14:03:09+00"), "FMDay, FMMonth DDth YYYY HH12:MI AM"), "Friday, July 05th 2024 02:03 PM");
        assert_eq!(
            format_template(&t("2024-01-01 00:00:00+00"), "IYYY-IW ID DDD Q WW CC J RM \"lit\""),
            "2024-01 1 001 1 01 21 2460311 I    lit"
        );
        assert_eq!(format_template(&t("2024-02-29 00:00:00+00"), "Dy Mon DD"), "Thu Feb 29");
        let d = parse_template("05 Jul 2024", "DD Mon YYYY").expect("parse");
        assert_eq!((d.year, d.month, d.day), (2024, 7, 5));
        let p = parse_template("2024-07-05 2:03:09 PM", "YYYY-MM-DD HH:MI:SS AM").expect("parse");
        assert_eq!((p.hour, p.minute, p.second), (14, 3, 9));
    }

    #[test]
    fn zones() {
        assert_eq!(zone_offset("UTC").ok(), Some(0));
        assert_eq!(zone_offset("+05:30").ok(), Some(19800));
        assert_eq!(zone_offset("UTC+3").ok(), Some(-10800));
        assert!(zone_offset("America/New_York").is_err());
    }
}
