//! `REINDEX`. pgmem keeps no index structures, so there is nothing to rebuild except what a
//! failed `CREATE INDEX CONCURRENTLY` left behind: an invalid index (`indisvalid` and
//! `indisready` false). Rebuilding computes its keys over the table again and, once that
//! succeeds, makes it valid and ready. A valid index is left alone.

use crate::eval::StatementState;
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{IndexDef, QualName, Relation, TableDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};
use std::sync::Arc;

fn notice(severity: &'static str, code: &'static str, message: String) -> Notice {
    Notice { severity, code, message }
}

pub(crate) fn reindex(r: &ir::Reindex, txn: &mut Txn, env: &SessionEnv, state: &StatementState) -> SqlResult<QueryResult> {
    let mut notices = Vec::new();
    let named = |what: &str| r.name.clone().ok_or_else(|| SqlError::syntax(format!("syntax error at or near \"{what}\"")));
    match r.kind {
        ir::ReindexKind::Index => {
            let name = named("INDEX")?;
            let (table, index) = match txn.catalog().find_relation(&name)? {
                Some(Relation::Index { table, index }) => (table, *index),
                Some(other) => return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not an index", other.name()))),
                None => return Err(SqlError::undefined_table(&name.to_string())),
            };
            rebuild(txn, &table, &index, r.concurrently, env, state)?;
        }
        ir::ReindexKind::Table => {
            let name = named("TABLE")?;
            let table = match txn.catalog().find_relation(&name)? {
                Some(Relation::Table(t)) => t,
                Some(other) => {
                    return Err(SqlError::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("\"{}\" is not a table or materialized view", other.name()),
                    ));
                }
                None => return Err(SqlError::undefined_table(&name.to_string())),
            };
            let mut tables = vec![Arc::clone(&table)];
            tables.extend(txn.catalog().partition_descendants(table.oid));
            let reindexed = reindex_tables(txn, &tables, r.concurrently, &mut notices, env, state)?;
            if reindexed == 0 {
                let what = if r.concurrently { "has no indexes that can be reindexed concurrently" } else { "has no indexes to reindex" };
                notices.push(notice("NOTICE", "00000", format!("table \"{}\" {what}", table.name)));
            }
        }
        ir::ReindexKind::Schema => {
            let name = named("SCHEMA")?;
            let schema = name.base().to_owned();
            if txn.catalog().schema(&schema).is_none() {
                return Err(pgmem_catalog::undefined_schema(&schema));
            }
            let tables: Vec<Arc<TableDef>> = txn.catalog().tables().filter(|t| t.schema == schema).cloned().collect();
            reindex_tables(txn, &tables, r.concurrently, &mut notices, env, state)?;
        }
        ir::ReindexKind::Database => {
            let tables: Vec<Arc<TableDef>> = txn.catalog().tables().cloned().collect();
            reindex_tables(txn, &tables, r.concurrently, &mut notices, env, state)?;
        }
        ir::ReindexKind::System => {}
    }
    Ok(QueryResult { tag: "REINDEX".into(), notices, ..Default::default() })
}

/// Reindexes the indexes of `tables`; returns how many were considered. Concurrently, an
/// invalid index is skipped with a warning (it needs `REINDEX INDEX` or `DROP INDEX`).
fn reindex_tables(
    txn: &mut Txn,
    tables: &[Arc<TableDef>],
    concurrently: bool,
    notices: &mut Vec<Notice>,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<usize> {
    let mut considered = 0;
    for t in tables {
        for idx in &t.indexes {
            if concurrently && !idx.valid {
                let message = format!("skipping reindex of invalid index \"{}.{}\"", t.schema, idx.name);
                notices.push(notice("WARNING", code::OBJECT_NOT_IN_PREREQUISITE_STATE, message));
                continue;
            }
            considered += 1;
            rebuild(txn, &t.qualified(), idx, concurrently, env, state)?;
        }
    }
    Ok(considered)
}

/// Rebuilds one index. A failed concurrent rebuild leaves its `_ccnew` copy behind, invalid,
/// as Postgres does; a failed plain one leaves the index as it was.
fn rebuild(
    txn: &mut Txn,
    table: &QualName,
    index: &IndexDef,
    concurrently: bool,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<()> {
    if index.ready {
        return Ok(());
    }
    let def = txn.catalog().table_q(table).cloned().ok_or_else(|| SqlError::undefined_table(&table.name))?;
    let mut probe = (*def).clone();
    let mut copy = None;
    if concurrently {
        let name = txn.catalog().choose_relation_name(&def.schema, &index.name, &[], "ccnew", &[]);
        let oid = txn.catalog_mut().alloc_oid();
        copy = Some(IndexDef { oid, name, valid: false, ready: false, clustered: false, replica: false, primary: false, ..index.clone() });
    }
    let built = copy.as_ref().unwrap_or(index);
    probe.indexes.retain(|i| i.oid != index.oid);
    probe.indexes.push(built.clone());
    if let Err(e) = crate::ddl::validate_index_build(txn, &probe, &built.name, env, state) {
        if let Some(copy) = copy {
            let mut def = (*def).clone();
            def.indexes.push(copy);
            txn.catalog_mut().replace_table(table, def);
            state.keep_on_error.set(true);
        }
        return Err(e);
    }
    txn.catalog_mut().set_index_valid(index.oid, true);
    Ok(())
}
