//! Storage parameters (`reloptions`): validation and merging for `WITH (...)`, `SET (...)` and
//! `RESET (...)`, following Postgres' reloptions.c (names, ranges, messages, ordering).

use pgmem_ir::RelOption;
use pgmem_types::{SqlError, SqlResult, code};

/// What carries the options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target<'a> {
    /// A heap table or materialized view (and its `toast.` options).
    Heap,
    /// An index of access method `.0` (None: unknown, any method's options are accepted).
    Index(Option<&'a str>),
    View,
}

enum Kind {
    Int(i64, i64),
    Real(f64, f64),
    Bool,
    /// Accepted values (compared case-insensitively) and the list in the error detail.
    Enum(&'static [&'static str], &'static str),
}

const I32_MAX: i64 = i32::MAX as i64;
const VACUUM_CLEANUP: &[&str] = &["on", "off", "auto", "true", "false", "yes", "no", "1", "0"];
const ON_OFF_AUTO: &[&str] = &["on", "off", "auto"];

/// Options an autovacuum-able relation (a table or its TOAST table) takes.
fn vacuum_option(name: &str) -> Option<Kind> {
    use Kind::{Bool, Enum, Int, Real};
    Some(match name {
        "autovacuum_enabled" | "vacuum_truncate" => Bool,
        "autovacuum_vacuum_threshold" => Int(0, I32_MAX),
        "autovacuum_vacuum_max_threshold" | "autovacuum_vacuum_insert_threshold" | "log_autovacuum_min_duration" => Int(-1, I32_MAX),
        "autovacuum_vacuum_cost_limit" => Int(1, 10000),
        "autovacuum_freeze_min_age" | "autovacuum_multixact_freeze_min_age" => Int(0, 1_000_000_000),
        "autovacuum_freeze_max_age" => Int(100_000, 2_000_000_000),
        "autovacuum_freeze_table_age" | "autovacuum_multixact_freeze_table_age" => Int(0, 2_000_000_000),
        "autovacuum_multixact_freeze_max_age" => Int(10_000, 2_000_000_000),
        "autovacuum_vacuum_cost_delay" | "autovacuum_vacuum_scale_factor" | "autovacuum_vacuum_insert_scale_factor" => Real(0.0, 100.0),
        "vacuum_max_eager_freeze_failure_rate" => Real(0.0, 1.0),
        "vacuum_index_cleanup" => Enum(VACUUM_CLEANUP, "\"on\", \"off\", and \"auto\""),
        _ => return None,
    })
}

fn heap_option(name: &str) -> Option<Kind> {
    use Kind::{Bool, Int, Real};
    match name {
        "fillfactor" => Some(Int(10, 100)),
        "toast_tuple_target" => Some(Int(128, 8160)),
        "parallel_workers" => Some(Int(0, 1024)),
        "user_catalog_table" => Some(Bool),
        "autovacuum_analyze_threshold" => Some(Int(0, I32_MAX)),
        "autovacuum_analyze_scale_factor" => Some(Real(0.0, 100.0)),
        _ => vacuum_option(name),
    }
}

fn index_option(method: &str, name: &str) -> Option<Kind> {
    use Kind::{Bool, Enum, Int, Real};
    match (method, name) {
        ("btree" | "hash" | "gist" | "spgist", "fillfactor") => Some(Int(10, 100)),
        ("btree", "deduplicate_items") | ("gin", "fastupdate") | ("brin", "autosummarize") => Some(Bool),
        ("btree", "vacuum_cleanup_index_scale_factor") => Some(Real(0.0, 1e10)),
        ("gist", "buffering") => Some(Enum(ON_OFF_AUTO, "\"on\", \"off\", and \"auto\"")),
        ("gin", "gin_pending_list_limit") => Some(Int(64, I32_MAX)),
        ("brin", "pages_per_range") => Some(Int(1, 131_072)),
        _ => None,
    }
}

