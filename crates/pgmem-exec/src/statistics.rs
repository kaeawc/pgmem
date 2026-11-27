//! Extended statistics objects: CREATE / ALTER / DROP STATISTICS (`statscmds.c`) and their
//! deparse (`pg_get_statisticsobjdef`). pgmem keeps only the catalog entry: no statistics are
//! gathered.

use crate::bind::Binder;
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, StatisticsDef, TableDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, Type, code};
use std::sync::Arc;

/// `pg_statistic_ext`'s class OID (the `classoid` of comments on statistics objects).
pub const STATISTIC_EXT_CLASS_OID: u32 = 3381;

/// The most keys (columns and expressions) one statistics object may have.
const MAX_KEYS: usize = 8;

fn done(tag: &str) -> QueryResult {
    QueryResult { tag: tag.to_owned(), ..Default::default() }
}

fn message(tag: &str, severity: &'static str, text: String) -> QueryResult {
    QueryResult { tag: tag.to_owned(), notices: vec![Notice { severity, code: "00000", message: text }], ..Default::default() }
}

/// Types without a default btree operator class cannot be used in statistics.
fn has_btree_opclass(t: &Type) -> bool {
    !matches!(t.base(), Type::Json | Type::JsonPath)
}

/// Resolves a statistics object name (qualified, or through the search path).
pub(crate) fn lookup(cat: &Catalog, name: &ir::ObjectName) -> SqlResult<Option<Arc<StatisticsDef>>> {
    cat.resolve(name, |schema, n| cat.find_statistics(schema, n).cloned())
}

pub(crate) fn missing(name: &ir::ObjectName) -> SqlError {
    SqlError::new(code::UNDEFINED_OBJECT, format!("statistics object \"{name}\" does not exist"))
}

/// `ChooseExtendedStatisticName`: `<table>_<keys>_stat`, numbered when taken.
fn choose_name(cat: &Catalog, schema: &str, table: &str, keys: &[String]) -> String {
    let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
    pgmem_catalog::choose_object_name(table, &keys, "stat", |n| cat.find_statistics(schema, n).is_none())
}

/// CREATE STATISTICS.
pub(crate) fn create_statistics(cs: &ir::CreateStatistics, txn: &mut Txn, binder: &Binder, env: &SessionEnv) -> SqlResult<QueryResult> {
    let tag = "CREATE STATISTICS";
    let cat = txn.catalog().clone();
    let [rel_name] = cs.tables.as_slice() else {
        return Err(SqlError::feature_not_supported("only a single relation is allowed in CREATE STATISTICS"));
    };
    let table = match cat.lookup_relation(rel_name)? {
        Some(pgmem_catalog::Relation::Table(t)) => t,
        Some(other) => {
            let what = match other {
                pgmem_catalog::Relation::View(_) => "views",
                pgmem_catalog::Relation::Sequence(_) => "sequences",
                _ => "indexes",
            };
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("cannot define statistics for relation \"{}\"", other.name()))
                .with_detail(format!("This operation is not supported for {what}.")));
        }
        None => return Err(SqlError::undefined_table(&rel_name.to_string())),
    };
    // Names, in statement order, for a generated object name.
    let key_names: Vec<String> = cs
        .keys
        .iter()
        .map(|k| match k {
            ir::StatisticsKey::Column(c) => c.clone(),
            ir::StatisticsKey::Expr(_) => "expr".to_owned(),
        })
        .collect();
    let (schema, name) = match &cs.name {
        Some(n) => {
            let schema = cat.creation_schema(n)?;
            if cat.find_statistics(&schema, n.base()).is_some() {
                let text = format!("statistics object \"{}\" already exists", n.base());
                if cs.if_not_exists {
                    return Ok(message(tag, "NOTICE", format!("{text}, skipping")));
                }
                return Err(SqlError::new(code::DUPLICATE_OBJECT, text));
            }
            (schema, n.base().to_owned())
        }
        None => (table.schema.clone(), choose_name(&cat, &table.schema, &table.name, &key_names)),
    };
    if cs.keys.len() > MAX_KEYS {
        return Err(SqlError::new("54011", "cannot have more than 8 columns in statistics"));
    }
    let mut columns = Vec::new();
    let mut exprs: Vec<ir::Expr> = Vec::new();
    let mut expr_texts: Vec<String> = Vec::new();
    for key in &cs.keys {
        match key {
            ir::StatisticsKey::Column(c) => {
                let Some(slot) = table.column_index(c) else {
                    if crate::bind::SYSTEM_COLUMNS.iter().any(|(n, _)| n == c) {
                        return Err(SqlError::feature_not_supported("statistics creation on system columns is not supported"));
                    }
                    return Err(SqlError::undefined_column(&format!("\"{c}\"")));
                };
                let ty = &table.columns[slot].ty;
                if !has_btree_opclass(ty) {
                    return Err(SqlError::feature_not_supported(format!(
                        "column \"{c}\" cannot be used in statistics because its type {} has no default btree operator class",
                        ty.name()
                    )));
                }
                columns.push(slot);
            }
            ir::StatisticsKey::Expr(e) => {
                let ty = crate::bind::ddl::bind_catalog_expr(binder, &table, e, "statistics expressions")?;
                if !has_btree_opclass(&ty) {
                    return Err(SqlError::feature_not_supported(format!(
                        "expression cannot be used in multivariate statistics because its type {} has no default btree operator class",
                        ty.name()
                    )));
                }
                expr_texts.push(crate::deparse::deparse_expr(&cat, &table, e)?);
                exprs.push(e.clone());
            }
        }
    }
    if !cs.kinds.is_empty() && columns.is_empty() && exprs.len() == 1 {
        return Err(SqlError::feature_not_supported(
            "when building statistics on a single expression, statistics kinds may not be specified",
        ));
    }
    let (mut d, mut f, mut m) = (cs.kinds.is_empty(), cs.kinds.is_empty(), cs.kinds.is_empty());
    for k in &cs.kinds {
        match k.as_str() {
            "ndistinct" => d = true,
            "dependencies" => f = true,
            "mcv" => m = true,
            other => return Err(SqlError::syntax(format!("unrecognized statistics kind \"{other}\""))),
        }
    }
    if cs.keys.len() < 2 && exprs.len() != 1 {
        return Err(SqlError::new(code::INVALID_OBJECT_DEFINITION, "extended statistics require at least 2 columns"));
    }
    columns.sort_unstable();
    if columns.windows(2).any(|w| w[0] == w[1]) {
        return Err(SqlError::new(code::DUPLICATE_COLUMN, "duplicate column name in statistics definition"));
    }
    for (i, t) in expr_texts.iter().enumerate() {
        if expr_texts[..i].contains(t) {
            return Err(SqlError::new(code::DUPLICATE_COLUMN, "duplicate expression in statistics definition"));
        }
    }
    // Expressions alone (one) build no multivariate statistics.
    let multivariate = !(columns.is_empty() && exprs.len() == 1);
    let mut kinds = Vec::new();
    if multivariate {
        for (on, c) in [(d, 'd'), (f, 'f'), (m, 'm')] {
            if on {
                kinds.push(c);
            }
        }
    }
    if !exprs.is_empty() {
        kinds.push('e');
    }
    let oid = txn.catalog_mut().alloc_oid();
    let role = crate::roles::current_role(env);
    let cat = txn.catalog_mut();
    cat.put_statistics(StatisticsDef { oid, name, schema, table: table.oid, columns, exprs, kinds, target: None });
    if role != env.user {
        cat.set_owner(oid, Some(&role));
    }
    Ok(done(tag))
}

