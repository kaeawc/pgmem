//! CREATE [OR REPLACE] VIEW, materialized views (CREATE / REFRESH), ALTER VIEW RENAME.

use crate::bind::{self, Binder, Ctx, Level, Scope};
use crate::eval::{EvalCtx, StatementState};
use crate::run::run;
use crate::session::{QueryResult, SessionEnv};
use pgmem_catalog::{ColumnDef, MatView, QualName, Relation, TableDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

/// CREATE [OR REPLACE] VIEW. Replacing keeps the view's OID and may only add columns at the
/// end: the existing ones keep their names and types (42P16).
pub(crate) fn create_view(cv: &ir::CreateView, txn: &mut Txn, binder: &Binder) -> SqlResult<QueryResult> {
    let mut def = bind::bind_create_view(binder, cv)?;
    let existing = txn.catalog().view(&def.schema, &def.name).cloned();
    match existing {
        Some(old) if cv.or_replace => {
            let old_types = view_column_types(binder, &old)?;
            let new_bound = bind_query_types(binder, &cv.query)?;
            if def.columns.len() < old.columns.len() {
                return Err(SqlError::new(code::INVALID_TABLE_DEFINITION, "cannot drop columns from view"));
            }
            for (i, (old_name, old_ty)) in old.columns.iter().zip(&old_types).enumerate() {
                if def.columns[i] != *old_name {
                    return Err(SqlError::new(
                        code::INVALID_TABLE_DEFINITION,
                        format!("cannot change name of view column \"{old_name}\" to \"{}\"", def.columns[i]),
                    )
                    .with_hint("Use ALTER VIEW ... RENAME COLUMN ... to change name of view column instead."));
                }
                if let Some(new_ty) = new_bound.get(i)
                    && new_ty != old_ty
                {
                    return Err(SqlError::new(
                        code::INVALID_TABLE_DEFINITION,
                        format!("cannot change data type of view column \"{old_name}\" from {} to {}", old_ty.name(), new_ty.name()),
                    ));
                }
            }
            def.oid = old.oid;
        }
        _ => def.oid = txn.catalog_mut().alloc_oid(),
    }
    txn.catalog_mut().create_view(def, cv.or_replace)?;
    Ok(tag("CREATE VIEW"))
}

fn bind_query_types(binder: &Binder, q: &ir::Query) -> SqlResult<Vec<pgmem_types::Type>> {
    let top = Level::new(Scope::default());
    Ok(binder.bind_query(q, &Ctx::new(&top))?.columns.into_iter().map(|c| c.ty).collect())
}

fn view_column_types(binder: &Binder, v: &pgmem_catalog::ViewDef) -> SqlResult<Vec<pgmem_types::Type>> {
    pgmem_types::context::with_search_path(&v.search_path, || bind_query_types(binder, &v.query))
}

/// The columns a CREATE TABLE AS / materialized view gets from its query: named by `names`
/// first, then by the query; unknown-typed literals become text.
fn query_columns(names: &[String], bound: &[crate::plan::OutCol]) -> SqlResult<Vec<ColumnDef>> {
    if names.len() > bound.len() {
        return Err(SqlError::syntax("too many column names were specified"));
    }
    let mut columns: Vec<ColumnDef> = Vec::with_capacity(bound.len());
    for (i, c) in bound.iter().enumerate() {
        let ty = if c.ty == pgmem_types::Type::Unknown { pgmem_types::Type::Text } else { c.ty.clone() };
        let name = names.get(i).cloned().unwrap_or_else(|| c.name.clone());
        if columns.iter().any(|x| x.name == name) {
            return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{name}\" specified more than once")));
        }
        let mut col = ColumnDef::new(name, ty);
        col.typmod = c.typmod;
        columns.push(col);
    }
    Ok(columns)
}