fn view_option(name: &str) -> Option<Kind> {
    match name {
        "security_barrier" | "security_invoker" => Some(Kind::Bool),
        "check_option" => Some(Kind::Enum(&["local", "cascaded"], "\"local\" and \"cascaded\"")),
        _ => None,
    }
}

fn lookup(target: Target, name: &str) -> Option<Kind> {
    match target {
        Target::Heap => heap_option(name),
        Target::View => view_option(name),
        Target::Index(Some(m)) => index_option(m, name),
        Target::Index(None) => ["btree", "hash", "gist", "gin", "spgist", "brin"].iter().find_map(|m| index_option(m, name)),
    }
}

fn bounds_error(name: &str, text: &str, lo: &str, hi: &str) -> SqlError {
    SqlError::new(code::INVALID_PARAMETER_VALUE, format!("value {text} out of bounds for option \"{name}\""))
        .with_detail(format!("Valid values are between \"{lo}\" and \"{hi}\"."))
}

fn invalid(what: &str, name: &str, text: &str) -> SqlError {
    SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for {what} option \"{name}\": {text}"))
}

/// reloptions.c `parse_int`: an integer (decimal or `0x` hex), or a number rounded to one.
fn parse_int(text: &str) -> Option<i64> {
    let t = text.trim();
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    let neg = t.starts_with('-');
    let whole = if let Some(hex) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        digits.parse::<i64>().ok()
    } else {
        None
    };
    let v = match whole {
        Some(v) if neg => -v,
        Some(v) => v,
        None => {
            let f: f64 = t.parse().ok().filter(|f: &f64| f.is_finite())?;
            let r = f.round_ties_even();
            if !(-2_147_483_648.0..=2_147_483_647.0).contains(&r) {
                return None;
            }
            #[allow(clippy::cast_possible_truncation)]
            {
                r as i64
            }
        }
    };
    (i64::from(i32::MIN)..=I32_MAX).contains(&v).then_some(v)
}

/// Whether boolean option `name` is set to true in `reloptions` (`name=value` entries).
pub(crate) fn bool_option(reloptions: &[String], name: &str) -> bool {
    reloptions.iter().any(|o| o.split_once('=').is_some_and(|(n, v)| n == name && parse_bool(v) == Some(true)))
}

/// `parse_bool`: `true`/`false`/`yes`/`no`/`on`/`off`/`1`/`0`, the words by unique prefix.
pub(crate) fn parse_bool(text: &str) -> Option<bool> {
    let t = text.trim().to_ascii_lowercase();
    let prefix = |full: &str, min: usize| t.len() >= min && full.starts_with(&t);
    if prefix("true", 1) || prefix("yes", 1) || t == "on" || t == "1" {
        Some(true)
    } else if prefix("false", 1) || prefix("no", 1) || prefix("off", 2) || t == "0" {
        Some(false)
    } else {
        None
    }
}

fn validate_value(name: &str, kind: &Kind, text: &str) -> SqlResult<()> {
    match kind {
        Kind::Int(lo, hi) => {
            let v = parse_int(text).ok_or_else(|| invalid("integer", name, text))?;
            if v < *lo || v > *hi {
                return Err(bounds_error(name, text, &lo.to_string(), &hi.to_string()));
            }
        }
        Kind::Real(lo, hi) => {
            let v: f64 = text.trim().parse().ok().filter(|f: &f64| !f.is_nan()).ok_or_else(|| invalid("floating point", name, text))?;
            if v < *lo || v > *hi {
                return Err(bounds_error(name, text, &format!("{lo:.6}"), &format!("{hi:.6}")));
            }
        }
        Kind::Bool => {
            parse_bool(text).ok_or_else(|| invalid("boolean", name, text))?;
        }
        Kind::Enum(values, detail) => {
            if !values.iter().any(|v| v.eq_ignore_ascii_case(text)) {
                return Err(invalid("enum", name, text).with_detail(format!("Valid values are {detail}.")));
            }
        }
    }
    Ok(())
}

fn unrecognized(name: &str) -> SqlError {
    SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized parameter \"{name}\""))
}

