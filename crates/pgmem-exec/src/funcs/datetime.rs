//! Date/time functions and operators. The session time zone is UTC.

use super::{Resolved, lenient, no_function, no_operator, resolved, strict};
use crate::cast::{date_to_micros, micros_to_date};
use crate::eval::EvalCtx;
use pgmem_types::calendar::{self, Tm};
use pgmem_types::datetime::{
    self as dt, Interval, TS_INFINITY, TS_NEG_INFINITY, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC, add_interval,
    sub_timestamps,
};
use pgmem_types::zone::{self, Zone};
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
                    Value::Timestamptz(t) => Value::Timestamp(zone::to_local(t)),
                    o => o,
                })
            }),
            vec![],
            Type::Timestamp,
        ),
        "current_date" if n == 0 => resolved(
            lenient("current_date", |a, c| {
                now_fn(a, c).map(|v| match v {
                    Value::Timestamptz(t) => Value::Date(micros_to_date(zone::to_local(t))),
                    o => o,
                })
            }),
            vec![],
            Type::Date,
        ),
        "current_time" if n == 0 => resolved(
            lenient("current_time", |a, c| {
                now_fn(a, c).map(|v| match v {
                    Value::Timestamptz(t) => Value::TimeTz(zone::to_local(t).rem_euclid(USECS_PER_DAY), zone::session_offset(t)),
                    o => o,
                })
            }),
            vec![],
            Type::TimeTz,
        ),
        "localtime" if n == 0 => resolved(
            lenient("localtime", |a, c| {
                now_fn(a, c).map(|v| match v {
                    Value::Timestamptz(t) => Value::Time(zone::to_local(t).rem_euclid(USECS_PER_DAY)),
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
        "date_bin" if n == 3 => {
            let src = match (&args[1], &args[2]) {
                (Type::Timestamp, _) | (_, Type::Timestamp) => Type::Timestamp,
                _ => Type::Timestamptz,
            };
            resolved(strict("date_bin", date_bin_fn), vec![Type::Interval, src.clone(), src.clone()], src)
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
        _ => resolve_constructors(name, args),
    }
}

fn resolve_constructors(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    match name {
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
        "to_number" if n == 2 => resolved(
            strict("to_number", |a, _| {
                Ok(Value::Numeric(super::numfmt::to_number(a[0].as_str().unwrap_or(""), a[1].as_str().unwrap_or(""))?))
            }),
            vec![Type::Text, Type::Text],
            Type::Numeric,
        ),
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
                _ => strict("justify_interval", |a, _| Ok(Value::Interval(justify_interval(iv(&a[0]))))),
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
                        Value::Interval(i) => !i.is_infinite(),
                        _ => true,
                    }))
                }),
                vec![t],
                Type::Bool,
            )
        }
        "timezone" if n == 2 => resolve_at_time_zone(&args[1], &args[0]).map(Some),
        "overlaps" if n == 4 => {
            // Each period is (start, end) or (start, interval); endpoints share one type.
            let ends = [&args[0], &args[2]].into_iter().chain([&args[1], &args[3]].into_iter().filter(|t| **t != Type::Interval));
            let mut t = Type::Unknown;
            for e in ends {
                t = match (&t, e) {
                    (Type::Unknown, e) => e.clone(),
                    (a, Type::Unknown) => a.clone(),
                    (a, e) => super::common_type(a, e).ok_or_else(|| no_function(name, args))?,
                };
            }
            let t = match t {
                Type::Unknown => Type::Timestamptz,
                Type::Date => Type::Timestamp,
                t @ (Type::Timestamp | Type::Timestamptz | Type::Time | Type::TimeTz) => t,
                _ => return Err(no_function(name, args)),
            };
            let end = |a: &Type| if *a == Type::Interval { Type::Interval } else { t.clone() };
            resolved(lenient("overlaps", overlaps_fn), vec![t.clone(), end(&args[1]), t.clone(), end(&args[3])], Type::Bool)
        }
        _ => Ok(None),
    }
}

