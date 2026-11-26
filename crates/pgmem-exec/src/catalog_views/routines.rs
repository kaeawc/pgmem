//! Catalog rows for user functions, procedures and triggers: `pg_proc`, `pg_trigger`,
//! `information_schema.routines` / `parameters` / `triggers`, and the
//! `pg_get_function_arguments` / `pg_get_function_result` / `pg_get_triggerdef` text.

use super::{Spec, boolean, ch, int2, int4, name, oid, snapshot, txt};
use crate::deparse::pg::format_type;
use crate::eval::EvalCtx;
use crate::plan::Row;
use pgmem_catalog::{ArgMode, Catalog, FuncReturn, FunctionDef, TriggerDef, TriggerTiming};
use pgmem_types::{SqlResult, Type, Value};

const PLPGSQL_OID: u32 = 13_562;
const SQL_OID: u32 = 14;
const TRIGGER_TYPE_OID: u32 = 2279;

fn lang_oid(f: &FunctionDef) -> u32 {
    if f.language == "plpgsql" { PLPGSQL_OID } else { SQL_OID }
}

/// The OID `pg_proc.prorettype` reports.
fn ret_type_oid(f: &FunctionDef) -> u32 {
    match &f.ret {
        FuncReturn::Scalar(Type::Unknown) => TRIGGER_TYPE_OID,
        FuncReturn::Scalar(t) | FuncReturn::SetOf(t) => t.oid(),
        FuncReturn::Table(_) => Type::Record.oid(),
        FuncReturn::None => Type::Void.oid(),
    }
}

fn type_text(t: &Type) -> String {
    match t {
        Type::Unknown => "trigger".to_owned(),
        other => format_type(other, -1),
    }
}

/// `pg_get_function_arguments`: `a integer, b text DEFAULT 'x'::text` (the identity form
/// leaves out the defaults).
pub fn function_arguments(catalog: &Catalog, f: &FunctionDef, with_defaults: bool) -> String {
    let parts: Vec<String> = f
        .args
        .iter()
        .map(|a| {
            let mut s = String::new();
            match a.mode {
                ArgMode::Out => s.push_str("OUT "),
                ArgMode::InOut => s.push_str("INOUT "),
                ArgMode::Variadic => s.push_str("VARIADIC "),
                ArgMode::In => {}
            }
            if let Some(n) = &a.name {
                s.push_str(&crate::funcs::string::quote_ident(n));
                s.push(' ');
            }
            s.push_str(&type_text(&a.ty));
            if with_defaults && let Some(d) = &a.default {
                let text = crate::deparse::deparse_domain_check(catalog, &a.ty, -1, d).unwrap_or_default();
                s.push_str(&format!(" DEFAULT {text}"));
            }
            s
        })
        .collect();
    parts.join(", ")
}

/// `pg_get_function_result`: `integer`, `SETOF text`, `TABLE(a integer)`, `trigger`.
pub fn function_result(f: &FunctionDef) -> Option<String> {
    Some(match &f.ret {
        FuncReturn::None => return None,
        FuncReturn::Scalar(t) => type_text(t),
        FuncReturn::SetOf(t) => format!("SETOF {}", type_text(t)),
        FuncReturn::Table(cols) => {
            let cols: Vec<String> =
                cols.iter().map(|(n, t)| format!("{} {}", crate::funcs::string::quote_ident(n), type_text(t))).collect();
            format!("TABLE({})", cols.join(", "))
        }
    })
}

fn oid_array(oids: impl Iterator<Item = u32>) -> Value {
    Value::Array(oids.map(Value::Oid).collect())
}

