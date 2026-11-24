//! Session, system-information, sequence and UUID functions.

use super::{Resolved, lenient, no_function, resolved, strict};
use crate::eval::EvalCtx;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};

/// UUID, version and configuration functions.
fn resolve_uuid_settings(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let n = args.len();
    match name {
        "gen_random_uuid" | "uuid_generate_v4" | "uuidv4" if n == 0 => {
            resolved(lenient("gen_random_uuid", gen_random_uuid), vec![], Type::Uuid)
        }
        "uuidv7" if n == 0 => resolved(lenient("uuidv7", uuidv7), vec![], Type::Uuid),
        "uuidv7" if n == 1 => resolved(lenient("uuidv7", uuidv7), vec![Type::Interval], Type::Uuid),
        "uuid_extract_version" if n == 1 => resolved(strict("uuid_extract_version", uuid_extract_version), vec![Type::Uuid], Type::Int2),
        "uuid_extract_timestamp" if n == 1 => {
            resolved(strict("uuid_extract_timestamp", uuid_extract_timestamp), vec![Type::Uuid], Type::Timestamptz)
        }
        "version" if n == 0 => {
            resolved(lenient("version", |_, _| Ok(Value::Text(crate::session::VERSION_STRING.to_owned()))), vec![], Type::Text)
        }
        "current_setting" if n == 1 || n == 2 => {
            let mut t = vec![Type::Text];
            if n == 2 {
                t.push(Type::Bool);
            }
            resolved(strict("current_setting", current_setting), t, Type::Text)
        }
        "set_config" if n == 3 => resolved(lenient("set_config", set_config), vec![Type::Text, Type::Text, Type::Bool], Type::Text),
        _ => Ok(None),
    }
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    if let Some(r) = resolve_uuid_settings(name, args)? {
        return Ok(Some(r));
    }
    let err = || no_function(name, args);
    let n = args.len();
    match name {
        "current_user" | "session_user" | "user" | "current_role" if n == 0 => {
            resolved(lenient("current_user", |_, c| Ok(Value::Text(c.env.user.clone()))), vec![], Type::Name)
        }
        "current_database" | "current_catalog" if n == 0 => {
            resolved(lenient("current_database", |_, c| Ok(Value::Text(c.env.database.clone()))), vec![], Type::Name)
        }
        "current_schema" if n == 0 => resolved(
            lenient("current_schema", |_, c| {
                Ok(c.txn.catalog().explicit_search_path().into_iter().next().map_or(Value::Null, Value::Text))
            }),
            vec![],
            Type::Name,
        ),
        "current_schemas" if n == 1 => resolved(
            strict("current_schemas", |a, c| {
                let catalog = c.txn.catalog();
                let names: Vec<String> = if a[0].as_bool() == Some(true) {
                    catalog
                        .search_path()
                        .into_iter()
                        .map(|e| match e {
                            pgmem_catalog::PathEntry::System => "pg_catalog".to_owned(),
                            pgmem_catalog::PathEntry::Schema(s) => s,
                        })
                        .collect()
                } else {
                    catalog.explicit_search_path()
                };
                Ok(Value::Array(names.into_iter().map(Value::Text).collect()))
            }),
            vec![Type::Bool],
            Type::array_of(Type::Name),
        ),
        "pg_my_temp_schema" if n == 0 => resolved(
            lenient("pg_my_temp_schema", |_, c| {
                let temp = format!("pg_temp_{}", c.env.backend_pid);
                Ok(Value::Oid(c.txn.catalog().namespace_oid(&temp).unwrap_or(0)))
            }),
            vec![],
            Type::Oid,
        ),
        "pg_is_other_temp_schema" if n == 1 => resolved(
            strict("pg_is_other_temp_schema", |a, c| {
                let mine = format!("pg_temp_{}", c.env.backend_pid);
                let oid = match &a[0] {
                    Value::Oid(o) | Value::Reg(_, o, _) => *o,
                    other => u32::try_from(other.as_i64().unwrap_or(0)).unwrap_or(0),
                };
                let other = c.txn.catalog().schema_by_oid(oid).is_some_and(|s| s.name.starts_with("pg_temp_") && s.name != mine);
                Ok(Value::Bool(other))
            }),
            vec![Type::Oid],
            Type::Bool,
        ),
        "pg_backend_pid" if n == 0 => resolved(lenient("pg_backend_pid", |_, c| Ok(Value::Int4(c.env.backend_pid))), vec![], Type::Int4),
        "nextval" if n == 1 => resolved(strict("nextval", nextval), vec![Type::Text], Type::Int8),
        "currval" if n == 1 => resolved(strict("currval", currval), vec![Type::Text], Type::Int8),
        "lastval" if n == 0 => resolved(lenient("lastval", lastval), vec![], Type::Int8),
        "setval" if n == 2 || n == 3 => {
            let mut t = vec![Type::Text, Type::Int8];
            if n == 3 {
                t.push(Type::Bool);
            }
            resolved(strict("setval", setval), t, Type::Int8)
        }
        "num_nulls" | "num_nonnulls" => {
            let targets = args.iter().map(|t| if *t == Type::Unknown { Type::Text } else { t.clone() }).collect();
            let f = if name == "num_nulls" {
                lenient("num_nulls", |a, _| Ok(Value::Int4(i32::try_from(a.iter().filter(|v| v.is_null()).count()).unwrap_or(0))))
            } else {
                lenient("num_nonnulls", |a, _| Ok(Value::Int4(i32::try_from(a.iter().filter(|v| !v.is_null()).count()).unwrap_or(0))))
            };
            resolved(f, targets, Type::Int4)
        }
        // Advisory locks: pgmem has no lock manager, so they always succeed immediately.
        "pg_advisory_lock" | "pg_advisory_xact_lock" | "pg_advisory_lock_shared" | "pg_advisory_xact_lock_shared" => {
            resolved(strict(name_static(name), |_, _| Ok(Value::Text(String::new()))), vec![Type::Int8; n.max(1)], Type::Void)
        }
        "pg_try_advisory_lock"
        | "pg_try_advisory_xact_lock"
        | "pg_advisory_unlock"
        | "pg_try_advisory_lock_shared"
        | "pg_advisory_unlock_shared" => {
            resolved(strict(name_static(name), |_, _| Ok(Value::Bool(true))), vec![Type::Int8; n.max(1)], Type::Bool)
        }
        "pg_advisory_unlock_all" if n == 0 => {
            resolved(lenient("pg_advisory_unlock_all", |_, _| Ok(Value::Text(String::new()))), vec![], Type::Void)
        }
        "pg_sleep" if n == 1 => resolved(strict("pg_sleep", |_, _| Ok(Value::Text(String::new()))), vec![Type::Float8], Type::Void),
        "txid_current" | "pg_current_xact_id" if n == 0 => {
            resolved(lenient("txid_current", |_, c| Ok(Value::Int8(c.env.txid))), vec![], Type::Int8)
        }
        "pg_typeof" if n == 1 => {
            let name = match &args[0] {
                Type::Unknown => "unknown".to_owned(),
                t => t.name(),
            };
            let _ = err;
            resolved(lenient("pg_typeof", |a, _| Ok(a[0].clone())), vec![Type::Text], Type::Text).map(|r| {
                r.map(|mut r| {
                    r.func = lenient("pg_typeof", |a, _| Ok(a[0].clone()));
                    r.args = vec![Type::Text];
                    r.ret = Type::Text;
                    // The binder replaces the argument with the type name constant.
                    r.func.name = "pg_typeof";
                    let _ = &name;
                    r
                })
            })
        }
        "pg_column_size" if n == 1 => {
            let t = if args[0] == Type::Unknown { Type::Text } else { args[0].clone() };
            resolved(strict("pg_column_size", pg_column_size), vec![t], Type::Int4)
        }
        _ => Ok(None),
    }
}