/// `expr AT TIME ZONE zone` (also `timezone(zone, expr)`). The zone is a name, an
/// abbreviation or a POSIX-style offset (text), or an interval (an offset east of UTC).
pub fn resolve_at_time_zone(src: &Type, zone_ty: &Type) -> SqlResult<Resolved> {
    let zt = if *zone_ty == Type::Interval { Type::Interval } else { Type::Text };
    match src {
        Type::Timestamptz | Type::Unknown => {
            Ok(Resolved { func: strict("timezone", tz_from_timestamptz), args: vec![zt, Type::Timestamptz], ret: Type::Timestamp })
        }
        Type::Timestamp | Type::Date => {
            Ok(Resolved { func: strict("timezone", tz_from_timestamp), args: vec![zt, Type::Timestamp], ret: Type::Timestamptz })
        }
        Type::TimeTz | Type::Time => {
            Ok(Resolved { func: strict("timezone", tz_of_timetz), args: vec![zt, Type::TimeTz], ret: Type::TimeTz })
        }
        other => Err(no_function("timezone", &[zone_ty.clone(), other.clone()])),
    }
}

/// The zone an `AT TIME ZONE` / `timezone()` argument names.
fn zone_arg(v: &Value) -> SqlResult<Zone> {
    match v {
        Value::Interval(i) => {
            if i.months != 0 || i.days != 0 {
                let text = dt::format_interval(i);
                return Err(SqlError::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("interval time zone \"{text}\" must not include months or days"),
                ));
            }
            Ok(Zone::fixed(i32::try_from(i.micros / USECS_PER_SEC).map_err(|_| SqlError::out_of_range("interval"))?))
        }
        other => {
            let name = other.as_str().unwrap_or("");
            Zone::from_spec(name).ok_or_else(|| bad_zone(name))
        }
    }
}

fn bad_zone(zone: &str) -> SqlError {
    SqlError::new(code::INVALID_PARAMETER_VALUE, format!("time zone \"{zone}\" not recognized"))
}

fn tz_from_timestamptz(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let z = zone_arg(&a[0])?;
    match &a[1] {
        Value::Timestamptz(t) if *t == TS_INFINITY || *t == TS_NEG_INFINITY => Ok(Value::Timestamp(*t)),
        Value::Timestamptz(t) => Ok(Value::Timestamp(z.local_of_utc(*t))),
        other => Err(SqlError::internal(format!("timezone() of {}", other.kind()))),
    }
}

fn tz_from_timestamp(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let z = zone_arg(&a[0])?;
    match &a[1] {
        Value::Timestamp(t) if *t == TS_INFINITY || *t == TS_NEG_INFINITY => Ok(Value::Timestamptz(*t)),
        Value::Timestamp(t) => Ok(Value::Timestamptz(z.utc_of_local(*t))),
        other => Err(SqlError::internal(format!("timezone() of {}", other.kind()))),
    }
}

/// `timetz AT TIME ZONE zone`: the same instant with the zone's offset (its current offset for
/// a named zone, as Postgres uses the statement time).
fn tz_of_timetz(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let z = zone_arg(&a[0])?;
    let Value::TimeTz(t, off) = &a[1] else { return Err(SqlError::internal("timezone() of non-timetz")) };
    let new_off = z.offset_at(ctx.env.txn_start);
    let utc = t - i64::from(*off) * USECS_PER_SEC;
    Ok(Value::TimeTz((utc + i64::from(new_off) * USECS_PER_SEC).rem_euclid(USECS_PER_DAY), new_off))
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
    let utc = ts_micros(v).ok_or_else(|| SqlError::internal("extract from non-datetime"))?;
    let is_date = matches!(v, Value::Date(_));
    let is_tz = matches!(v, Value::Timestamptz(_));
    // Fields of a timestamptz are those of its local time in the session zone.
    let micros = if is_tz { zone::to_local(utc) } else { utc };
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
            let unix = utc + dt::PG_EPOCH_UNIX_SECS * USECS_PER_SEC;
            if is_date { Numeric::from_i64(unix / USECS_PER_SEC) } else { micros_numeric(unix, 6) }
        }
        "timezone" | "timezone_hour" | "timezone_minute" if is_tz => {
            let off = i64::from(zone::session_offset(utc));
            Numeric::from_i64(match f {
                "timezone" => off,
                "timezone_hour" => off / 3600,
                _ => (off % 3600) / 60,
            })
        }
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
    // A timestamptz is truncated in its local time (the session zone or the given one).
    let zone = match (a.get(2), &a[1]) {
        (Some(z), _) => Some(zone_arg(z)?),
        (None, Value::Timestamptz(_)) => Some(pgmem_types::context::current().zone),
        _ => None,
    };
    let local = zone.as_ref().map_or(micros, |z| z.local_of_utc(micros));
    let d = tm(local)?;
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
            let ty = if matches!(a[1], Value::Timestamp(_)) { "timestamp without time zone" } else { "timestamp with time zone" };
            return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unit \"{other}\" not recognized for type {ty}")));
        }
    };
    let t = dt::from_tm(&truncated)?;
    let t = zone.as_ref().map_or(t, |z| z.utc_of_local(t));
    Ok(match &a[1] {
        Value::Timestamp(_) => Value::Timestamp(t),
        _ => Value::Timestamptz(t),
    })
}