/// Validates a SET / WITH list against `target`; the result holds the entries to store
/// (`name`, `value`). TOAST-namespace options are validated and dropped (there is no TOAST
/// relation to keep them on).
fn validate(target: Target, opts: &[RelOption]) -> SqlResult<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for o in opts {
        let text = o.value.clone().unwrap_or_else(|| "true".into());
        match o.namespace.as_deref() {
            None => {
                let kind = lookup(target, &o.name).ok_or_else(|| unrecognized(&o.name))?;
                validate_value(&o.name, &kind, &text)?;
                if out.iter().any(|(n, _)| *n == o.name) {
                    return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("parameter \"{}\" specified more than once", o.name)));
                }
                out.push((o.name.clone(), text));
            }
            Some("toast") if target == Target::Heap => {
                // Only the autovacuum-style options exist on a TOAST table.
                let kind = vacuum_option(&o.name).ok_or_else(|| unrecognized(&o.name))?;
                validate_value(&o.name, &kind, &text)?;
            }
            // Views ignore options in the toast namespace.
            Some("toast") if target == Target::View => {}
            Some(ns) => {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized parameter namespace \"{ns}\"")));
            }
        }
    }
    Ok(out)
}

fn key_of(entry: &str) -> &str {
    entry.split_once('=').map_or(entry, |(k, _)| k)
}

/// The options after `SET (opts)` (`reset: false`) or `RESET (opts)` (`reset: true`) on a
/// relation holding `old`: Postgres' `transformRelOptions` keeps the old entries not
/// mentioned, then appends the new ones.
pub fn alter(target: Target, old: &[String], opts: &[RelOption], reset: bool) -> SqlResult<Vec<String>> {
    if reset {
        if opts.iter().any(|o| o.value.is_some()) {
            return Err(SqlError::syntax("RESET must not include values for parameters"));
        }
        return Ok(old.iter().filter(|e| !opts.iter().any(|o| o.namespace.is_none() && o.name == key_of(e))).cloned().collect());
    }
    let new = validate(target, opts)?;
    let mut out: Vec<String> = old.iter().filter(|e| !new.iter().any(|(n, _)| n == key_of(e))).cloned().collect();
    out.extend(new.into_iter().map(|(n, v)| format!("{n}={v}")));
    Ok(out)
}

/// The options of a new table: `oids=false` is accepted and dropped, a partitioned table takes
/// none.
pub fn table(opts: &[RelOption], partitioned: bool) -> SqlResult<Vec<String>> {
    let mut rest = Vec::with_capacity(opts.len());
    for o in opts {
        if o.namespace.is_none() && o.name == "oids" {
            if o.value.as_deref().is_none_or(|v| parse_bool(v) != Some(false)) {
                return Err(SqlError::feature_not_supported("tables declared WITH OIDS are not supported"));
            }
        } else {
            rest.push(o.clone());
        }
    }
    if partitioned && !rest.is_empty() {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, "cannot specify storage parameters for a partitioned table")
            .with_hint("Specify storage parameters for its leaf partitions instead."));
    }
    create(Target::Heap, &rest)
}

/// The options a new relation starts with.
pub fn create(target: Target, opts: &[RelOption]) -> SqlResult<Vec<String>> {
    alter(target, &[], opts, false)
}