/// `pg_column_size`: the datum's size as Postgres stores it in memory (fixed-width types, or
/// a 4-byte varlena header plus the data).
fn pg_column_size(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let n = match &a[0] {
        Value::Bool(_) => 1,
        Value::Int2(_) => 2,
        Value::Int4(_) | Value::Float4(_) | Value::Date(_) | Value::Oid(_) => 4,
        Value::Int8(_) | Value::Float8(_) | Value::Timestamp(_) | Value::Timestamptz(_) | Value::Time(_) => 8,
        Value::TimeTz(..) => 12,
        Value::Interval(_) | Value::Uuid(_) => 16,
        Value::Text(s) | Value::Json(s) => s.len() + 4,
        Value::Bytea(b) => b.len() + 4,
        // Short numeric: varlena header, 2-byte header, 2 bytes per base-10000 digit.
        Value::Numeric(n) => n.to_binary().len().saturating_sub(2),
        other => pgmem_types::text::encode(other, &other.natural_type())?.len() + 4,
    };
    Ok(Value::Int4(i32::try_from(n).unwrap_or(i32::MAX)))
}

fn name_static(name: &str) -> &'static str {
    match name {
        "pg_advisory_lock" => "pg_advisory_lock",
        "pg_advisory_xact_lock" => "pg_advisory_xact_lock",
        "pg_advisory_lock_shared" => "pg_advisory_lock_shared",
        "pg_advisory_xact_lock_shared" => "pg_advisory_xact_lock_shared",
        "pg_try_advisory_lock" => "pg_try_advisory_lock",
        "pg_try_advisory_xact_lock" => "pg_try_advisory_xact_lock",
        "pg_advisory_unlock" => "pg_advisory_unlock",
        "pg_try_advisory_lock_shared" => "pg_try_advisory_lock_shared",
        _ => "pg_advisory_unlock_shared",
    }
}

