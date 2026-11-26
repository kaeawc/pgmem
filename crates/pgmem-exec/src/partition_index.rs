//! Partitioned indexes (`indexcmds.c` `DefineIndex`, `tablecmds.c` `ATExecAttachPartitionIdx`):
//! an index on a partitioned table has one attached index per partition (`pg_inherits` rows,
//! `relispartition` on the child). `CREATE INDEX ... ON ONLY` makes the parent alone, invalid
//! until `ALTER INDEX ... ATTACH PARTITION` has attached an index for every partition.

use crate::bind::{self, Binder};
use crate::eval::StatementState;
use crate::session::{QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, ConstraintKind, IndexDef, Oid, QualName, Relation, TableDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};

/// The partition slot holding parent column `slot` (matched by name).
fn slot_by_name(parent: &TableDef, part: &TableDef, slot: usize) -> Option<usize> {
    let name = &parent.columns.get(slot)?.name;
    part.live_columns().find(|(_, c)| c.name == *name).map(|(i, _)| i)
}

/// `CompareIndexInfo`: the same keys (columns mapped by `slot_of`), expressions, predicate,
/// uniqueness and NULLS [NOT] DISTINCT.
pub(crate) fn same_definition(parent: &IndexDef, child: &IndexDef, slot_of: &dyn Fn(usize) -> Option<usize>) -> bool {
    let columns: Option<Vec<usize>> = parent.columns.iter().map(|c| slot_of(*c)).collect();
    columns.is_some_and(|c| c == child.columns)
        && parent.exprs == child.exprs
        && parent.predicate == child.predicate
        && parent.unique == child.unique
        && parent.nulls_not_distinct == child.nulls_not_distinct
}

/// Whether the index implements a PRIMARY KEY / UNIQUE constraint of its table.
pub(crate) fn is_constraint_index(table: &TableDef, idx: &IndexDef) -> bool {
    table
        .constraints
        .iter()
        .any(|c| c.name == idx.name && matches!(c.kind, ConstraintKind::PrimaryKey { .. } | ConstraintKind::Unique { .. }))
}

/// The index of partition `part` attached to `parent_index`, if any.
fn attached_on(catalog: &Catalog, parent_index: Oid, part: &TableDef) -> Option<Oid> {
    catalog.index_children(parent_index).into_iter().find(|c| part.indexes.iter().any(|i| i.oid == *c))
}

/// An index of `part` that `DefineIndex` / `AttachPartitionEnsureIndexes` would attach to
/// `pidx` of `parent`: not attached yet, the same definition, a constraint index when the
/// parent's is one.
fn adoptable(catalog: &Catalog, parent: &TableDef, pidx: &IndexDef, part: &TableDef) -> Option<Oid> {
    let needs_constraint = is_constraint_index(parent, pidx);
    part.indexes
        .iter()
        .find(|i| {
            catalog.index_parent(i.oid).is_none()
                && same_definition(pidx, i, &|s| slot_by_name(parent, part, s))
                && (!needs_constraint || is_constraint_index(part, i))
        })
        .map(|i| i.oid)
}

/// `validatePartitionedIndex`: marks a partitioned index valid once every partition has a valid
/// attached index, then its parent the same way.
fn validate(catalog: &mut Catalog, index: Oid) {
    if catalog.index_valid(index) {
        return;
    }
    let Some((table, _)) = catalog.index_by_oid(index) else { return };
    let parts = catalog.partitions(table.oid).len();
    let valid = catalog.index_children(index).into_iter().filter(|c| catalog.index_valid(*c)).count();
    if valid == parts {
        catalog.set_index_valid(index, true);
        if let Some(parent) = catalog.index_parent(index) {
            validate(catalog, parent);
        }
    }
}

/// `CREATE INDEX`.
pub(crate) fn create_index(
    ci: &ir::CreateIndex,
    txn: &mut Txn,
    binder: &Binder,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let Some((tname, idx)) = bind::bind_create_index(binder, ci)? else {
        return Ok(crate::ddl::notice(
            "CREATE INDEX",
            format!("relation \"{}\" already exists, skipping", ci.name.clone().unwrap_or_default()),
        ));
    };
    define(ci, &tname, idx, txn, env, state)?;
    Ok(QueryResult { tag: "CREATE INDEX".into(), ..Default::default() })
}