/// `date_bin(stride, source, origin)`: the start of the stride-sized bin containing `source`,
/// counting from `origin` (Postgres 14+).
fn date_bin_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let stride = iv(&a[0]);
    if stride.months != 0 {
        return Err(SqlError::feature_not_supported("timestamps cannot be binned into intervals containing months or years"));
    }
    let stride_us = i64::from(stride.days) * USECS_PER_DAY + stride.micros;
    if stride_us <= 0 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "stride must be greater than zero"));
    }
    let (Some(src), Some(origin)) = (ts_micros(&a[1]), ts_micros(&a[2])) else { return Err(SqlError::internal("date_bin arguments")) };
    if [src, origin].iter().any(|t| *t == TS_INFINITY || *t == TS_NEG_INFINITY) {
        return Err(SqlError::feature_not_supported("timestamps cannot be binned into infinite intervals"));
    }
    let diff = src - origin;
    let mut delta = diff - diff % stride_us;
    if diff < 0 && diff % stride_us != 0 {
        delta -= stride_us;
    }
    let out = origin + delta;
    Ok(if matches!(a[1], Value::Timestamp(_)) { Value::Timestamp(out) } else { Value::Timestamptz(out) })
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

/// Wall-clock microseconds of a timestamp-ish value (timestamptz in the session zone).
fn local_micros(v: &Value) -> Option<i64> {
    match v {
        Value::Timestamptz(t) => Some(zone::to_local(*t)),
        other => ts_micros(other),
    }
}

fn age2_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (Some(x), Some(y)) = (local_micros(&a[0]), local_micros(&a[1])) else { return Err(SqlError::internal("age of non-timestamp")) };
    Ok(Value::Interval(age(x, y)?))
}

/// `age(ts)`: from midnight of the current date.
fn age1_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let today = date_to_micros(micros_to_date(zone::to_local(ctx.env.txn_start)));
    let x = local_micros(&a[0]).ok_or_else(|| SqlError::internal("age of non-timestamp"))?;
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
    let local = make_ts(a)?;
    Ok(Value::Timestamptz(match a.get(6) {
        // make_timestamptz reads a numeric zone the ISO way (`+02` is east of UTC).
        Some(Value::Text(z)) if z.trim_start().starts_with(['+', '-']) => {
            let east = pgmem_types::zone::Zone::from_spec(&format!("UTC{}", flip_sign(z.trim()))).ok_or_else(|| bad_zone(z))?;
            east.utc_of_local(local)
        }
        Some(z) => zone_arg(z)?.utc_of_local(local),
        None => zone::from_local(local),
    }))
}

/// `+02:00` → `-02:00`: turns an ISO offset into the POSIX spelling of the same zone.
fn flip_sign(z: &str) -> String {
    match z.strip_prefix('+') {
        Some(rest) => format!("-{rest}"),
        None => format!("+{}", z.trim_start_matches('-')),
    }
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
    format_template_tz(t, fmt, None)
}

/// `to_char` of broken-down time `t`; `tz` is the (offset east, abbreviation) for a
/// timestamptz, whose TZ / TZH / TZM / OF patterns print it.
fn format_template_tz(t: &Tm, fmt: &str, tz: Option<(i32, String)>) -> String {
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
            "TZ" => {
                let abbrev = tz.as_ref().map(|(_, a)| a.clone()).unwrap_or_default();
                (if upper { abbrev } else { abbrev.to_lowercase() }, None)
            }
            "TZH" => {
                let off = tz.as_ref().map_or(0, |(o, _)| *o);
                (format!("{}{:02}", if off < 0 { '-' } else { '+' }, off.unsigned_abs() / 3600), None)
            }
            "TZM" => (format!("{:02}", tz.as_ref().map_or(0, |(o, _)| o.unsigned_abs() % 3600 / 60)), None),
            "OF" => (pgmem_types::zone::format_offset(tz.as_ref().map_or(0, |(o, _)| *o)), None),
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
        Value::Timestamp(t) => Ok(Value::Text(format_template(&tm(*t)?, fmt))),
        Value::Timestamptz(t) => {
            let info = pgmem_types::context::with_zone(|z| (z.offset_at(*t), z.abbrev_at(*t)));
            Ok(Value::Text(format_template_tz(&tm(zone::to_local(*t))?, fmt, Some(info))))
        }
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
        v => Ok(Value::Text(super::numfmt::to_char(v, fmt)?)),
    }
}