/// ALTER STATISTICS.
pub(crate) fn alter_statistics(
    name: &ir::ObjectName,
    if_exists: bool,
    action: &ir::AlterStatisticsAction,
    txn: &mut Txn,
    env: &SessionEnv,
) -> SqlResult<QueryResult> {
    let tag = "ALTER STATISTICS";
    let cat = txn.catalog().clone();
    let Some(st) = lookup(&cat, name)? else {
        if if_exists {
            return Ok(message(tag, "NOTICE", format!("statistics object \"{name}\" does not exist, skipping")));
        }
        return Err(missing(name));
    };
    let taken = |schema: &str, n: &str| {
        SqlError::new(code::DUPLICATE_OBJECT, format!("statistics object \"{n}\" already exists in schema \"{schema}\""))
    };
    let mut def = (*st).clone();
    let mut result = done(tag);
    match action {
        ir::AlterStatisticsAction::RenameTo(new) => {
            if cat.find_statistics(&st.schema, new).is_some() {
                return Err(taken(&st.schema, new));
            }
            def.name.clone_from(new);
        }
        ir::AlterStatisticsAction::SetSchema(schema) => {
            let schema = cat.resolve_schema_name(schema);
            if !cat.schema_exists(&schema) {
                return Err(pgmem_catalog::undefined_schema(&schema));
            }
            if cat.find_statistics(&schema, &st.name).is_some_and(|o| o.oid != st.oid) {
                return Err(taken(&schema, &st.name));
            }
            def.schema = schema;
        }
        ir::AlterStatisticsAction::SetStatistics(n) => {
            if *n < -1 {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("statistics target {n} is too low")));
            }
            let n = if *n > 10_000 {
                result.notices.push(Notice { severity: "WARNING", code: "01000", message: "lowering statistics target to 10000".into() });
                10_000
            } else {
                *n
            };
            def.target = (n >= 0).then(|| i16::try_from(n).unwrap_or(i16::MAX));
        }
        ir::AlterStatisticsAction::OwnerTo(owner) => {
            crate::roles::set_owner(txn, env, st.oid, owner)?;
            return Ok(result);
        }
    }
    txn.catalog_mut().put_statistics(def);
    Ok(result)
}