/// Creates `idx` on `tname` and, unless ON ONLY, attaches or creates one per partition.
/// Returns the new index's OID and whether it is valid.
fn define(
    ci: &ir::CreateIndex,
    tname: &QualName,
    mut idx: IndexDef,
    txn: &mut Txn,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<(Oid, bool)> {
    let table = txn.catalog().table_q(tname).cloned().ok_or_else(|| SqlError::undefined_table(&tname.name))?;
    let mut def = (*table).clone();
    idx.oid = txn.catalog_mut().alloc_oid();
    let oid = idx.oid;
    let unique = idx.unique;
    let name = idx.name.clone();
    def.indexes.push(idx.clone());
    if unique {
        crate::ddl::validate_unique(txn, &def, &name, env, state)?;
    }
    let partitioned = txn.catalog().partition_key(def.oid).cloned();
    if let Some(key) = &partitioned {
        crate::partition::check_unique_keys(&def, key)?;
    }
    let partitions = txn.catalog().partitions(def.oid).into_iter().map(|(t, _)| t).collect::<Vec<_>>();
    txn.catalog_mut().replace_table(tname, def.clone());
    if partitioned.is_none() {
        return Ok((oid, true));
    }
    let mut valid = !ci.only || partitions.is_empty();
    if !ci.only {
        for p in partitions {
            let child_oid = match adoptable(txn.catalog(), &def, &idx, &p) {
                Some(c) => {
                    valid &= txn.catalog().index_valid(c);
                    c
                }
                None => {
                    let (c, child_valid) = define_child(ci, &p, txn, env, state)?;
                    valid &= child_valid;
                    c
                }
            };
            txn.catalog_mut().set_index_parent(child_oid, Some(oid));
        }
    }
    txn.catalog_mut().set_index_valid(oid, valid);
    Ok((oid, valid))
}

/// The partition's copy of the index being created (named by default, like Postgres).
fn define_child(ci: &ir::CreateIndex, part: &TableDef, txn: &mut Txn, env: &SessionEnv, state: &StatementState) -> SqlResult<(Oid, bool)> {
    let mut child = ci.clone();
    child.name = None;
    child.if_not_exists = false;
    child.table = ir::ObjectName(vec![part.schema.clone(), part.name.clone()]);
    let snapshot = txn.catalog().clone();
    let b = Binder::new(&snapshot, &[]);
    let (q, cidx) = bind::bind_create_index(&b, &child)?.ok_or_else(|| SqlError::internal("partition index exists"))?;
    define(&child, &q, cidx, txn, env, state)
}

fn attach_error(state: &'static str, child: &str, parent: &str, detail: String) -> SqlError {
    SqlError::new(state, format!("cannot attach index \"{child}\" as a partition of index \"{parent}\"")).with_detail(detail)
}

/// The index `name` names, with its table.
fn lookup_index(txn: &Txn, name: &ir::ObjectName, not_index: &'static str) -> SqlResult<(std::sync::Arc<TableDef>, IndexDef)> {
    match txn.catalog().find_relation(name)? {
        Some(Relation::Index { table, index }) => {
            let t = txn.catalog().table_q(&table).cloned().ok_or_else(|| SqlError::undefined_table(&table.name))?;
            Ok((t, *index))
        }
        Some(r) => Err(SqlError::new(not_index, format!("\"{}\" is not an index", r.name()))),
        None => Err(SqlError::undefined_table(&name.to_string())),
    }
}

/// `ALTER INDEX parent ATTACH PARTITION child`.
pub(crate) fn attach_index(index: &ir::ObjectName, partition: &ir::ObjectName, txn: &mut Txn) -> SqlResult<QueryResult> {
    let (ptable, pidx) = lookup_index(txn, index, code::WRONG_OBJECT_TYPE)?;
    if !txn.catalog().is_partitioned(ptable.oid) {
        return Err(SqlError::new(
            code::WRONG_OBJECT_TYPE,
            format!("ALTER action ATTACH PARTITION cannot be performed on relation \"{}\"", pidx.name),
        )
        .with_detail("This operation is not supported for indexes."));
    }
    let (ctable, cidx) = lookup_index(txn, partition, code::INVALID_OBJECT_DEFINITION)?;
    let catalog = txn.catalog();
    match catalog.index_parent(cidx.oid) {
        Some(p) if p == pidx.oid => return Ok(QueryResult { tag: "ALTER INDEX".into(), ..Default::default() }),
        Some(_) => {
            return Err(attach_error(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                &cidx.name,
                &pidx.name,
                format!("Index \"{}\" is already attached to another index.", cidx.name),
            ));
        }
        None => {}
    }
    if catalog.partition_of(ctable.oid).is_none_or(|of| of.parent != ptable.oid) {
        return Err(attach_error(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            &cidx.name,
            &pidx.name,
            format!("Index \"{}\" is not an index on any partition of table \"{}\".", cidx.name, ptable.name),
        ));
    }
    if attached_on(catalog, pidx.oid, &ctable).is_some() {
        return Err(attach_error(
            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
            &cidx.name,
            &pidx.name,
            format!("Another index is already attached for partition \"{}\".", ctable.name),
        ));
    }
    if !same_definition(&pidx, &cidx, &|s| slot_by_name(&ptable, &ctable, s)) {
        return Err(attach_error(code::INVALID_OBJECT_DEFINITION, &cidx.name, &pidx.name, "The index definitions do not match.".into()));
    }
    if is_constraint_index(&ptable, &pidx) && !is_constraint_index(&ctable, &cidx) {
        return Err(attach_error(
            code::INVALID_OBJECT_DEFINITION,
            &cidx.name,
            &pidx.name,
            format!(
                "The index \"{}\" belongs to a constraint in table \"{}\" but no constraint exists for index \"{}\".",
                pidx.name, ptable.name, cidx.name
            ),
        ));
    }
    let cat = txn.catalog_mut();
    cat.set_index_parent(cidx.oid, Some(pidx.oid));
    validate(cat, pidx.oid);
    Ok(QueryResult { tag: "ALTER INDEX".into(), ..Default::default() })
}

/// After a partition got copies of its parent's indexes (`links`: partition index name →
/// parent index OID), records the attachments.
pub(crate) fn link_by_name(catalog: &mut Catalog, part: Oid, links: &[(String, Oid)]) {
    let Some(t) = catalog.table_by_oid(part).cloned() else { return };
    for (name, parent) in links {
        if let Some(i) = t.indexes.iter().find(|i| i.name == *name) {
            catalog.set_index_parent(i.oid, Some(*parent));
        }
    }
}

/// The parent indexes a partition being created or attached needs an index for, each with the
/// partition's existing index to attach, if one qualifies (`AttachPartitionEnsureIndexes`).
pub(crate) fn plan_partition_indexes<'a>(catalog: &Catalog, parent: &'a TableDef, part: &TableDef) -> Vec<(&'a IndexDef, Option<Oid>)> {
    let mut taken: Vec<Oid> = Vec::new();
    let mut out = Vec::new();
    for pidx in &parent.indexes {
        let needs_constraint = is_constraint_index(parent, pidx);
        let found = part
            .indexes
            .iter()
            .find(|i| {
                !taken.contains(&i.oid)
                    && catalog.index_parent(i.oid).is_none()
                    && same_definition(pidx, i, &|s| slot_by_name(parent, part, s))
                    && (!needs_constraint || is_constraint_index(part, i))
            })
            .map(|i| i.oid);
        if let Some(o) = found {
            taken.push(o);
        }
        out.push((pidx, found));
    }
    out
}