pub(super) fn pg_proc(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(super::pg_catalog::PG_PROC);
    Ok(m.catalog
        .functions()
        .map(|f| {
            let inputs: Vec<&pgmem_catalog::FuncArg> = f.args.iter().filter(|a| a.mode.is_input()).collect();
            let has_out = f.args.iter().any(|a| a.mode != ArgMode::In);
            let named = f.args.iter().any(|a| a.name.is_some());
            s.row(vec![
                ("oid", oid(f.oid)),
                ("proname", name(&f.name)),
                ("pronamespace", oid(m.namespace(&f.schema))),
                ("proowner", oid(owner_oid(ctx, m.catalog, f))),
                ("prolang", oid(lang_oid(f))),
                ("procost", Value::Float4(f.cost as f32)),
                ("prorows", Value::Float4(f.rows as f32)),
                ("provariadic", oid(0)),
                ("prosupport", Value::Reg(pgmem_types::RegKind::Proc, 0, "-".into())),
                ("prokind", ch(if f.procedure { 'p' } else { 'f' })),
                ("prosecdef", boolean(f.security_definer)),
                ("proleakproof", boolean(f.leakproof)),
                ("proisstrict", boolean(f.strict)),
                ("proretset", boolean(f.returns_set())),
                ("provolatile", ch(f.volatility)),
                ("proparallel", ch(f.parallel)),
                (
                    "proconfig",
                    if f.config.is_empty() { Value::Null } else { Value::Array(f.config.iter().cloned().map(Value::Text).collect()) },
                ),
                ("pronargs", int2(inputs.len())),
                ("pronargdefaults", int2(inputs.iter().filter(|a| a.default.is_some()).count())),
                ("prorettype", oid(ret_type_oid(f))),
                ("proargtypes", Value::Array(inputs.iter().map(|a| Value::Oid(a.ty.oid())).collect())),
                ("proallargtypes", if has_out { oid_array(f.args.iter().map(|a| a.ty.oid())) } else { Value::Null }),
                (
                    "proargmodes",
                    if has_out {
                        Value::Array(f.args.iter().map(|a| Value::Text(a.mode.code().to_string())).collect())
                    } else {
                        Value::Null
                    },
                ),
                (
                    "proargnames",
                    if named {
                        Value::Array(f.args.iter().map(|a| Value::Text(a.name.clone().unwrap_or_default())).collect())
                    } else {
                        Value::Null
                    },
                ),
                ("prosrc", txt(f.body.clone())),
            ])
        })
        .collect())
}

/// `pg_trigger.tgtype` bits.
fn tgtype(t: &TriggerDef) -> i16 {
    let mut v = 0i16;
    if t.for_each_row {
        v |= 1;
    }
    match t.timing {
        TriggerTiming::Before => v |= 2,
        TriggerTiming::InsteadOf => v |= 64,
        TriggerTiming::After => {}
    }
    if t.insert {
        v |= 4;
    }
    if t.delete {
        v |= 8;
    }
    if t.update.is_some() {
        v |= 16;
    }
    if t.truncate {
        v |= 32;
    }
    v
}

pub(super) fn pg_trigger(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(super::pg_catalog::PG_TRIGGER);
    Ok(m.catalog
        .triggers()
        .map(|t| {
            let mut args = Vec::new();
            for a in &t.args {
                args.extend_from_slice(a.as_bytes());
                args.push(0);
            }
            let attrs: Vec<Value> = match (&t.update, m.catalog.table_by_oid(t.table)) {
                (Some(cols), Some(table)) => {
                    cols.iter().filter_map(|c| table.column_index(c)).map(|i| Value::Int2(i16::try_from(i + 1).unwrap_or(0))).collect()
                }
                _ => Vec::new(),
            };
            s.row(vec![
                ("oid", oid(t.oid)),
                ("tgrelid", oid(t.table)),
                ("tgparentid", oid(0)),
                ("tgname", name(&t.name)),
                ("tgfoid", oid(t.function)),
                ("tgtype", Value::Int2(tgtype(t))),
                ("tgenabled", ch(t.enabled)),
                ("tgisinternal", boolean(false)),
                ("tgconstrrelid", oid(0)),
                ("tgconstrindid", oid(0)),
                ("tgconstraint", oid(0)),
                ("tgdeferrable", boolean(false)),
                ("tginitdeferred", boolean(false)),
                ("tgnargs", int2(t.args.len())),
                ("tgattr", Value::Array(attrs)),
                ("tgargs", Value::Bytea(args)),
            ])
        })
        .collect())
}

fn timing_text(t: TriggerTiming) -> &'static str {
    match t {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
        TriggerTiming::InsteadOf => "INSTEAD OF",
    }
}

