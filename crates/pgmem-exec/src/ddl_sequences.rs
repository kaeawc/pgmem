//! ALTER SEQUENCE and the identity column actions of ALTER TABLE (ADD / SET / DROP IDENTITY,
//! RESTART), which change the column's sequence.

use crate::session::{Notice, QueryResult};
use pgmem_catalog::{Identity, QualName, Relation, SequenceDef, TableDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};
use std::sync::Arc;

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

/// ALTER SEQUENCE [IF EXISTS] name {options | RENAME TO new_name}.
pub(crate) fn alter_sequence(
    name: &ir::ObjectName,
    if_exists: bool,
    options: &ir::SequenceOptions,
    rename_to: Option<&str>,
    txn: &mut Txn,
) -> SqlResult<QueryResult> {
    let seq = match txn.catalog().find_relation(name)? {
        Some(Relation::Sequence(s)) => s,
        Some(r) => return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a sequence", r.name()))),
        None if if_exists => {
            return Ok(QueryResult {
                tag: "ALTER SEQUENCE".into(),
                notices: vec![Notice {
                    severity: "NOTICE",
                    code: "00000",
                    message: format!("relation \"{}\" does not exist, skipping", name.base()),
                }],
                ..Default::default()
            });
        }
        None => return Err(SqlError::undefined_table(&name.to_string())),
    };
    let old = QualName::new(&seq.schema, &seq.name);
    let mut def = (*seq).clone();
    if let Some(new) = rename_to {
        if txn.catalog().relation(&seq.schema, new).is_some() {
            return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{new}\" already exists")));
        }
        new.clone_into(&mut def.name);
        // Defaults name the sequence: follow the rename (Postgres stores its OID).
        for (table, slot) in crate::deps::defaults_using_sequence(txn.catalog(), seq.oid) {
            if let Some(t) = txn.catalog().table_q(&table).cloned() {
                let mut t = (*t).clone();
                t.columns[slot].default = Some(crate::bind::ddl::nextval_default(&def.schema, new));
                txn.catalog_mut().replace_table(&table, t);
            }
        }
        txn.catalog_mut().replace_sequence(&old, def);
        return Ok(tag("ALTER SEQUENCE"));
    }
    let restart = crate::sequences::apply_options(txn.catalog(), &mut def, options, false)?;
    match &options.owned_by {
        Some(Some(owner)) => def.owned_by = Some(crate::sequences::resolve_owner(txn.catalog(), &def.schema, owner)?),
        Some(None) => def.owned_by = None,
        None => {}
    }
    if let Some(at) = restart {
        txn.restart_sequence(&def, Some(at));
    }
    txn.catalog_mut().replace_sequence(&old, def);
    Ok(tag("ALTER SEQUENCE"))
}

/// The sequence behind identity column `column` of `table`.
fn identity_sequence_of(txn: &Txn, table: &TableDef, column: &str) -> Option<Arc<SequenceDef>> {
    let owner = Some((table.name.clone(), column.to_owned()));
    txn.catalog().sequences().find(|s| s.schema == table.schema && s.owned_by == owner).cloned()
}

/// Runs an identity action of ALTER TABLE; false for any other action.
pub(crate) fn alter_identity(txn: &mut Txn, table: &TableDef, action: &ir::AlterTableAction) -> SqlResult<bool> {
    let tname = &table.name;
    let column = match action {
        ir::AlterTableAction::AddIdentity { column, .. }
        | ir::AlterTableAction::SetIdentity { column, .. }
        | ir::AlterTableAction::DropIdentity { column, .. } => column,
        _ => return Ok(false),
    };
    let slot = table.column_index(column).ok_or_else(|| SqlError::undefined_column(&format!("\"{column}\" of relation \"{tname}\"")))?;
    let col = &table.columns[slot];
    let not_identity = || {
        SqlError::new(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("column \"{column}\" of relation \"{tname}\" is not an identity column"),
        )
    };
    let mut def = table.clone();
    match action {
        ir::AlterTableAction::AddIdentity { always, options, .. } => {
            if !col.not_null {
                return Err(SqlError::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("column \"{column}\" of relation \"{tname}\" must be declared NOT NULL before identity can be added"),
                ));
            }
            if col.identity.is_some() {
                return Err(SqlError::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("column \"{column}\" of relation \"{tname}\" is already an identity column"),
                ));
            }
            if col.default.is_some() {
                return Err(SqlError::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("column \"{column}\" of relation \"{tname}\" already has a default value"),
                ));
            }
            let mut taken = Vec::new();
            let mut seq = crate::bind::ddl::identity_sequence(txn.catalog(), &table.schema, tname, column, &col.ty, options, &mut taken)?;
            seq.oid = txn.catalog_mut().alloc_oid();
            def.columns[slot].identity = Some(if *always { Identity::Always } else { Identity::ByDefault });
            def.columns[slot].default = Some(crate::bind::ddl::nextval_default(&table.schema, &seq.name));
            txn.catalog_mut().create_sequence(seq)?;
        }
        ir::AlterTableAction::SetIdentity { always, options, .. } => {
            if col.identity.is_none() {
                return Err(not_identity());
            }
            if let Some(a) = always {
                def.columns[slot].identity = Some(if *a { Identity::Always } else { Identity::ByDefault });
            }
            if let Some(seq) = identity_sequence_of(txn, table, column) {
                let mut new = (*seq).clone();
                let restart = crate::sequences::apply_options(txn.catalog(), &mut new, options, false)?;
                if let Some(at) = restart {
                    txn.restart_sequence(&new, Some(at));
                }
                txn.catalog_mut().replace_sequence(&QualName::new(&seq.schema, &seq.name), new);
            }
        }
        ir::AlterTableAction::DropIdentity { if_exists, .. } => {
            if col.identity.is_none() {
                return if *if_exists { Ok(true) } else { Err(not_identity()) };
            }
            def.columns[slot].identity = None;
            def.columns[slot].default = None;
            if let Some(seq) = identity_sequence_of(txn, table, column) {
                txn.catalog_mut().drop_sequence(&QualName::new(&seq.schema, &seq.name));
            }
        }
        _ => return Ok(false),
    }
    txn.catalog_mut().replace_table(&table.qualified(), def);
    Ok(true)
}
