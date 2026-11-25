//! System information and catalog functions: `format_type`, `pg_get_*def`, `obj_description`,
//! `to_regclass`, privilege checks (always granted: pgmem sessions are superusers), and the
//! relation-size functions.

use super::{Resolved, lenient, resolved, strict};
use crate::catalog_views::{CLASS_CLASS_OID, CONSTRAINT_CLASS_OID, Model, NAMESPACE_CLASS_OID, constraint_def, index_def};
use crate::eval::EvalCtx;
use crate::funcs::string::quote_ident;
use pgmem_types::{RegKind, SqlError, SqlResult, Type, Value, code};

fn oid_arg(v: &Value) -> Option<u32> {
    match v {
        Value::Oid(o) | Value::Reg(_, o, _) => Some(*o),
        other => other.as_i64().and_then(|n| u32::try_from(n).ok()),
    }
}

fn text_arg(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// `format_type(type_oid, typmod)`: the SQL spelling of a type with its modifier.
pub fn format_type(type_oid: u32, typmod: i32, catalog: &pgmem_catalog::Catalog) -> Option<String> {
    let Some(ty) = Type::from_oid(type_oid) else {
        // A table's row type is named after the table.
        if let Some(t) = catalog.tables().find(|t| crate::catalog_views::rowtype_oid_of(t.oid) == type_oid) {
            return Some(quote_ident(&t.name));
        }
        // Other builtin types (pseudo-types such as "any" or anyelement, tsvector, ...).
        return crate::catalog_views::builtin_type_name(|o, _| o == type_oid).map(|(_, _, f)| f.to_owned());
    };
    let (elem, array) = match &ty {
        Type::Array(e) => ((**e).clone(), "[]"),
        other => (other.clone(), ""),
    };
    let base = if typmod < 0 {
        elem.name()
    } else {
        match elem {
            Type::Varchar => format!("character varying({})", typmod - 4),
            Type::Bpchar => format!("character({})", typmod - 4),
            Type::Numeric => {
                let (p, s) = crate::cast::numeric_typmod_parts(typmod);
                format!("numeric({p},{s})")
            }
            Type::Timestamp => format!("timestamp({typmod}) without time zone"),
            Type::Timestamptz => format!("timestamp({typmod}) with time zone"),
            Type::Time => format!("time({typmod}) without time zone"),
            Type::TimeTz => format!("time({typmod}) with time zone"),
            Type::Interval => format!("interval({typmod})"),
            other => other.name(),
        }
    };
    Some(format!("{base}{array}"))
}

fn format_type_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
    let typmod = a.get(1).and_then(Value::as_i64).and_then(|n| i32::try_from(n).ok()).unwrap_or(-1);
    Ok(Value::Text(format_type(o, typmod, ctx.txn.catalog()).unwrap_or_else(|| "???".to_owned())))
}

fn model<'a>(ctx: &'a EvalCtx) -> Model<'a> {
    Model::new(ctx.txn.catalog(), ctx.env)
}

/// `pg_get_function_*` / `pg_get_functiondef` / `pg_get_triggerdef`.
fn routine_text_function(name: &str, n: usize) -> SqlResult<Option<Resolved>> {
    use Type::{Bool, Oid, Text};
    match name {
        "pg_get_triggerdef" if n == 1 || n == 2 => resolved(
            strict("pg_get_triggerdef", |a, c| {
                let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
                let cat = c.txn.catalog();
                Ok(cat
                    .triggers()
                    .find(|t| t.oid == o)
                    .map_or(Value::Null, |t| Value::Text(crate::catalog_views::routines::trigger_def(cat, t))))
            }),
            [Oid, Bool][..n].to_vec(),
            Text,
        ),
        "pg_get_triggerdef" => Ok(None),
        _ if n != 1 => Ok(None),
        _ => {
            let f = match name {
                "pg_get_function_arguments" => strict("pg_get_function_arguments", |a, c| function_text(a, c, FnText::Arguments)),
                "pg_get_function_identity_arguments" => {
                    strict("pg_get_function_identity_arguments", |a, c| function_text(a, c, FnText::Identity))
                }
                "pg_get_function_result" => strict("pg_get_function_result", |a, c| function_text(a, c, FnText::Result)),
                _ => strict("pg_get_functiondef", |a, c| function_text(a, c, FnText::Definition)),
            };
            resolved(f, vec![Oid], Text)
        }
    }
}