/// `pg_get_triggerdef`.
pub fn trigger_def(catalog: &Catalog, t: &TriggerDef, pretty: bool) -> String {
    let mut events = Vec::new();
    if t.insert {
        events.push("INSERT".to_owned());
    }
    if t.delete {
        events.push("DELETE".to_owned());
    }
    if let Some(cols) = &t.update {
        events.push(if cols.is_empty() { "UPDATE".to_owned() } else { format!("UPDATE OF {}", cols.join(", ")) });
    }
    if t.truncate {
        events.push("TRUNCATE".to_owned());
    }
    // Pretty mode leaves out the schema of a relation the search path finds.
    let table = catalog
        .relation_by_oid(t.table)
        .map(|r| {
            if pretty {
                crate::deparse::relation_name(catalog, r.schema(), r.name())
            } else {
                format!("{}.{}", crate::funcs::string::quote_ident(r.schema()), crate::funcs::string::quote_ident(r.name()))
            }
        })
        .unwrap_or_default();
    // ruleutils' `generate_function_name`: schema-qualified unless the search path finds it.
    let func = catalog
        .function_by_oid(t.function)
        .map(|f| {
            let name = crate::funcs::string::quote_ident(&f.name);
            if catalog.function_visible(f) { name } else { format!("{}.{name}", crate::funcs::string::quote_ident(&f.schema)) }
        })
        .unwrap_or_default();
    let mut s = format!(
        "CREATE TRIGGER {} {} {} ON {table} FOR EACH {}",
        crate::funcs::string::quote_ident(&t.name),
        timing_text(t.timing),
        events.join(" OR "),
        if t.for_each_row { "ROW" } else { "STATEMENT" }
    );
    if let Some(w) = &t.when {
        s.push_str(&format!(" WHEN ({w})"));
    }
    let args: Vec<String> = t.args.iter().map(|a| crate::deparse::pg::quote_literal(a)).collect();
    s.push_str(&format!(" EXECUTE FUNCTION {func}({})", args.join(", ")));
    s
}

/// The OID of the role that owns function `f` (`proowner`).
fn owner_oid(ctx: &EvalCtx, catalog: &Catalog, f: &FunctionDef) -> u32 {
    let owner = crate::roles::owner_of(catalog, ctx.env, f.oid);
    if owner == ctx.env.user {
        return super::model::SUPERUSER_OID;
    }
    catalog.role(&owner).map(|r| r.oid).or_else(|| super::pg_catalog::predefined_role(&owner)).unwrap_or(super::model::SUPERUSER_OID)
}

/// `information_schema.routines`: the routines the current user owns (through membership)
/// or may execute; the definition only to the owner.
pub(super) fn is_routines(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(super::information_schema::ROUTINES);
    let me = crate::roles::current_role(ctx.env);
    let superuser = crate::roles::is_superuser(m.catalog, ctx.env, &me);
    let member_of_owner =
        |f: &FunctionDef| superuser || m.catalog.roles_of(&me).contains(&crate::roles::owner_of(m.catalog, ctx.env, f.oid));
    let yes_no = |b: bool| txt(if b { "YES" } else { "NO" });
    Ok(m.catalog
        .functions()
        .filter(|f| {
            member_of_owner(f) || crate::roles::has_privilege(m.catalog, ctx.env, &me, &pgmem_catalog::GrantObject::Function(f.oid), 'X')
        })
        .map(|f| {
            let specific = format!("{}_{}", f.name, f.oid);
            let ret = match &f.ret {
                FuncReturn::None => None,
                FuncReturn::Scalar(Type::Unknown) => Some((txt("trigger"), "pg_catalog".to_owned(), "trigger".to_owned())),
                FuncReturn::Table(_) => Some((txt("record"), "pg_catalog".to_owned(), "record".to_owned())),
                FuncReturn::Scalar(t) | FuncReturn::SetOf(t) => {
                    let data_type = match t {
                        Type::Domain(_) => "USER-DEFINED".to_owned(),
                        _ => super::information_schema::data_type_name(t),
                    };
                    let schema = match t {
                        Type::Array(_) => "pg_catalog".to_owned(),
                        _ => super::information_schema::type_schema(t),
                    };
                    Some((txt(data_type), schema, t.typname()))
                }
            };
            let catalog = super::information_schema::catalog_name(ctx);
            let routine = !f.procedure;
            s.row(vec![
                ("specific_catalog", catalog.clone()),
                ("specific_schema", name(&f.schema)),
                ("specific_name", name(&specific)),
                ("routine_catalog", catalog.clone()),
                ("routine_schema", name(&f.schema)),
                ("routine_name", name(&f.name)),
                ("routine_type", txt(if f.procedure { "PROCEDURE" } else { "FUNCTION" })),
                ("data_type", ret.as_ref().map_or(Value::Null, |r| r.0.clone())),
                ("type_udt_catalog", if ret.is_some() { catalog } else { Value::Null }),
                ("type_udt_schema", ret.as_ref().map_or(Value::Null, |r| name(&r.1))),
                ("type_udt_name", ret.as_ref().map_or(Value::Null, |r| name(&r.2))),
                ("dtd_identifier", if routine { name("0") } else { Value::Null }),
                ("routine_body", txt(if f.language == "sql" { "SQL" } else { "EXTERNAL" })),
                ("routine_definition", if member_of_owner(f) { txt(f.body.clone()) } else { Value::Null }),
                ("external_language", txt(f.language.to_uppercase())),
                ("parameter_style", txt("GENERAL")),
                ("is_deterministic", yes_no(f.volatility == 'i')),
                ("sql_data_access", txt("MODIFIES")),
                ("is_null_call", if routine { yes_no(f.strict) } else { Value::Null }),
                ("schema_level_routine", txt("YES")),
                ("max_dynamic_result_sets", int4(0)),
                ("security_type", txt(if f.security_definer { "DEFINER" } else { "INVOKER" })),
                ("as_locator", txt("NO")),
                ("is_udt_dependent", txt("NO")),
            ])
        })
        .collect())
}