/// The `WITH (...)` list of `pg_get_indexdef`: a value is bare only when it needs no
/// identifier quoting, else a quoted literal.
pub fn flatten(options: &[String]) -> String {
    use crate::deparse::pg::{quote_ident, quote_literal};
    options
        .iter()
        .map(|e| {
            let (k, v) = e.split_once('=').unwrap_or((e, ""));
            let value = if quote_ident(v) == v { v.to_owned() } else { quote_literal(v) };
            format!("{}={value}", quote_ident(k))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A `pg_class.reloptions` value.
pub fn as_value(options: &[String]) -> pgmem_types::Value {
    if options.is_empty() {
        pgmem_types::Value::Null
    } else {
        pgmem_types::Value::Array(options.iter().cloned().map(pgmem_types::Value::Text).collect())
    }
}

/// The command tag of `ALTER <object> ... SET / RESET (...)`.
pub(crate) fn alter_tag(object: pgmem_ir::ObjectType) -> &'static str {
    use pgmem_ir::ObjectType as O;
    match object {
        O::Index => "ALTER INDEX",
        O::View => "ALTER VIEW",
        O::MaterializedView => "ALTER MATERIALIZED VIEW",
        _ => "ALTER TABLE",
    }
}

/// `SET (options)` / `RESET (options)` on a table or materialized view.
pub(crate) fn alter_table(txn: &mut pgmem_storage::Txn, t: &pgmem_catalog::TableDef, reset: bool, options: &[RelOption]) -> SqlResult<()> {
    if !reset && txn.catalog().is_partitioned(t.oid) {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, "cannot specify storage parameters for a partitioned table")
            .with_hint("Specify storage parameters for its leaf partitions instead."));
    }
    let mut def = t.clone();
    def.reloptions = alter(Target::Heap, &t.reloptions, options, reset)?;
    txn.catalog_mut().replace_table(&t.qualified(), def);
    Ok(())
}

/// `ALTER {TABLE | INDEX | VIEW | MATERIALIZED VIEW} name {SET | RESET} (options)`.
pub(crate) fn alter_relation(
    object: pgmem_ir::ObjectType,
    name: &pgmem_ir::ObjectName,
    if_exists: bool,
    reset: bool,
    options: &[RelOption],
    txn: &mut pgmem_storage::Txn,
) -> SqlResult<crate::session::QueryResult> {
    use pgmem_catalog::Relation;
    use pgmem_ir::ObjectType as O;
    let tag = alter_tag(object);
    let done = crate::session::QueryResult { tag: tag.to_owned(), ..Default::default() };
    let wrong = |what: &str| SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not {what}", name.base()));
    let Some(rel) = txn.catalog().find_relation(name)? else {
        if !if_exists {
            return Err(SqlError::undefined_table(&name.to_string()));
        }
        return Ok(crate::session::QueryResult {
            notices: vec![crate::session::Notice {
                severity: "NOTICE",
                code: "00000",
                message: format!("relation \"{}\" does not exist, skipping", name.base()),
            }],
            ..done
        });
    };
    match rel {
        Relation::Table(t) if matches!(object, O::Table) || (object == O::MaterializedView && t.matview.is_some()) => {
            alter_table(txn, &t, reset, options)?;
        }
        Relation::View(v) if matches!(object, O::Table | O::View) => {
            let mut def = (*v).clone();
            def.reloptions = alter(Target::View, &v.reloptions, options, reset)?;
            def.check_option = def.reloptions.iter().any(|o| o.starts_with("check_option="));
            if !reset && def.check_option && options.iter().any(|o| o.name == "check_option") {
                crate::views::check_option_allowed(txn.catalog(), &def)?;
            }
            // Replaced in place: the view keeps its OID, grants, owner and triggers.
            txn.catalog_mut().create_view(def, true)?;
        }
        Relation::Index { table, index } if matches!(object, O::Table | O::Index) => {
            let Some(t) = txn.catalog().table_q(&table).cloned() else { return Err(SqlError::undefined_table(&table.name)) };
            let mut def = (*t).clone();
            let new = alter(Target::Index(None), &index.reloptions, options, reset)?;
            for i in def.indexes.iter_mut().filter(|i| i.oid == index.oid) {
                i.reloptions.clone_from(&new);
            }
            txn.catalog_mut().replace_table(&table, def);
        }
        Relation::Sequence(_) if object == O::Table => {
            return Err(SqlError::new(
                code::WRONG_OBJECT_TYPE,
                format!("ALTER action {} cannot be performed on relation \"{}\"", if reset { "RESET" } else { "SET" }, name.base()),
            )
            .with_detail("This operation is not supported for sequences."));
        }
        _ => {
            return Err(wrong(match object {
                O::Index => "an index",
                O::View => "a view",
                O::MaterializedView => "a materialized view",
                _ => "a table",
            }));
        }
    }
    Ok(done)
}
