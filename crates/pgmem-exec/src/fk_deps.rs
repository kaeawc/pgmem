//! Foreign keys depend on the unique index and the key columns they reference: dropping either
//! (DROP CONSTRAINT, DROP INDEX, DROP COLUMN) is refused, or with CASCADE drops the foreign
//! keys too.

use crate::ddl::{cascade_notice, dependents_error, display_name, drop_fk};
use crate::session::Notice;
use pgmem_catalog::{Catalog, ConstraintKind, Oid, QualName, TableDef};
use pgmem_storage::Txn;
use pgmem_types::SqlResult;

/// A foreign key: its table and constraint name.
pub(crate) type Fk = (QualName, String);

/// Foreign keys referencing `table` for which `uses(owner, columns, ref_columns)` holds, in
/// creation order.
fn referencing(cat: &Catalog, table: &TableDef, uses: impl Fn(&TableDef, &[usize], &[usize]) -> bool) -> Vec<Fk> {
    let target = table.qualified();
    let mut out: Vec<(Oid, Fk)> = cat
        .tables()
        .flat_map(|o| {
            o.constraints.iter().filter_map(|c| match &c.kind {
                ConstraintKind::ForeignKey { columns, ref_table, ref_columns, .. }
                    if *ref_table == target && uses(o, columns, ref_columns) =>
                {
                    Some((c.oid, (o.qualified(), c.name.clone())))
                }
                _ => None,
            })
        })
        .collect();
    out.sort_by_key(|(oid, _)| *oid);
    out.into_iter().map(|(_, fk)| fk).collect()
}

/// Foreign keys using index `index` of `table`.
pub(crate) fn on_index(cat: &Catalog, table: &TableDef, index: &str) -> Vec<Fk> {
    referencing(cat, table, |_, _, refs| table.fk_index(refs).is_some_and(|i| i.name == index))
}

/// Foreign keys referencing column `slot` of `table`. A self-referencing key whose own
/// columns include it goes with the column anyway.
pub(crate) fn on_column(cat: &Catalog, table: &TableDef, slot: usize) -> Vec<Fk> {
    referencing(cat, table, |owner, cols, refs| refs.contains(&slot) && !(owner.oid == table.oid && cols.contains(&slot)))
}

/// Refuses to drop `what` (`"constraint"`, `"index"`, `"column"`) named `name` while `fks`
/// depend on it (as `on`, e.g. `index t_pkey`), or with CASCADE drops them.
pub(crate) fn drop_dependents(
    txn: &mut Txn,
    fks: &[Fk],
    (what, name, on): (&str, &str, &str),
    cascade: bool,
    notices: &mut Vec<Notice>,
) -> SqlResult<()> {
    if fks.is_empty() {
        return Ok(());
    }
    let members: Vec<String> = fks.iter().map(|(t, con)| format!("constraint {con} on table {}", display_name(txn.catalog(), t))).collect();
    if !cascade {
        let lines: Vec<String> = members.iter().map(|m| format!("{m} depends on {on}")).collect();
        return Err(dependents_error(what, name, lines.join("\n")));
    }
    for (t, con) in fks {
        drop_fk(txn, t, con);
    }
    notices.push(cascade_notice(&members));
    Ok(())
}

/// How a dependency on index `index` of `table` is reported. On a partitioned table the
/// foreign key also has a clone per partition, on that partition's index; Postgres reports
/// the one on the newest index of the tree.
pub(crate) fn reported_index(cat: &Catalog, table: &TableDef, index: &str) -> String {
    let mut shown = QualName::new(&table.schema, index);
    if let Some(idx) = table.indexes.iter().find(|i| i.name == index) {
        let mut newest = idx.oid;
        let mut stack = cat.index_children(idx.oid);
        while let Some(oid) = stack.pop() {
            if let Some((t, i)) = cat.index_by_oid(oid) {
                if oid > newest {
                    newest = oid;
                    shown = QualName::new(&t.schema, &i.name);
                }
                stack.extend(cat.index_children(oid));
            }
        }
    }
    display_name(cat, &shown)
}

/// ALTER TABLE ... DROP CONSTRAINT of a primary key or unique constraint foreign keys use.
pub(crate) fn drop_key_constraint(txn: &mut Txn, table: &TableDef, name: &str, cascade: bool, notices: &mut Vec<Notice>) -> SqlResult<()> {
    let is_key = table
        .constraints
        .iter()
        .any(|c| c.name == name && matches!(c.kind, ConstraintKind::PrimaryKey { .. } | ConstraintKind::Unique { .. }));
    if !is_key {
        return Ok(());
    }
    let cat = txn.catalog();
    let fks = on_index(cat, table, name);
    let shown = display_name(cat, &table.qualified());
    let index = reported_index(cat, table, name);
    drop_dependents(txn, &fks, ("constraint", &format!("{name} on table {shown}"), &format!("index {index}")), cascade, notices)
}

/// ALTER TABLE ... DROP COLUMN of a column other tables' foreign keys reference.
pub(crate) fn drop_column(txn: &mut Txn, table: &TableDef, column: &str, cascade: bool, notices: &mut Vec<Notice>) -> SqlResult<()> {
    let Some(slot) = table.column_index(column) else { return Ok(()) };
    let cat = txn.catalog();
    let fks = on_column(cat, table, slot);
    let shown = display_name(cat, &table.qualified());
    let name = format!("{column} of table {shown}");
    drop_dependents(txn, &fks, ("column", &name, &format!("column {name}")), cascade, notices)
}