/// Template patterns that read input, longest first (`DCH_keywords` order matters for prefixes).
const INPUT_PATTERNS: &[&str] = &[
    "A.D.", "A.M.", "B.C.", "P.M.", "Y,YYY", "HH24", "HH12", "IDDD", "IYYY", "MONTH", "SSSSS", "SSSS", "YYYY", "DDD", "IYY", "MON", "TZH",
    "TZM", "YYY", "DAY", "FF1", "FF2", "FF3", "FF4", "FF5", "FF6", "AD", "AM", "BC", "CC", "DD", "DY", "HH", "ID", "IW", "IY", "MI", "MM",
    "MS", "OF", "PM", "RM", "SS", "TZ", "US", "WW", "YY", "D", "I", "J", "Q", "W", "Y",
];

/// Fields collected by `DCH_from_char`.
#[derive(Default)]
struct FromChar {
    year: Option<i64>,
    year_digits: usize,
    cc: Option<i64>,
    month: Option<i64>,
    day: Option<i64>,
    doy: Option<i64>,
    ww: Option<i64>,
    julian: Option<i64>,
    hour: i64,
    clock12: bool,
    minute: i64,
    second: i64,
    micros: i64,
    ssss: Option<i64>,
    pm: Option<bool>,
    bc: bool,
    tzh: Option<i64>,
    tzm: Option<i64>,
    tz: Option<i32>,
}

/// Parses `src` according to a Postgres template: the broken-down local time and the zone
/// offset (seconds east) when the template has one.
fn parse_template(src: &str, fmt: &str) -> SqlResult<(Tm, Option<i32>)> {
    let mut fc = FromChar::default();
    let mut s = src;
    let mut f = fmt;
    while !f.is_empty() {
        let mut fill = false;
        loop {
            if let Some(r) = f.strip_prefix("FM").or_else(|| f.strip_prefix("fm")) {
                f = r;
                fill = true;
            } else if let Some(r) = f.strip_prefix("FX").or_else(|| f.strip_prefix("fx")) {
                f = r;
            } else {
                break;
            }
        }
        let upper = f.to_ascii_uppercase();
        let Some(pat) = INPUT_PATTERNS.iter().find(|p| upper.starts_with(**p)) else {
            let c = f.chars().next().unwrap_or(' ');
            f = &f[c.len_utf8()..];
            if c == '"' {
                // Quoted literal: skip as many input characters.
                let end = f.find('"').unwrap_or(f.len());
                let n = f[..end].chars().count();
                f = f.get(end + 1..).unwrap_or("");
                s = s.char_indices().nth(n).map_or("", |(i, _)| &s[i..]);
                continue;
            }
            // A template separator matches any run of non-alphanumeric input characters.
            if c.is_alphanumeric() {
                s = s.char_indices().nth(1).map_or("", |(i, _)| &s[i..]);
            } else {
                s = s.trim_start_matches(|ch: char| !ch.is_alphanumeric() && ch != '-' && ch != '+');
                if !c.is_whitespace() {
                    s = s.strip_prefix(['-', '+']).unwrap_or(s);
                }
            }
            continue;
        };
        f = &f[pat.len()..];
        // An ordinal suffix after a number pattern.
        let th = f.starts_with("TH") || f.starts_with("th");
        if th {
            f = &f[2..];
        }
        // Variable-width fields: before a separator (or the end), or in fill mode, read every digit.
        let next_sep = fill || th || f.is_empty() || f.chars().next().is_some_and(|c| !c.is_alphanumeric()) || !next_is_digit_pattern(f);
        s = s.trim_start_matches(' ');
        let field = PatField { name: pat, next_sep };
        read_pattern(&mut fc, pat, &mut s, &field, src, fmt)?;
        if th {
            let ord = s.len().min(2);
            if s[..ord].chars().all(|c| c.is_ascii_alphabetic()) {
                s = &s[ord..];
            }
        }
    }
    build_from_char(&fc, src)
}