pub(super) fn is_parameters(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(super::information_schema::PARAMETERS);
    let mut out = Vec::new();
    for f in m.catalog.functions() {
        let specific = format!("{}_{}", f.name, f.oid);
        for (i, a) in f.args.iter().enumerate() {
            out.push(s.row(vec![
                ("specific_catalog", super::information_schema::catalog_name(ctx)),
                ("specific_schema", name(&f.schema)),
                ("specific_name", name(&specific)),
                ("ordinal_position", int4(i64::try_from(i + 1).unwrap_or(0))),
                (
                    "parameter_mode",
                    txt(match a.mode {
                        ArgMode::In | ArgMode::Variadic => "IN",
                        ArgMode::Out => "OUT",
                        ArgMode::InOut => "INOUT",
                    }),
                ),
                ("is_result", txt("NO")),
                ("as_locator", txt("NO")),
                ("parameter_name", a.name.as_deref().map_or(Value::Null, name)),
                ("data_type", txt(super::information_schema::data_type_name(&a.ty))),
                ("udt_name", name(&a.ty.typname())),
            ]));
        }
    }
    Ok(out)
}

pub(super) fn is_triggers(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(super::information_schema::TRIGGERS);
    let mut out = Vec::new();
    for t in m.catalog.triggers() {
        let Some(rel) = m.catalog.relation_by_oid(t.table) else { continue };
        let func = m.catalog.function_by_oid(t.function).map(|f| f.name.clone()).unwrap_or_default();
        let mut events = Vec::new();
        if t.insert {
            events.push("INSERT");
        }
        if t.delete {
            events.push("DELETE");
        }
        if t.update.is_some() {
            events.push("UPDATE");
        }
        for e in events {
            // action_order: position among the table's triggers for the same event and timing.
            let order = m
                .catalog
                .table_triggers(t.table)
                .iter()
                .filter(|o| o.timing == t.timing && o.for_each_row == t.for_each_row)
                .filter(|o| match e {
                    "INSERT" => o.insert,
                    "DELETE" => o.delete,
                    _ => o.update.is_some(),
                })
                .position(|o| o.oid == t.oid)
                .map_or(1, |p| p + 1);
            out.push(s.row(vec![
                ("trigger_catalog", super::information_schema::catalog_name(ctx)),
                ("trigger_schema", name(rel.schema())),
                ("trigger_name", name(&t.name)),
                ("event_manipulation", txt(e)),
                ("event_object_catalog", super::information_schema::catalog_name(ctx)),
                ("event_object_schema", name(rel.schema())),
                ("event_object_table", name(rel.name())),
                ("action_order", int4(i64::try_from(order).unwrap_or(1))),
                ("action_condition", t.when.as_ref().map_or(Value::Null, |w| txt(format!("({w})")))),
                ("action_statement", txt(format!("EXECUTE FUNCTION {func}()"))),
                ("action_orientation", txt(if t.for_each_row { "ROW" } else { "STATEMENT" })),
                ("action_timing", txt(timing_text(t.timing))),
            ]));
        }
    }
    Ok(out)
}