fn gen_random_uuid(_: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| SqlError::internal(format!("gen_random_uuid: {e}")))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(Value::Uuid(b))
}

/// Microseconds between the Unix epoch and 2000-01-01 (Postgres' timestamp epoch).
const UNIX_TO_PG_MICROS: i64 = 946_684_800_000_000;

/// `uuidv7([shift interval])`: a 48-bit Unix millisecond timestamp from the server clock, then
/// 12 bits of sub-millisecond precision (as Postgres 18 fills `rand_a`), then random bits.
fn uuidv7(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let mut now = ctx.env.clock.now_micros();
    if let Some(Value::Interval(iv)) = a.first() {
        now = super::datetime::tstz_add_interval(now, iv)?;
    }
    let unix_us = now + UNIX_TO_PG_MICROS;
    let ms = unix_us.div_euclid(1000);
    let sub_us = unix_us.rem_euclid(1000);
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|e| SqlError::internal(format!("uuidv7: {e}")))?;
    let ms_bytes = ms.to_be_bytes();
    b[..6].copy_from_slice(&ms_bytes[2..8]);
    // Sub-millisecond fraction scaled to 12 bits.
    let frac = u16::try_from(sub_us * 4096 / 1000).unwrap_or(0);
    b[6] = 0x70 | u8::try_from(frac >> 8).unwrap_or(0);
    b[7] = u8::try_from(frac & 0xff).unwrap_or(0);
    b[8] = (b[8] & 0x3f) | 0x80;
    Ok(Value::Uuid(b))
}

fn uuid_bytes(v: &Value) -> [u8; 16] {
    match v {
        Value::Uuid(b) => *b,
        _ => [0; 16],
    }
}

/// The version of an RFC 9562 (variant 10) UUID, else NULL.
fn uuid_extract_version(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let b = uuid_bytes(&a[0]);
    if b[8] & 0xc0 != 0x80 {
        return Ok(Value::Null);
    }
    Ok(Value::Int2(i16::from(b[6] >> 4)))
}

/// The timestamp of a version 1 or 7 UUID, else NULL.
fn uuid_extract_timestamp(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let b = uuid_bytes(&a[0]);
    if b[8] & 0xc0 != 0x80 {
        return Ok(Value::Null);
    }
    match b[6] >> 4 {
        7 => {
            let mut ms = [0u8; 8];
            ms[2..].copy_from_slice(&b[..6]);
            let unix_ms = i64::from_be_bytes(ms);
            Ok(Value::Timestamptz(unix_ms * 1000 - UNIX_TO_PG_MICROS))
        }
        1 => {
            // 60-bit count of 100ns intervals since 1582-10-15 (time_low, time_mid, time_hi).
            let low = u64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
            let mid = u64::from(u16::from_be_bytes([b[4], b[5]]));
            let hi = u64::from(u16::from_be_bytes([b[6] & 0x0f, b[7]]));
            let ticks = i64::try_from((hi << 48) | (mid << 32) | low).unwrap_or(0);
            // 1582-10-15 to 1970-01-01 is 12219292800 seconds.
            let unix_us = ticks / 10 - 12_219_292_800_000_000;
            Ok(Value::Timestamptz(unix_us - UNIX_TO_PG_MICROS))
        }
        _ => Ok(Value::Null),
    }
}

fn current_setting(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let name = a[0].as_str().unwrap_or("").to_ascii_lowercase();
    match ctx.state.setting(ctx.env, &name) {
        Some(v) => Ok(Value::Text(crate::settings::show(crate::settings::lookup(&name), &v))),
        None if a.get(1).and_then(Value::as_bool) == Some(true) => Ok(Value::Null),
        None => Err(crate::settings::unrecognized(&name)),
    }
}

/// `set_config(name, value, is_local)`: validated now, applied to the session when the
/// statement completes (later calls in the same statement see it).
fn set_config(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let Some(name) = a[0].as_str() else {
        return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "SET requires parameter name"));
    };
    let name = name.to_ascii_lowercase();
    let raw = a[1].as_str().map(str::to_owned);
    let local = a[2].as_bool().unwrap_or(false);
    let current = ctx.state.setting(ctx.env, &name);
    let stored = match raw {
        Some(raw) => crate::session::validate_setting(&name, &raw, current.as_deref())?,
        // A NULL value resets the parameter.
        None => {
            if let Some(def) = crate::settings::lookup(&name) {
                crate::settings::check_settable(def)?;
            } else if !crate::settings::is_custom(&name) {
                return Err(crate::settings::unrecognized(&name));
            }
            crate::session::default_settings()
                .get(&name)
                .cloned()
                .or_else(|| crate::settings::lookup(&name).and_then(|d| d.boot).map(str::to_owned))
                .unwrap_or_default()
        }
    };
    let shown = crate::settings::show(crate::settings::lookup(&name), &stored);
    ctx.state.settings.borrow_mut().push((name, stored, local));
    Ok(Value::Text(shown))
}