/// Reads one template field's input into `fc` (`DCH_from_char`'s per-node actions).
fn read_pattern(fc: &mut FromChar, pat: &&str, input: &mut &str, field: &PatField, src: &str, fmt: &str) -> SqlResult<()> {
    let bad = || SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid value \"{src}\" for \"{fmt}\""));
    let mut s = *input;
    let num = |s: &mut &str, len: usize, signed: bool| field.read(s, len, signed).map(|(v, _)| v);
    match *pat {
        "YYYY" | "IYYY" => {
            let before = s.len();
            fc.year = Some(num(&mut s, 4, false)?);
            fc.year_digits = before - s.len();
        }
        "Y,YYY" => {
            let hi = num(&mut s, 1, false)?;
            s = s.strip_prefix(',').ok_or_else(bad)?;
            let lo = num(&mut s, 3, false)?;
            fc.year = Some(hi * 1000 + lo);
            fc.year_digits = 4;
        }
        "YYY" | "IYY" => {
            let before = s.len();
            fc.year = Some(num(&mut s, 3, false)?);
            fc.year_digits = before - s.len();
        }
        "YY" | "IY" => {
            let before = s.len();
            fc.year = Some(num(&mut s, 2, false)?);
            fc.year_digits = before - s.len();
        }
        "Y" | "I" => {
            let before = s.len();
            fc.year = Some(num(&mut s, 1, false)?);
            fc.year_digits = before - s.len();
        }
        "CC" => fc.cc = Some(num(&mut s, 2, false)?),
        "MM" => fc.month = Some(num(&mut s, 2, false)?),
        "DD" => fc.day = Some(num(&mut s, 2, false)?),
        "DDD" | "IDDD" => fc.doy = Some(num(&mut s, 3, false)?),
        "WW" | "IW" => fc.ww = Some(num(&mut s, 2, false)?),
        "D" | "ID" | "Q" | "W" => {
            num(&mut s, 1, false)?;
        }
        "J" => fc.julian = Some(num(&mut s, 7, false)?),
        "HH" | "HH12" => {
            fc.hour = num(&mut s, 2, false)?;
            fc.clock12 = true;
        }
        "HH24" => fc.hour = num(&mut s, 2, false)?,
        "MI" => fc.minute = num(&mut s, 2, false)?,
        "SS" => fc.second = num(&mut s, 2, false)?,
        "SSSS" | "SSSSS" => fc.ssss = Some(num(&mut s, 5, false)?),
        "MS" => fc.micros = num(&mut s, 3, false)? * 1000,
        "US" => fc.micros = num(&mut s, 6, false)?,
        "FF1" | "FF2" | "FF3" | "FF4" | "FF5" | "FF6" => {
            // The fraction scales by the digits actually read.
            let n = usize::from(pat.as_bytes()[2] - b'0');
            let (v, digits) = field.read(&mut s, n, false)?;
            fc.micros = v * 10i64.pow(6u32.saturating_sub(u32::try_from(digits).unwrap_or(6)));
        }
        "TZH" => fc.tzh = Some(num(&mut s, 2, true)?),
        "TZM" => fc.tzm = Some(num(&mut s, 2, false)?),
        "OF" => {
            let h = num(&mut s, 2, true)?;
            let m = if let Some(r) = s.strip_prefix(':') {
                s = r;
                num(&mut s, 2, false)?
            } else {
                0
            };
            fc.tzh = Some(h);
            fc.tzm = Some(m);
        }
        "TZ" => {
            let len = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
            let (secs, _) = pgmem_types::zone::abbreviation(&s[..len]).ok_or_else(bad)?;
            fc.tz = Some(secs);
            s = &s[len..];
        }
        "AM" | "PM" | "A.M." | "P.M." => {
            let up = s.to_ascii_uppercase();
            let (is_pm, len) = if up.starts_with("A.M.") {
                (false, 4)
            } else if up.starts_with("P.M.") {
                (true, 4)
            } else if up.starts_with("AM") {
                (false, 2)
            } else if up.starts_with("PM") {
                (true, 2)
            } else {
                return Err(bad());
            };
            fc.pm = Some(is_pm);
            fc.clock12 = true;
            s = &s[len..];
        }
        "AD" | "BC" | "A.D." | "B.C." => {
            let up = s.to_ascii_uppercase();
            let (bc, len) = if up.starts_with("B.C.") {
                (true, 4)
            } else if up.starts_with("A.D.") {
                (false, 4)
            } else if up.starts_with("BC") {
                (true, 2)
            } else if up.starts_with("AD") {
                (false, 2)
            } else {
                return Err(bad());
            };
            fc.bc = bc;
            s = &s[len..];
        }
        "MONTH" | "MON" => {
            let idx = MONTHS
                .iter()
                .position(|m| s.len() >= 3 && s.is_char_boundary(3) && s[..3].eq_ignore_ascii_case(&m[..3]))
                .ok_or_else(bad)?;
            let full = MONTHS[idx];
            s = if *pat == "MONTH" && s.len() >= full.len() && s.is_char_boundary(full.len()) && s[..full.len()].eq_ignore_ascii_case(full)
            {
                &s[full.len()..]
            } else {
                &s[3..]
            };
            fc.month = Some(i64::try_from(idx + 1).unwrap_or(1));
        }
        "RM" => {
            let up = s.to_ascii_uppercase();
            let (idx, r) =
                ROMAN.iter().enumerate().rev().filter(|(_, r)| up.starts_with(**r)).max_by_key(|(_, r)| r.len()).ok_or_else(bad)?;
            fc.month = Some(i64::try_from(idx + 1).unwrap_or(1));
            s = &s[r.len()..];
        }
        "DAY" | "DY" => {
            let idx =
                DAYS.iter().position(|d| s.len() >= 3 && s.is_char_boundary(3) && s[..3].eq_ignore_ascii_case(&d[..3])).ok_or_else(bad)?;
            let full = DAYS[idx];
            s = if *pat == "DAY" && s.len() >= full.len() && s.is_char_boundary(full.len()) && s[..full.len()].eq_ignore_ascii_case(full) {
                &s[full.len()..]
            } else {
                &s[3..]
            };
        }
        _ => return Err(SqlError::feature_not_supported(format!("template pattern \"{pat}\" is not supported by pgmem for parsing"))),
    }
    *input = s;
    Ok(())
}