fn privilege_function(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let targets = args.iter().map(|t| if *t == Type::Unknown { Type::Text } else { t.clone() }).collect();
    let f = match name {
        "has_table_privilege" => strict("has_table_privilege", |a, c| privilege_check(a, c, PrivKind::Table)),
        "has_sequence_privilege" => strict("has_sequence_privilege", |a, c| privilege_check(a, c, PrivKind::Sequence)),
        "has_schema_privilege" => strict("has_schema_privilege", |a, c| privilege_check(a, c, PrivKind::Schema)),
        _ => strict("pg_has_role", |a, c| privilege_check(a, c, PrivKind::Role)),
    };
    resolved(f, targets, Type::Bool)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrivKind {
    Table,
    Sequence,
    Schema,
    Role,
}

/// A role argument: a name, or an OID of the session user / a created role.
fn role_arg(v: &Value, ctx: &EvalCtx) -> SqlResult<String> {
    let cat = ctx.txn.catalog();
    if let Some(o) = oid_arg(v).filter(|_| !matches!(v, Value::Text(_))) {
        if o == 10 {
            return Ok(ctx.env.user.clone());
        }
        return cat
            .roles()
            .find(|r| r.oid == o)
            .map(|r| r.name.clone())
            .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("role with OID {o} does not exist")));
    }
    let name = text_arg(v).to_owned();
    if !crate::roles::role_exists(cat, ctx.env, &name) && name != "public" {
        return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("role \"{name}\" does not exist")));
    }
    Ok(name)
}

/// `has_table_privilege([user,] table, 'SELECT, INSERT [WITH GRANT OPTION]')` and its
/// schema / sequence / role siblings: true if any listed privilege is held.
fn privilege_check(a: &[Value], ctx: &EvalCtx, kind: PrivKind) -> SqlResult<Value> {
    let (user, object, privs) = match a {
        [o, p] => (crate::roles::current_role(ctx.env), o, p),
        [u, o, p] => (role_arg(u, ctx)?, o, p),
        _ => return Ok(Value::Null),
    };
    let cat = ctx.txn.catalog();
    let raw: Vec<String> = text_arg(privs).split(',').map(|w| w.trim().to_owned()).collect();
    let words: Vec<String> = raw
        .iter()
        .map(|w| w.to_lowercase().trim_end_matches(" with grant option").trim_end_matches(" with admin option").trim().to_owned())
        .collect();
    let unrecognized = |i: usize| SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized privilege type: \"{}\"", raw[i]));
    if kind == PrivKind::Role {
        let role = role_arg(object, ctx)?;
        for (i, w) in words.iter().enumerate() {
            if !matches!(w.as_str(), "member" | "usage" | "set") {
                return Err(unrecognized(i));
            }
        }
        let member = crate::roles::is_superuser(cat, ctx.env, &user) || cat.roles_of(&user).contains(&role);
        return Ok(Value::Bool(member));
    }
    let (object, valid) = match kind {
        PrivKind::Schema => {
            let s = text_arg(object).to_owned();
            let s = match oid_arg(object).filter(|_| !matches!(object, Value::Text(_))) {
                Some(o) => cat.schema_by_oid(o).map(|sc| sc.name.clone()).unwrap_or(s),
                None => s,
            };
            if cat.schema(&s).is_none() && !pgmem_catalog::is_system_schema(&s) {
                return Err(pgmem_catalog::undefined_schema(&s));
            }
            (pgmem_catalog::GrantObject::Schema(s), "usage create")
        }
        _ => {
            let rel = match object {
                Value::Text(t) => crate::reg::relation_from_text(t, cat)?,
                other => cat.relation_by_oid(oid_arg(other).unwrap_or(0)),
            };
            let Some(rel) = rel else {
                // System catalogs are readable by everyone.
                let system = match object {
                    Value::Text(t) => crate::catalog_views::system_relation(None, &crate::funcs::misc::parse_qualified(t)).is_some(),
                    other => crate::catalog_views::system_relation_by_oid(oid_arg(other).unwrap_or(0)).is_some(),
                };
                if system {
                    return Ok(Value::Bool(words.iter().all(|w| w == "select")));
                }
                return Err(match object {
                    Value::Text(t) => SqlError::undefined_table(t),
                    other => {
                        SqlError::new(code::UNDEFINED_TABLE, format!("relation with OID {} does not exist", oid_arg(other).unwrap_or(0)))
                    }
                });
            };
            let valid = if kind == PrivKind::Sequence {
                "usage select update"
            } else {
                "select insert update delete truncate references trigger maintain"
            };
            (pgmem_catalog::GrantObject::Relation(rel.oid()), valid)
        }
    };
    let mut held = false;
    for (i, w) in words.iter().enumerate() {
        if !valid.split(' ').any(|v| v == w) {
            return Err(unrecognized(i));
        }
        let letter = match w.as_str() {
            "select" => 'r',
            "insert" => 'a',
            "update" => 'w',
            "delete" => 'd',
            "truncate" => 'D',
            "references" => 'x',
            "trigger" => 't',
            "maintain" => 'm',
            "usage" => 'U',
            _ => 'C',
        };
        held |= crate::roles::has_privilege(cat, ctx.env, &user, &object, letter);
    }
    Ok(Value::Bool(held))
}