/// DETACH PARTITION: the partition's indexes are no longer attached.
pub(crate) fn detach_indexes(catalog: &mut Catalog, part: &TableDef) {
    for i in &part.indexes {
        catalog.set_index_parent(i.oid, None);
    }
}

/// After ALTER TABLE ADD PRIMARY KEY / UNIQUE recursed into a partition: attaches the
/// partition's new key indexes (those not in `before`) to the parent's new ones.
pub(crate) fn link_new_indexes(catalog: &mut Catalog, parent: Oid, new_parent_indexes: &[Oid], part: Oid, before: &[Oid]) {
    let (Some(pt), Some(ct)) = (catalog.table_by_oid(parent).cloned(), catalog.table_by_oid(part).cloned()) else { return };
    let mut fresh: Vec<&IndexDef> = ct.indexes.iter().filter(|i| !before.contains(&i.oid)).collect();
    for pidx in pt.indexes.iter().filter(|i| new_parent_indexes.contains(&i.oid)) {
        if let Some(pos) = fresh.iter().position(|i| same_definition(pidx, i, &|s| slot_by_name(&pt, &ct, s))) {
            let child = fresh.remove(pos);
            catalog.set_index_parent(child.oid, Some(pidx.oid));
        }
    }
}

/// DROP INDEX: an attached index can't go on its own (`2BP01`).
pub(crate) fn check_drop(catalog: &Catalog, index: &IndexDef) -> SqlResult<()> {
    let Some(parent) = catalog.index_parent(index.oid) else { return Ok(()) };
    let Some((_, p)) = catalog.index_by_oid(parent) else { return Ok(()) };
    Err(SqlError::new(
        code::DEPENDENT_OBJECTS_STILL_EXIST,
        format!("cannot drop index {} because index {} requires it", index.name, p.name),
    )
    .with_hint(format!("You can drop index {} instead.", p.name)))
}

/// DROP INDEX of a partitioned index: its attached indexes go with it.
pub(crate) fn drop_children(catalog: &mut Catalog, index: Oid) {
    for child in catalog.index_children(index) {
        drop_children(catalog, child);
        if let Some((t, _)) = catalog.index_by_oid(child) {
            let mut def = (*t).clone();
            def.indexes.retain(|i| i.oid != child);
            catalog.replace_table(&t.qualified(), def);
        }
        catalog.forget_index(child);
    }
    catalog.forget_index(index);
}
