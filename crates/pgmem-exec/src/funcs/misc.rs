//! Session, system-information, sequence and UUID functions.

use super::{Resolved, lenient, no_function, resolved, strict};
use crate::eval::EvalCtx;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    match name {
        "gen_random_uuid" | "uuid_generate_v4" if n == 0 => resolved(lenient("gen_random_uuid", gen_random_uuid), vec![], Type::Uuid),
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
        "set_config" if n == 3 => {
            resolved(lenient("set_config", |a, _| Ok(a[1].clone())), vec![Type::Text, Type::Text, Type::Bool], Type::Text)
        }
        "current_user" | "session_user" | "user" | "current_role" if n == 0 => {
            resolved(lenient("current_user", |_, c| Ok(Value::Text(c.env.user.clone()))), vec![], Type::Text)
        }
        "current_database" | "current_catalog" if n == 0 => {
            resolved(lenient("current_database", |_, c| Ok(Value::Text(c.env.database.clone()))), vec![], Type::Text)
        }
        "current_schema" if n == 0 => resolved(lenient("current_schema", |_, _| Ok(Value::Text("public".into()))), vec![], Type::Text),
        "current_schemas" if n == 1 => resolved(
            strict("current_schemas", |a, _| {
                let mut v = Vec::new();
                if a[0].as_bool() == Some(true) {
                    v.push(Value::Text("pg_catalog".into()));
                }
                v.push(Value::Text("public".into()));
                Ok(Value::Array(v))
            }),
            vec![Type::Bool],
            Type::array_of(Type::Text),
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
            resolved(strict(name_static(name), |_, _| Ok(Value::Text(String::new()))), vec![Type::Int8; n.max(1)], Type::Text)
        }
        "pg_try_advisory_lock"
        | "pg_try_advisory_xact_lock"
        | "pg_advisory_unlock"
        | "pg_try_advisory_lock_shared"
        | "pg_advisory_unlock_shared" => {
            resolved(strict(name_static(name), |_, _| Ok(Value::Bool(true))), vec![Type::Int8; n.max(1)], Type::Bool)
        }
        "pg_advisory_unlock_all" if n == 0 => {
            resolved(lenient("pg_advisory_unlock_all", |_, _| Ok(Value::Text(String::new()))), vec![], Type::Text)
        }
        "pg_sleep" if n == 1 => resolved(strict("pg_sleep", |_, _| Ok(Value::Text(String::new()))), vec![Type::Float8], Type::Text),
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
        _ => Ok(None),
    }
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

fn current_setting(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let name = a[0].as_str().unwrap_or("").to_ascii_lowercase();
    match ctx.env.setting(&name) {
        Some(v) => Ok(Value::Text(v)),
        None if a.get(1).and_then(Value::as_bool) == Some(true) => Ok(Value::Null),
        None => Err(SqlError::new(code::UNDEFINED_OBJECT, format!("unrecognized configuration parameter \"{name}\""))),
    }
}

/// Resolves a sequence name argument (`'public.users_id_seq'`, `'"MySeq"'`, `'users_id_seq'::regclass`).
fn sequence(ctx: &EvalCtx, v: &Value) -> SqlResult<std::sync::Arc<pgmem_catalog::SequenceDef>> {
    let raw = v.as_str().unwrap_or("");
    let name = parse_qualified(raw);
    ctx.txn.catalog().sequence(&name).cloned().ok_or_else(|| SqlError::undefined_table(&name))
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
