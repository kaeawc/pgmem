//! ALTER TABLE attribute actions that only record catalog state (no physical effect):
//! `SET STATISTICS | STORAGE | COMPRESSION`, `REPLICA IDENTITY`, `CLUSTER ON`,
//! `SET WITHOUT CLUSTER` and `SET [UN]LOGGED`.

use crate::catalog_views::type_storage;
use crate::eval::StatementState;
use crate::session::Notice;
use pgmem_catalog::{ConstraintKind, IndexDef, QualName, Relation, TableDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};

const SYSTEM_COLUMNS: [&str; 6] = ["tableoid", "cmax", "xmax", "cmin", "xmin", "ctid"];

/// Applies `action` if it is one of the attribute actions; Ok(false) for any other.
pub(crate) fn apply(
    txn: &mut Txn,
    current: &QualName,
    table: &TableDef,
    action: &ir::AlterTableAction,
    state: &StatementState,
) -> SqlResult<bool> {
    use ir::AlterTableAction as A;
    let mut def = table.clone();
    // The action the partitions get too, when it recurses.
    let mut child_action = None;
    match action {
        A::SetStatistics { column, target } => {
            let i = column_index(&def, column)?;
            let target = match *target {
                t if t < -1 => {
                    return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("statistics target {t} is too low")));
                }
                t if t > 10000 => {
                    state.notices.borrow_mut().push(Notice {
                        severity: "WARNING",
                        code: "01000",
                        message: "lowering statistics target to 10000".to_owned(),
                    });
                    10000
                }
                t => t,
            };
            def.columns[i].stattarget = if target == -1 { None } else { i16::try_from(target).ok() };
            child_action = Some(A::SetStatistics { column: column.clone(), target });
        }
        A::SetStorage { column, storage } => {
            let i = column_index(&def, column)?;
            let (len, _, _, default, _) = type_storage(&def.columns[i].ty);
            let chosen = match storage.to_ascii_lowercase().as_str() {
                "plain" => Some('p'),
                "external" => Some('e'),
                "extended" => Some('x'),
                "main" => Some('m'),
                "default" => None,
                _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid storage type \"{storage}\""))),
            };
            if len != -1 && chosen.is_some_and(|c| c != 'p') {
                return Err(SqlError::feature_not_supported(format!(
                    "column data type {} can only have storage PLAIN",
                    crate::deparse::pg::format_type(&def.columns[i].ty, def.columns[i].typmod)
                )));
            }
            def.columns[i].storage = chosen.filter(|c| *c != default);
            child_action = Some(action.clone());
        }
        A::SetCompression { column, method } => {
            let i = column_index(&def, column)?;
            let (_, _, _, default, _) = type_storage(&def.columns[i].ty);
            if !matches!(default, 'x' | 'm') {
                return Err(SqlError::feature_not_supported(format!(
                    "column data type {} does not support compression",
                    crate::deparse::pg::format_type(&def.columns[i].ty, def.columns[i].typmod)
                )));
            }
            def.columns[i].compression = match method.to_ascii_lowercase().as_str() {
                "default" => None,
                "pglz" => Some('p'),
                "lz4" => Some('l'),
                _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid compression method \"{method}\""))),
            };
            child_action = Some(action.clone());
        }
        A::ReplicaIdentity { kind, index } => {
            let chosen = match index {
                Some(name) => Some(find_index(txn, &def, name)?),
                None => None,
            };
            if let Some(pos) = chosen {
                let idx = &def.indexes[pos];
                let deferrable = def.constraints.iter().any(|c| c.name == idx.name && c.deferrable);
                if !idx.unique {
                    return Err(SqlError::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("cannot use non-unique index \"{}\" as replica identity", idx.name),
                    ));
                }
                if deferrable {
                    return Err(SqlError::feature_not_supported(format!(
                        "cannot use non-immediate index \"{}\" as replica identity",
                        idx.name
                    )));
                }
                if !idx.exprs.is_empty() {
                    return Err(SqlError::feature_not_supported(format!(
                        "cannot use expression index \"{}\" as replica identity",
                        idx.name
                    )));
                }
                if idx.predicate.is_some() {
                    return Err(SqlError::feature_not_supported(format!("cannot use partial index \"{}\" as replica identity", idx.name)));
                }
                if let Some(&c) = idx.columns.iter().find(|&&c| !def.columns[c].not_null) {
                    return Err(SqlError::new(
                        code::WRONG_OBJECT_TYPE,
                        format!(
                            "index \"{}\" cannot be used as replica identity because column \"{}\" is nullable",
                            idx.name, def.columns[c].name
                        ),
                    ));
                }
            }
            for (i, idx) in def.indexes.iter_mut().enumerate() {
                idx.replica = chosen == Some(i);
            }
            def.replident = *kind;
        }
        A::ClusterOn(index) => {
            if txn.catalog().partition_key(def.oid).is_some() {
                return Err(SqlError::feature_not_supported("cannot mark index clustered in partitioned table"));
            }
            let chosen = match index {
                Some(name) => {
                    let pos = find_index(txn, &def, name)?;
                    if def.indexes[pos].predicate.is_some() {
                        return Err(SqlError::feature_not_supported(format!("cannot cluster on partial index \"{name}\"")));
                    }
                    Some(pos)
                }
                None => None,
            };
            for (i, idx) in def.indexes.iter_mut().enumerate() {
                idx.clustered = chosen == Some(i);
            }
        }
        A::SetUnlogged(unlogged) => {
            let cat = txn.catalog();
            let kind = if def.matview.is_some() {
                Some("materialized views")
            } else if cat.partition_key(def.oid).is_some() {
                Some("partitioned tables")
            } else {
                None
            };
            if let Some(kind) = kind {
                return Err(SqlError::new(
                    code::WRONG_OBJECT_TYPE,
                    format!(
                        "ALTER action SET {} cannot be performed on relation \"{}\"",
                        if *unlogged { "UNLOGGED" } else { "LOGGED" },
                        def.name
                    ),
                )
                .with_detail(format!("This operation is not supported for {kind}.")));
            }
            if def.schema.starts_with("pg_temp_") {
                return Err(SqlError::new(
                    code::INVALID_TABLE_DEFINITION,
                    format!("cannot change logged status of table \"{}\" because it is temporary", def.name),
                ));
            }
            if def.unlogged != *unlogged {
                // A permanent table may only reference permanent tables: an unlogged table
                // cannot be referenced by a logged one, and a logged one cannot reference it.
                let (word, other) = if *unlogged { ("unlogged", "logged") } else { ("logged", "unlogged") };
                for t in cat.tables() {
                    for c in &t.constraints {
                        let ConstraintKind::ForeignKey { ref_table, .. } = &c.kind else { continue };
                        let referenced = cat.table_q(ref_table);
                        let (referencing, target) = (&**t, referenced.map(|r| &**r));
                        let peer = if *unlogged && target.is_some_and(|r| r.oid == def.oid) {
                            Some(referencing)
                        } else if !*unlogged && referencing.oid == def.oid {
                            target
                        } else {
                            None
                        };
                        if let Some(p) = peer
                            && p.oid != def.oid
                            && p.unlogged != *unlogged
                            && !p.schema.starts_with("pg_temp_")
                        {
                            return Err(SqlError::new(
                                code::INVALID_TABLE_DEFINITION,
                                format!(
                                    "could not change table \"{}\" to {word} because it references {other} table \"{}\"",
                                    def.name, p.name
                                ),
                            ));
                        }
                    }
                }
            }
            def.unlogged = *unlogged;
        }
        _ => return Ok(false),
    }
    let oid = def.oid;
    txn.catalog_mut().replace_table(current, def);
    if let Some(child) = child_action {
        let partitions: Vec<QualName> = txn.catalog().partitions(oid).into_iter().map(|(t, _)| t.qualified()).collect();
        for p in partitions {
            let t = txn.catalog().table_q(&p).cloned().ok_or_else(|| SqlError::undefined_table(&p.name))?;
            apply(txn, &p, &t, &child, state)?;
        }
    }
    Ok(true)
}

fn column_index(def: &TableDef, name: &str) -> SqlResult<usize> {
    def.column_index(name).ok_or_else(|| {
        if SYSTEM_COLUMNS.contains(&name) {
            SqlError::feature_not_supported(format!("cannot alter system column \"{name}\""))
        } else {
            SqlError::undefined_column(&format!("\"{name}\" of relation \"{}\"", def.name))
        }
    })
}

/// The position of index `name` among the table's indexes, with Postgres' errors for a name
/// that is no index of this table.
fn find_index(txn: &Txn, def: &TableDef, name: &str) -> SqlResult<usize> {
    if let Some(pos) = def.indexes.iter().position(|i: &IndexDef| i.name == name) {
        return Ok(pos);
    }
    match txn.catalog().relation(&def.schema, name) {
        Some(Relation::Index { .. }) => {
            Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{name}\" is not an index for table \"{}\"", def.name)))
        }
        Some(_) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{name}\" is not an index"))),
        None => Err(SqlError::new(code::UNDEFINED_OBJECT, format!("index \"{name}\" for table \"{}\" does not exist", def.name))),
    }
}