/// Resolves a sequence name argument (`'public.users_id_seq'`, `'"MySeq"'`, `'users_id_seq'::regclass`).
fn sequence(ctx: &EvalCtx, v: &Value) -> SqlResult<std::sync::Arc<pgmem_catalog::SequenceDef>> {
    let catalog = ctx.txn.catalog();
    let rel = match v {
        Value::Reg(_, oid, _) | Value::Oid(oid) => catalog.relation_by_oid(*oid),
        _ => crate::reg::relation_from_text(v.as_str().unwrap_or(""), catalog)?,
    };
    match rel {
        Some(pgmem_catalog::Relation::Sequence(s)) => Ok(s),
        Some(other) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a sequence", other.name()))),
        None => Err(SqlError::undefined_table(&parse_qualified(v.as_str().unwrap_or("")))),
    }
}

/// Applies identifier folding to a possibly-qualified, possibly-quoted name in a string.
pub fn parse_qualified(raw: &str) -> String {
    let last = raw.rsplit('.').next().unwrap_or(raw).trim();
    if let Some(q) = last.strip_prefix('"').and_then(|s| s.strip_suffix('"')) { q.replace("\"\"", "\"") } else { last.to_lowercase() }
}

fn nextval(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let seq = sequence(ctx, &a[0])?;
    let v = ctx.txn.database().nextval(&seq)?;
    ctx.state.currval.borrow_mut().insert(seq.oid, v);
    Ok(Value::Int8(v))
}

fn currval(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let seq = sequence(ctx, &a[0])?;
    if let Some(v) = ctx.state.currval.borrow().get(&seq.oid) {
        return Ok(Value::Int8(*v));
    }
    ctx.env.currval.get(&seq.oid).map(|v| Value::Int8(*v)).ok_or_else(|| {
        SqlError::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("currval of sequence \"{}\" is not yet defined in this session", seq.name),
        )
    })
}

fn lastval(_: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    if let Some(v) = ctx.state.currval.borrow().values().last() {
        return Ok(Value::Int8(*v));
    }
    ctx.env
        .lastval
        .map(Value::Int8)
        .ok_or_else(|| SqlError::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "lastval is not yet defined in this session"))
}

fn setval(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let seq = sequence(ctx, &a[0])?;
    let v = a[1].as_i64().unwrap_or(0);
    let called = a.get(2).and_then(Value::as_bool).unwrap_or(true);
    ctx.txn.database().setval(&seq, v, called)?;
    if called {
        ctx.state.currval.borrow_mut().insert(seq.oid, v);
    }
    Ok(Value::Int8(v))
}

// ---------------------------------------------------------------------------------------------
// Enum support functions (enum.c). The first argument is the enum type's OID, added by the
// binder; the labels are read from the statement's type registry.
// ---------------------------------------------------------------------------------------------

fn enum_labels(a: &[Value]) -> SqlResult<(u32, Vec<pgmem_types::usertype::EnumLabel>)> {
    let Value::Oid(oid) = a[0] else { return Err(SqlError::internal("enum function without its type")) };
    let t = pgmem_types::usertype::lookup(oid).ok_or_else(|| SqlError::internal(format!("cache lookup failed for type {oid}")))?;
    match &t.kind {
        pgmem_types::usertype::UserKind::Enum { labels } => Ok((oid, labels.clone())),
        _ => Err(SqlError::internal(format!("type {} is not an enum", t.name))),
    }
}

pub fn enum_first(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (oid, labels) = enum_labels(a)?;
    Ok(labels.first().map_or(Value::Null, |l| Value::Enum(oid, l.oid)))
}

pub fn enum_last(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (oid, labels) = enum_labels(a)?;
    Ok(labels.last().map_or(Value::Null, |l| Value::Enum(oid, l.oid)))
}

/// `enum_range(e)`: every label; `enum_range(lo, hi)`: the labels from `lo` to `hi`
/// inclusive, a NULL bound meaning the first / last label.
pub fn enum_range(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (oid, labels) = enum_labels(a)?;
    let pos = |v: &Value| match v {
        Value::Enum(_, l) => labels.iter().position(|x| x.oid == *l),
        _ => None,
    };
    let (lo, hi) = match a.len() {
        2 => (0, labels.len()),
        _ => (pos(&a[1]).unwrap_or(0), pos(&a[2]).map_or(labels.len(), |p| p + 1)),
    };
    let items = if lo < hi { labels[lo..hi].iter().map(|l| Value::Enum(oid, l.oid)).collect() } else { Vec::new() };
    Ok(Value::Array(items))
}