#[derive(Clone, Copy)]
enum FnText {
    Arguments,
    Identity,
    Result,
    Definition,
}

/// `pg_get_function_arguments` and friends for a user function (NULL for unknown OIDs).
fn function_text(a: &[Value], ctx: &EvalCtx, what: FnText) -> SqlResult<Value> {
    use crate::catalog_views::routines::{function_arguments, function_result};
    let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
    let cat = ctx.txn.catalog();
    let Some(f) = cat.function_by_oid(o) else { return Ok(Value::Null) };
    Ok(match what {
        FnText::Arguments => Value::Text(function_arguments(cat, f, true)),
        FnText::Identity => Value::Text(function_arguments(cat, f, false)),
        FnText::Result => function_result(f).map_or(Value::Null, Value::Text),
        FnText::Definition => {
            let kind = if f.procedure { "PROCEDURE" } else { "FUNCTION" };
            let mut s = format!(
                "CREATE OR REPLACE {kind} {}.{}({})\n",
                quote_ident(&f.schema),
                quote_ident(&f.name),
                function_arguments(cat, f, true)
            );
            if let Some(r) = function_result(f) {
                s.push_str(&format!(" RETURNS {r}\n"));
            }
            s.push_str(&format!(" LANGUAGE {}\n", f.language));
            match f.volatility {
                'i' => s.push_str(" IMMUTABLE"),
                's' => s.push_str(" STABLE"),
                _ => {}
            }
            if f.strict {
                s.push_str(" STRICT");
            }
            if f.volatility != 'v' || f.strict {
                s.push('\n');
            }
            s.push_str(&format!("AS $function${}$function$\n", f.body));
            Value::Text(s)
        }
    })
}

fn constraintdef_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let m = model(ctx);
    let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
    let pretty = a.get(1).and_then(Value::as_bool) == Some(true);
    if let Some(c) = m.constraint_by_oid(o) {
        return Ok(Value::Text(crate::deparse::with_pretty(pretty, || constraint_def(&m, &c))));
    }
    let domain = m.catalog.types().flat_map(|t| crate::catalog_views::domain_constraints(m.catalog, t)).find(|c| c.oid == o);
    Ok(domain.map_or(Value::Null, |c| Value::Text(c.def())))
}