/// Builds the timestamp from the collected fields (`do_to_timestamp`).
fn build_from_char(fc: &FromChar, src: &str) -> SqlResult<(Tm, Option<i32>)> {
    let range = || SqlError::new(code::DATETIME_FIELD_OVERFLOW, format!("date/time field value out of range: \"{src}\""));
    let mut year = match (fc.year, fc.cc) {
        (Some(y), cc) => {
            // Partial years are adjusted toward 2020 (`adjust_partial_year_to_2020`).
            match (fc.year_digits, cc) {
                (1, _) => y + 2000,
                (2, _) if y < 70 => y + 2000,
                (2, _) => y + 1900,
                (3, _) if y < 520 => y + 2000,
                (3, _) => y + 1000,
                _ => y,
            }
        }
        (None, Some(cc)) => (cc - 1) * 100 + 1,
        // Without a year field the year is 0 (1 BC), as in Postgres.
        (None, None) => 0,
    };
    if fc.bc && fc.year.is_some() {
        if year <= 0 {
            return Err(range());
        }
        year = -(year - 1);
    }
    let year32 = i32::try_from(year).map_err(|_| range())?;
    let (mut y, mut m, mut d) = (year32, fc.month.unwrap_or(1), fc.day.unwrap_or(1));
    if let Some(j) = fc.julian {
        let (jy, jm, jd) = calendar::ymd_from_days(j - 2_451_545);
        (y, m, d) = (jy, i64::from(jm), i64::from(jd));
    } else if fc.month.is_none() && fc.day.is_none() {
        let day_of_year = fc.doy.or_else(|| fc.ww.map(|w| (w - 1) * 7 + 1));
        if let Some(doy) = day_of_year {
            let (jy, jm, jd) = calendar::ymd_from_days(calendar::days_from_ymd(y, 1, 1) + doy - 1);
            (y, m, d) = (jy, i64::from(jm), i64::from(jd));
        }
    }
    let (m, d) = (u32::try_from(m).map_err(|_| range())?, u32::try_from(d).map_err(|_| range())?);
    if !calendar::valid_date(y, m, d) {
        return Err(range());
    }
    let mut hour = fc.hour;
    if fc.clock12 {
        if !(1..=12).contains(&hour) {
            return Err(SqlError::new(code::INVALID_DATETIME_FORMAT, format!("hour \"{hour}\" is invalid for the 12-hour clock"))
                .with_hint("Use the 24-hour clock, or give an hour between 1 and 12."));
        }
        match fc.pm {
            Some(true) if hour < 12 => hour += 12,
            Some(false) if hour == 12 => hour = 0,
            _ => {}
        }
    }
    let (mut minute, mut second) = (fc.minute, fc.second);
    if let Some(ssss) = fc.ssss {
        hour = ssss / 3600;
        minute = ssss / 60 % 60;
        second = ssss % 60;
    }
    if hour > 24 || minute > 59 || second > 60 {
        return Err(range());
    }
    let t = Tm {
        year: y,
        month: m,
        day: d,
        hour: u32::try_from(hour).map_err(|_| range())?,
        minute: u32::try_from(minute).map_err(|_| range())?,
        second: u32::try_from(second).map_err(|_| range())?,
        micros: u32::try_from(fc.micros).map_err(|_| range())?,
    };
    let zone = match (fc.tz, fc.tzh) {
        (Some(z), _) => Some(z),
        (None, Some(h)) => {
            let mins = fc.tzm.unwrap_or(0);
            let secs = i32::try_from(h.abs() * 3600 + mins * 60).map_err(|_| range())?;
            Some(if h < 0 || (h == 0 && src.contains("-0")) { -secs } else { secs })
        }
        (None, None) => None,
    };
    Ok((t, zone))
}