/// Creates the table for a query result and fills it (`with_data`); returns the row count.
#[allow(clippy::too_many_arguments)]
fn create_from_query(
    name: &ir::ObjectName,
    names: &[String],
    query: &ir::Query,
    with_data: bool,
    matview: bool,
    unlogged: bool,
    reloptions: Vec<String>,
    txn: &mut Txn,
    binder: &Binder,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<usize> {
    let schema = binder.catalog.creation_schema(name)?;
    let base = name.base().to_owned();
    let top = Level::new(Scope::default());
    let bound = binder.bind_query(query, &Ctx::new(&top))?;
    let columns = query_columns(names, &bound.columns)?;
    let rows = if with_data { run(&bound.rel, &EvalCtx::new(txn, &[], env, state))? } else { Vec::new() };
    let oid = txn.catalog_mut().alloc_oid();
    let matview = matview
        .then(|| Box::new(MatView { query: query.clone(), search_path: binder.catalog.resolved_search_path(), populated: with_data }));
    let def = TableDef { oid, schema, name: base, columns, constraints: Vec::new(), indexes: Vec::new(), matview, unlogged, reloptions };
    txn.catalog_mut().create_table(def)?;
    txn.created_table(oid);
    let count = rows.len();
    for row in rows {
        txn.insert(oid, row);
    }
    Ok(count)
}

fn already_exists(binder: &Binder, name: &ir::ObjectName) -> SqlResult<Option<String>> {
    let schema = binder.catalog.creation_schema(name)?;
    let base = name.base();
    Ok(binder.catalog.relation(&schema, base).is_some().then(|| format!("relation \"{base}\" already exists")))
}

/// CREATE MATERIALIZED VIEW: a table holding the query's rows (none WITH NO DATA).
pub(crate) fn create_matview(
    cv: &ir::CreateView,
    txn: &mut Txn,
    binder: &Binder,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    if let Some(msg) = already_exists(binder, &cv.name)? {
        return Err(SqlError::new(code::DUPLICATE_TABLE, msg));
    }
    let reloptions = crate::reloptions::create(crate::reloptions::Target::Heap, &cv.reloptions)?;
    let n = create_from_query(&cv.name, &cv.columns, &cv.query, cv.with_data, true, false, reloptions, txn, binder, env, state)?;
    Ok(tag(&if cv.with_data { format!("SELECT {n}") } else { "CREATE MATERIALIZED VIEW".to_owned() }))
}

/// CREATE TABLE ... AS query [WITH [NO] DATA] (and SELECT INTO).
pub(crate) fn create_table_as(
    ct: &ir::CreateTable,
    txn: &mut Txn,
    binder: &Binder,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let query = ct.as_query.as_deref().ok_or_else(|| SqlError::internal("CREATE TABLE AS without a query"))?;
    if let Some(msg) = already_exists(binder, &ct.name)? {
        if ct.if_not_exists {
            return Ok(QueryResult {
                tag: "CREATE TABLE AS".into(),
                notices: vec![crate::session::Notice { severity: "NOTICE", code: "00000", message: format!("{msg}, skipping") }],
                ..Default::default()
            });
        }
        return Err(SqlError::new(code::DUPLICATE_TABLE, msg));
    }
    let with_data = ct.options.with_data;
    let reloptions = crate::reloptions::table(&ct.options.reloptions, false)?;
    let n = create_from_query(
        &ct.name,
        &ct.options.column_names,
        query,
        with_data,
        false,
        ct.options.unlogged,
        reloptions,
        txn,
        binder,
        env,
        state,
    )?;
    Ok(tag(&if with_data { format!("SELECT {n}") } else { "CREATE TABLE AS".to_owned() }))
}

/// REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA].
pub(crate) fn refresh_matview(
    name: &ir::ObjectName,
    concurrently: bool,
    with_data: bool,
    txn: &mut Txn,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let def = match txn.catalog().find_relation(name)? {
        Some(Relation::Table(t)) if t.matview.is_some() => t,
        Some(_) => return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a materialized view", name.base()))),
        None => return Err(SqlError::undefined_table(&name.to_string())),
    };
    let mv = def.matview.clone().ok_or_else(|| SqlError::internal("not a materialized view"))?;
    if concurrently {
        if !mv.populated {
            return Err(SqlError::feature_not_supported("CONCURRENTLY cannot be used when the materialized view is not populated"));
        }
        if !def.indexes.iter().any(|i| i.unique && i.predicate.is_none()) {
            return Err(SqlError::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("cannot refresh materialized view \"{}.{}\" concurrently", def.schema, def.name),
            )
            .with_hint("Create a unique index with no WHERE clause on one or more columns of the materialized view."));
        }
    }
    let rows = if with_data {
        let catalog = txn.catalog().clone();
        let binder = Binder::new(&catalog, &[]);
        let top = Level::new(Scope::default());
        let bound = pgmem_types::context::with_search_path(&mv.search_path, || binder.bind_query(&mv.query, &Ctx::new(&top)))?;
        run(&bound.rel, &EvalCtx::new(txn, &[], env, state))?
    } else {
        Vec::new()
    };
    txn.truncate(def.oid);
    for row in rows {
        txn.insert(def.oid, row);
    }
    let mut new_def = (*def).clone();
    new_def.matview = Some(Box::new(MatView { populated: with_data, ..*mv }));
    txn.catalog_mut().replace_table(&def.qualified(), new_def);
    Ok(tag("REFRESH MATERIALIZED VIEW"))
}

/// ALTER [MATERIALIZED] VIEW name RENAME TO new_name.
pub(crate) fn rename_view(
    name: &ir::ObjectName,
    new_name: &str,
    materialized: bool,
    if_exists: bool,
    txn: &mut Txn,
) -> SqlResult<QueryResult> {
    let what = if materialized { "ALTER MATERIALIZED VIEW" } else { "ALTER VIEW" };
    let rel = txn.catalog().find_relation(name)?;
    match rel {
        None if if_exists => Ok(QueryResult {
            tag: what.into(),
            notices: vec![crate::session::Notice {
                severity: "NOTICE",
                code: "00000",
                message: format!("relation \"{}\" does not exist, skipping", name.base()),
            }],
            ..Default::default()
        }),
        None => Err(SqlError::undefined_table(&name.to_string())),
        Some(Relation::View(v)) if !materialized => {
            if txn.catalog().relation(&v.schema, new_name).is_some() {
                return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{new_name}\" already exists")));
            }
            let mut def = (*v).clone();
            def.name = new_name.to_owned();
            let triggers: Vec<_> = txn.catalog().triggers().filter(|t| t.table == v.oid).cloned().collect();
            let cat = txn.catalog_mut();
            cat.drop_view(&QualName::new(&v.schema, &v.name));
            cat.create_view(def, false)?;
            for t in triggers {
                cat.put_trigger((*t).clone());
            }
            Ok(tag(what))
        }
        Some(Relation::Table(t)) if materialized && t.matview.is_some() => {
            if txn.catalog().relation(&t.schema, new_name).is_some() {
                return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{new_name}\" already exists")));
            }
            let mut def = (*t).clone();
            def.name = new_name.to_owned();
            txn.catalog_mut().replace_table(&t.qualified(), def);
            Ok(tag(what))
        }
        Some(_) => Err(SqlError::new(
            code::WRONG_OBJECT_TYPE,
            format!("\"{}\" is not a {}", name.base(), if materialized { "materialized view" } else { "view" }),
        )),
    }
}