/// DROP STATISTICS.
pub(crate) fn drop_statistics(names: &[ir::ObjectName], if_exists: bool, txn: &mut Txn) -> SqlResult<QueryResult> {
    let tag = "DROP STATISTICS";
    let mut notices = Vec::new();
    for name in names {
        let found = match lookup(txn.catalog(), name) {
            Ok(f) => f,
            Err(e) if if_exists && e.code == code::INVALID_SCHEMA_NAME => {
                notices.push(Notice { severity: "NOTICE", code: "00000", message: format!("{}, skipping", e.message) });
                continue;
            }
            Err(e) => return Err(e),
        };
        match found {
            Some(st) => {
                txn.catalog_mut().drop_statistics(st.oid);
            }
            None if if_exists => notices.push(Notice {
                severity: "NOTICE",
                code: "00000",
                message: format!("statistics object \"{name}\" does not exist, skipping"),
            }),
            None => return Err(missing(name)),
        }
    }
    Ok(QueryResult { tag: tag.into(), notices, ..Default::default() })
}

/// The table a statistics object is on.
fn table_of<'a>(cat: &'a Catalog, st: &StatisticsDef) -> Option<&'a Arc<TableDef>> {
    cat.table_by_oid(st.table)
}

/// The expressions' text: as `pg_get_expr` prints them (`pretty` off) or as the statistics
/// definition does (`lower(c)`, `(a + b)`).
fn expr_texts(cat: &Catalog, table: &TableDef, st: &StatisticsDef, definition: bool) -> Vec<String> {
    st.exprs
        .iter()
        .map(|e| {
            let text = crate::deparse::with_pretty(definition, || crate::deparse::deparse_expr(cat, table, e).unwrap_or_default());
            let function = matches!(e, ir::Expr::Function(_));
            if definition && !function { format!("({text})") } else { text }
        })
        .collect()
}

/// `a, b, (a + b)`: the columns, then the expressions.
fn key_list(cat: &Catalog, table: &TableDef, st: &StatisticsDef) -> String {
    let mut items: Vec<String> =
        st.columns.iter().map(|&i| crate::deparse::quote_ident(table.columns.get(i).map_or("", |c| c.name.as_str()))).collect();
    items.extend(expr_texts(cat, table, st, true));
    items.join(", ")
}

/// `pg_get_statisticsobjdef(oid)`.
pub fn definition(cat: &Catalog, oid: u32) -> Option<String> {
    let st = cat.statistics_by_oid(oid)?;
    let table = table_of(cat, st)?;
    let q = crate::deparse::quote_ident;
    let (d, f, m) = (st.kinds.contains(&'d'), st.kinds.contains(&'f'), st.kinds.contains(&'m'));
    let mut kinds = Vec::new();
    for (on, n) in [(d, "ndistinct"), (f, "dependencies"), (m, "mcv")] {
        if on {
            kinds.push(n);
        }
    }
    let kind_list = if !kinds.is_empty() && kinds.len() < 3 { format!(" ({})", kinds.join(", ")) } else { String::new() };
    let relation = if crate::reg::relation_visible(cat, &table.schema, &table.name) {
        q(&table.name)
    } else {
        format!("{}.{}", q(&table.schema), q(&table.name))
    };
    Some(format!("CREATE STATISTICS {}.{}{kind_list} ON {} FROM {relation}", q(&st.schema), q(&st.name), key_list(cat, table, st)))
}

/// `pg_get_statisticsobjdef_columns(oid)`.
pub fn columns_text(cat: &Catalog, oid: u32) -> Option<String> {
    let st = cat.statistics_by_oid(oid)?;
    Some(key_list(cat, table_of(cat, st)?, st))
}

/// `pg_get_statisticsobjdef_expressions(oid)`: None when the object has no expressions.
pub fn expressions(cat: &Catalog, oid: u32) -> Option<Vec<String>> {
    let st = cat.statistics_by_oid(oid)?;
    let table = table_of(cat, st)?;
    (!st.exprs.is_empty()).then(|| expr_texts(cat, table, st, false))
}

/// Statistics objects that read a column (DROP COLUMN drops them).
pub(crate) fn drop_for_column(txn: &mut Txn, table: &TableDef, slot: usize, column: &str) {
    let users: Vec<u32> = txn
        .catalog()
        .statistics()
        .filter(|s| s.table == table.oid && (s.columns.contains(&slot) || s.exprs.iter().any(|e| mentions_column(e, column))))
        .map(|s| s.oid)
        .collect();
    for oid in users {
        txn.catalog_mut().drop_statistics(oid);
    }
}

/// Renames a column inside the expressions of the table's statistics objects.
pub(crate) fn rename_column(txn: &mut Txn, table: &TableDef, old: &str, new: &str) {
    let all: Vec<StatisticsDef> = txn.catalog().statistics().filter(|s| s.table == table.oid).map(|s| (**s).clone()).collect();
    for mut st in all {
        for e in &mut st.exprs {
            crate::ddl::rename_in_expr(e, &table.name, old, new);
        }
        txn.catalog_mut().put_statistics(st);
    }
}

fn mentions_column(e: &ir::Expr, column: &str) -> bool {
    let mut probe = e.clone();
    crate::ddl::rename_in_expr(&mut probe, "", column, "\0renamed");
    probe != *e
}