/// A numeric template field being read (`from_char_parse_int_len`).
struct PatField<'a> {
    name: &'a str,
    /// Followed by a separator (or the end, or in fill mode): read every digit.
    next_sep: bool,
}

impl PatField<'_> {
    /// Reads the field's number: (value, digits read).
    fn read(&self, s: &mut &str, len: usize, signed: bool) -> SqlResult<(i64, usize)> {
        let b = s.as_bytes();
        let mut end = usize::from(signed && matches!(b.first(), Some(b'-' | b'+')));
        let start = end;
        while end < b.len() && b[end].is_ascii_digit() && (self.next_sep || end - start < len) {
            end += 1;
        }
        let shown: String = s.chars().take(len).collect();
        if end == start {
            return Err(SqlError::new(code::INVALID_DATETIME_FORMAT, format!("invalid value \"{shown}\" for \"{}\"", self.name))
                .with_detail("Value must be an integer."));
        }
        if !self.next_sep && end - start < len {
            let remain = s.chars().count();
            return Err(SqlError::new(
                code::INVALID_DATETIME_FORMAT,
                format!("source string too short for \"{}\" formatting field", self.name),
            )
            .with_detail(format!("Field requires {len} characters, but only {remain} remain."))
            .with_hint("If your source string is not fixed-width, try using the \"FM\" modifier."));
        }
        let v = s[..end].parse::<i64>().map_err(|_| SqlError::new(code::DATETIME_FIELD_OVERFLOW, "value for field is out of range"))?;
        *s = &s[end..];
        Ok((v, end - start))
    }
}

/// Whether the template continues with a digit-reading pattern (no separator between).
fn next_is_digit_pattern(f: &str) -> bool {
    let up = f.trim_start_matches("FM").to_ascii_uppercase();
    INPUT_PATTERNS.iter().find(|p| up.starts_with(**p)).is_some_and(|p| {
        !matches!(*p, "MONTH" | "MON" | "DAY" | "DY" | "AM" | "PM" | "A.M." | "P.M." | "AD" | "BC" | "A.D." | "B.C." | "TZ" | "RM")
    })
}

fn to_timestamp_fmt_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (t, z) = parse_template(a[0].as_str().unwrap_or(""), a[1].as_str().unwrap_or(""))?;
    let local = dt::from_tm(&t)?;
    // Fields without a zone in the template are local time in the session zone.
    Ok(Value::Timestamptz(match z {
        Some(off) => local - i64::from(off) * pgmem_types::datetime::USECS_PER_SEC,
        None => zone::from_local(local),
    }))
}

fn to_date_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (t, _) = parse_template(a[0].as_str().unwrap_or(""), a[1].as_str().unwrap_or(""))?;
    Ok(Value::Date(dt::date_from_ymd(t.year, t.month, t.day)?))
}

/// `interval_justify_interval`.
fn justify_interval(i: Interval) -> Interval {
    if i.is_infinite() {
        return i;
    }
    let mut out = i;
    out.days += i32::try_from(out.micros / USECS_PER_DAY).unwrap_or(0);
    out.micros %= USECS_PER_DAY;
    out.months += out.days / 30;
    out.days %= 30;
    if out.months > 0 && (out.days < 0 || (out.days == 0 && out.micros < 0)) {
        out.days += 30;
        out.months -= 1;
    } else if out.months < 0 && (out.days > 0 || (out.days == 0 && out.micros > 0)) {
        out.days -= 30;
        out.months += 1;
    }
    if out.days > 0 && out.micros < 0 {
        out.micros += USECS_PER_DAY;
        out.days -= 1;
    } else if out.days < 0 && out.micros > 0 {
        out.micros -= USECS_PER_DAY;
        out.days += 1;
    }
    out
}