fn indexdef_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let m = model(ctx);
    let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
    let Some((t, idx)) = m.indexes().into_iter().find(|(_, i)| i.oid == o) else { return Ok(Value::Null) };
    let column = a.get(1).and_then(Value::as_i64).unwrap_or(0);
    if column > 0 {
        // Just the key expression of one column.
        let k = usize::try_from(column - 1).unwrap_or(usize::MAX);
        let key = if let Some(c) = idx.columns.get(k) {
            t.columns.get(*c).map(|c| quote_ident(&c.name))
        } else {
            idx.exprs.get(k - idx.columns.len()).map(|e| crate::deparse::deparse_expr(m.catalog, t, e).unwrap_or_default())
        };
        return Ok(key.map_or(Value::Null, Value::Text));
    }
    let def = index_def(&m, t, idx);
    // Pretty mode leaves out the schema of relations on the search path.
    let pretty = a.get(2).and_then(Value::as_bool) == Some(true);
    // Pretty mode prints the table name alone when it is visible.
    let pretty_def = || -> Option<String> {
        let start = def.find(" ON ")? + 4;
        let end = start + def[start..].find(" USING ")?;
        let parts = crate::reg::parse_qualified(&def[start..end]).ok()?;
        let [schema, name] = parts.as_slice() else { return None };
        m.catalog.relation_visible(schema, name).then(|| format!("{}{}{}", &def[..start], quote_ident(name), &def[end..]))
    };
    Ok(Value::Text(if pretty { pretty_def().unwrap_or(def) } else { def }))
}

fn viewdef_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let catalog = ctx.txn.catalog();
    let pretty = a.get(1).and_then(Value::as_bool) == Some(true);
    let view = match &a[0] {
        Value::Text(s) => match crate::reg::relation_from_text(s, catalog)? {
            Some(pgmem_catalog::Relation::View(v)) => Some(v),
            _ => {
                let (schema, name) = s.rsplit_once('.').map_or((None, s.as_str()), |(sc, n)| (Some(sc), n));
                if let Some(d) = crate::catalog_views::system_relation(schema, name) {
                    return Ok(crate::catalog_views::system_viewdef(d.oid, pretty).map_or(Value::Null, Value::Text));
                }
                return Err(SqlError::undefined_table(s));
            }
        },
        other => match oid_arg(other) {
            Some(o) if crate::catalog_views::system_relation_by_oid(o).is_some() => {
                return Ok(crate::catalog_views::system_viewdef(o, pretty).map_or(Value::Null, Value::Text));
            }
            o => o.and_then(|o| catalog.views().find(|v| v.oid == o).cloned()),
        },
    };
    Ok(view.map_or(Value::Null, |v| {
        Value::Text(crate::deparse::with_pretty(pretty, || crate::deparse::deparse_view(catalog, &v)).unwrap_or_default())
    }))
}

/// `pg_get_serial_sequence('table', 'column')`: the sequence a serial or identity column
/// draws from, schema-qualified.
fn serial_sequence_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let catalog = ctx.txn.catalog();
    let table = match crate::reg::relation_from_text(text_arg(&a[0]), catalog)? {
        Some(pgmem_catalog::Relation::Table(t)) => t,
        _ => return Err(SqlError::undefined_table(text_arg(&a[0]))),
    };
    let column = text_arg(&a[1]);
    if table.column(column).is_none() {
        return Err(SqlError::undefined_column(&format!("\"{column}\" of relation \"{}\"", table.name)));
    }
    let seq = catalog.owned_sequences(&table).find(|s| s.owned_by.as_ref().is_some_and(|(_, c)| c == column));
    Ok(seq.map_or(Value::Null, |s| Value::Text(format!("{}.{}", quote_ident(&s.schema), quote_ident(&s.name)))))
}

/// The `classoid` a catalog name in `obj_description(oid, 'pg_class')` stands for.
fn catalog_class(name: &str) -> Option<u32> {
    Some(match name {
        "pg_class" => CLASS_CLASS_OID,
        "pg_constraint" => CONSTRAINT_CLASS_OID,
        "pg_namespace" => NAMESPACE_CLASS_OID,
        _ => return crate::catalog_views::system_relation(None, name).map(|d| d.oid),
    })
}

fn obj_description_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let catalog = ctx.txn.catalog();
    let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
    let text = match a.get(1) {
        Some(class) => catalog_class(text_arg(class)).and_then(|c| catalog.comment(c, o, 0)),
        None => catalog.comments().find(|((_, obj, sub), _)| *obj == o && *sub == 0).map(|(_, t)| t.as_str()),
    };
    Ok(text.map_or(Value::Null, |t| Value::Text(t.to_owned())))
}