fn justify_hours(i: Interval) -> Interval {
    if i.is_infinite() {
        return i;
    }
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
    if i.is_infinite() {
        return i;
    }
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
        Value::Timestamptz(t) => Value::Timestamptz(tstz_add_interval(*t, &i)?),
        Value::Timestamp(t) => Value::Timestamp(add_interval(*t, &i)?),
        Value::Date(d) => Value::Timestamp(add_interval(date_to_micros(*d), &i)?),
        Value::Time(t) => Value::Time((t + i.micros).rem_euclid(USECS_PER_DAY)),
        other => return Err(SqlError::internal(format!("{} + interval", other.kind()))),
    })
}

/// `timestamptz + interval`: months and days move the local (session zone) wall-clock time,
/// the time part is absolute, as in Postgres' `timestamptz_pl_interval`.
pub fn tstz_add_interval(t: i64, i: &Interval) -> SqlResult<i64> {
    if i.is_infinite() {
        return dt::infinite_shift(t, i);
    }
    if t == TS_INFINITY || t == TS_NEG_INFINITY {
        return Ok(t);
    }
    let mut out = t;
    if i.months != 0 || i.days != 0 {
        let local = add_interval(zone::to_local(t), &Interval { months: i.months, days: i.days, micros: 0 })?;
        out = zone::from_local(local);
    }
    out.checked_add(i.micros).ok_or_else(|| SqlError::new(code::DATETIME_FIELD_OVERFLOW, "timestamp out of range"))
}

/// `overlaps_timestamp`, with Postgres' handling of NULL endpoints.
fn overlaps_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let end = |s: &Value, e: &Value| -> SqlResult<Value> {
        match (s, e) {
            (s, Value::Interval(_)) if !s.is_null() => ts_plus_interval(&[s.clone(), e.clone()], ctx),
            (_, Value::Interval(_)) => Ok(Value::Null),
            _ => Ok(e.clone()),
        }
    };
    let gt = |x: &Value, y: &Value| -> SqlResult<bool> { Ok(pgmem_types::compare_values(x, y)?.is_gt()) };
    let lt = |x: &Value, y: &Value| -> SqlResult<bool> { Ok(pgmem_types::compare_values(x, y)?.is_lt()) };
    // Order each period's endpoints; a single NULL endpoint becomes the end.
    let norm = |s: Value, e: Value| -> SqlResult<Option<(Value, Value)>> {
        Ok(match (s.is_null(), e.is_null()) {
            (true, true) => None,
            (true, false) => Some((e, Value::Null)),
            (false, true) => Some((s, Value::Null)),
            (false, false) if gt(&s, &e)? => Some((e, s)),
            _ => Some((s, e)),
        })
    };
    let (Some((ts1, te1)), Some((ts2, te2))) = (norm(a[0].clone(), end(&a[0], &a[1])?)?, norm(a[2].clone(), end(&a[2], &a[3])?)?) else {
        return Ok(Value::Null);
    };
    let (n1, n2) = (te1.is_null(), te2.is_null());
    Ok(if gt(&ts1, &ts2)? {
        if n2 {
            Value::Null
        } else if lt(&ts1, &te2)? {
            Value::Bool(true)
        } else if n1 {
            Value::Null
        } else {
            Value::Bool(false)
        }
    } else if lt(&ts1, &ts2)? {
        if n1 {
            Value::Null
        } else if lt(&ts2, &te1)? {
            Value::Bool(true)
        } else if n2 {
            Value::Null
        } else {
            Value::Bool(false)
        }
    } else if n1 || n2 {
        Value::Null
    } else {
        Value::Bool(true)
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
        let (d, _) = parse_template("05 Jul 2024", "DD Mon YYYY").expect("parse");
        assert_eq!((d.year, d.month, d.day), (2024, 7, 5));
        let (p, _) = parse_template("2024-07-05 2:03:09 PM", "YYYY-MM-DD HH:MI:SS AM").expect("parse");
        assert_eq!((p.hour, p.minute, p.second), (14, 3, 9));
    }

    #[test]
    fn zones() {
        let off = |s: &str| zone_arg(&Value::Text(s.into())).map(|z| z.offset_at(0)).ok();
        assert_eq!(off("UTC"), Some(0));
        // POSIX convention: a bare positive offset is west of Greenwich.
        assert_eq!(off("+05:30"), Some(-19800));
        assert_eq!(off("UTC+3"), Some(-10800));
        assert_eq!(off("EST"), Some(-18000));
        assert!(off("Nowhere/Land").is_none());
    }
}