fn col_description_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let (Some(o), Some(n)) = (oid_arg(&a[0]), a[1].as_i64()) else { return Ok(Value::Null) };
    let sub = i32::try_from(n).unwrap_or(-1);
    Ok(ctx.txn.catalog().comment(CLASS_CLASS_OID, o, sub).map_or(Value::Null, |t| Value::Text(t.to_owned())))
}

fn userbyid_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
    let name = match o {
        10 => ctx.env.user.clone(),
        6171 => "pg_database_owner".to_owned(),
        _ => format!("unknown (OID={o})"),
    };
    Ok(Value::Text(name))
}

fn to_reg_fn(kind: RegKind) -> crate::plan::ScalarFn {
    match kind {
        RegKind::Class => |a, ctx| Ok(crate::reg::to_reg(a[0].clone(), RegKind::Class, ctx.txn.catalog()).unwrap_or(Value::Null)),
        RegKind::Type => |a, ctx| Ok(crate::reg::to_reg(a[0].clone(), RegKind::Type, ctx.txn.catalog()).unwrap_or(Value::Null)),
        RegKind::Namespace => |a, ctx| Ok(crate::reg::to_reg(a[0].clone(), RegKind::Namespace, ctx.txn.catalog()).unwrap_or(Value::Null)),
        RegKind::Proc => |a, ctx| Ok(crate::reg::to_reg(a[0].clone(), RegKind::Proc, ctx.txn.catalog()).unwrap_or(Value::Null)),
        RegKind::Procedure => |a, ctx| Ok(crate::reg::to_reg(a[0].clone(), RegKind::Procedure, ctx.txn.catalog()).unwrap_or(Value::Null)),
    }
}

/// Relation sizes: pgmem keeps no pages, so tables report one 8 kB page once they have rows.
fn relation_size_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
    let size = match ctx.txn.catalog().relation_by_oid(o) {
        Some(pgmem_catalog::Relation::Table(t)) if !ctx.txn.scan(t.oid).is_empty() => 8192,
        Some(_) => 0,
        None => return Ok(Value::Null),
    };
    Ok(Value::Int8(size))
}

fn always_true(_: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(true))
}

fn always_false(_: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bool(false))
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    use Type::{Bool, Int4, Oid, Text};
    let n = args.len();
    let name_arg = Type::Name;
    match name {
        "format_type" if n == 2 => resolved(lenient("format_type", format_type_fn), vec![Oid, Int4], Text),
        "pg_get_function_arguments"
        | "pg_get_function_identity_arguments"
        | "pg_get_function_result"
        | "pg_get_functiondef"
        | "pg_get_triggerdef" => routine_text_function(name, n),
        "pg_get_constraintdef" if n == 1 || n == 2 => {
            resolved(strict("pg_get_constraintdef", constraintdef_fn), [Oid, Bool][..n].to_vec(), Text)
        }
        "pg_get_indexdef" if n == 1 || n == 3 => resolved(strict("pg_get_indexdef", indexdef_fn), [Oid, Int4, Bool][..n].to_vec(), Text),
        "pg_get_viewdef" if n == 1 || n == 2 => {
            let first = if args[0] == Type::Unknown || args[0].is_string() { Text } else { Oid };
            resolved(strict("pg_get_viewdef", viewdef_fn), [first, Bool][..n].to_vec(), Text)
        }
        // pgmem stores the deparsed expression as the node tree, so printing it is the identity.
        "pg_get_expr" if n == 2 || n == 3 => {
            resolved(strict("pg_get_expr", |a, _| Ok(a[0].clone())), [Text, Oid, Bool][..n].to_vec(), Text)
        }
        "pg_get_serial_sequence" if n == 2 => resolved(strict("pg_get_serial_sequence", serial_sequence_fn), vec![Text, Text], Text),
        "obj_description" if n == 1 || n == 2 => {
            resolved(strict("obj_description", obj_description_fn), [Oid, name_arg][..n].to_vec(), Text)
        }
        "col_description" if n == 2 => resolved(strict("col_description", col_description_fn), vec![Oid, Int4], Text),
        "shobj_description" if n == 2 => resolved(strict("shobj_description", |_, _| Ok(Value::Null)), vec![Oid, name_arg], Text),
        "pg_get_userbyid" if n == 1 => resolved(strict("pg_get_userbyid", userbyid_fn), vec![Oid], Type::Name),
        "pg_table_is_visible" if n == 1 => resolved(
            strict("pg_table_is_visible", |a, c| {
                let Some(o) = oid_arg(&a[0]) else { return Ok(Value::Null) };
                let cat = c.txn.catalog();
                Ok(match cat.relation_by_oid(o) {
                    Some(r) => Value::Bool(cat.relation_visible(r.schema(), r.name())),
                    None => Value::Null,
                })
            }),
            vec![Oid],
            Bool,
        ),
        "pg_type_is_visible" | "pg_function_is_visible" if n == 1 => resolved(strict("pg_type_is_visible", always_true), vec![Oid], Bool),
        "has_table_privilege" | "has_schema_privilege" | "has_sequence_privilege" | "pg_has_role" if n == 2 || n == 3 => {
            privilege_function(name, args)
        }
        "has_table_privilege"
        | "has_schema_privilege"
        | "has_database_privilege"
        | "has_column_privilege"
        | "has_sequence_privilege"
        | "has_function_privilege"
        | "has_any_column_privilege"
        | "pg_has_role"
            if (2..=4).contains(&n) =>
        {
            let targets = args.iter().map(|t| if *t == Type::Unknown { Text } else { t.clone() }).collect();
            resolved(strict("has_privilege", always_true), targets, Bool)
        }
        "to_regclass" if n == 1 => resolved(strict("to_regclass", to_reg_fn(RegKind::Class)), vec![Text], Type::Reg(RegKind::Class)),
        "to_regtype" if n == 1 => resolved(strict("to_regtype", to_reg_fn(RegKind::Type)), vec![Text], Type::Reg(RegKind::Type)),
        "to_regnamespace" if n == 1 => {
            resolved(strict("to_regnamespace", to_reg_fn(RegKind::Namespace)), vec![Text], Type::Reg(RegKind::Namespace))
        }
        "to_regproc" if n == 1 => resolved(strict("to_regproc", to_reg_fn(RegKind::Proc)), vec![Text], Type::Reg(RegKind::Proc)),
        "to_regprocedure" if n == 1 => {
            resolved(strict("to_regprocedure", to_reg_fn(RegKind::Procedure)), vec![Text], Type::Reg(RegKind::Procedure))
        }
        "pg_relation_size" | "pg_total_relation_size" | "pg_table_size" | "pg_indexes_size" if n == 1 => {
            resolved(strict("pg_relation_size", relation_size_fn), vec![Type::Reg(RegKind::Class)], Type::Int8)
        }
        "pg_is_in_recovery" if n == 0 => resolved(lenient("pg_is_in_recovery", always_false), vec![], Bool),
        "pg_encoding_to_char" if n == 1 => resolved(
            strict("pg_encoding_to_char", |a, _| Ok(Value::Text(if a[0].as_i64() == Some(6) { "UTF8" } else { "" }.to_owned()))),
            vec![Int4],
            Type::Name,
        ),
        "pg_char_to_encoding" if n == 1 => resolved(
            strict("pg_char_to_encoding", |a, _| Ok(Value::Int4(if text_arg(&a[0]).eq_ignore_ascii_case("utf8") { 6 } else { -1 }))),
            vec![Type::Name],
            Int4,
        ),
        "pg_postmaster_start_time" | "pg_conf_load_time" if n == 0 => {
            resolved(lenient("pg_postmaster_start_time", |_, ctx| Ok(Value::Timestamptz(ctx.env.txn_start))), vec![], Type::Timestamptz)
        }
        "inet_server_port" | "inet_client_port" if n == 0 => resolved(lenient("inet_server_port", |_, _| Ok(Value::Null)), vec![], Int4),
        "pg_cancel_backend" | "pg_terminate_backend" if n == 1 => resolved(strict("pg_cancel_backend", always_false), vec![Int4], Bool),
        _ => {
            let _ = code::UNDEFINED_FUNCTION;
            Ok(None)
        }
    }
}
