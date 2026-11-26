//! The relation locks each statement takes, after Postgres (`LockTableCommand`, the parser's
//! and planner's relation locks, `AlterTableGetLockLevel` and the DDL commands' own lock
//! levels). pgmem never waits for them; they are recorded so `pg_locks` lists them and so
//! `LOCK ... NOWAIT` in another session sees the conflict.

use pgmem_catalog::{Catalog, Oid, Relation, TableDef};
use pgmem_ir::{self as ir, AlterTableAction, Statement};
use pgmem_storage::locks::TableLockMode;
use pgmem_types::{SqlError, SqlResult, code};

pub const ACCESS_SHARE: TableLockMode = 1;
pub const ROW_SHARE: TableLockMode = 2;
pub const ROW_EXCLUSIVE: TableLockMode = 3;
pub const SHARE_UPDATE_EXCLUSIVE: TableLockMode = 4;
pub const SHARE: TableLockMode = 5;
pub const SHARE_ROW_EXCLUSIVE: TableLockMode = 6;
pub const EXCLUSIVE: TableLockMode = 7;
pub const ACCESS_EXCLUSIVE: TableLockMode = 8;

/// A table and, as the planner opens them, its indexes; a partitioned table with every
/// partition below it (pgmem does not prune partitions at plan time).
pub fn table_with_indexes(catalog: &Catalog, table: &TableDef, mode: TableLockMode, out: &mut Vec<(Oid, TableLockMode)>) {
    out.push((table.oid, mode));
    out.extend(table.indexes.iter().map(|i| (i.oid, mode)));
    for part in catalog.partition_descendants(table.oid) {
        out.push((part.oid, mode));
        out.extend(part.indexes.iter().map(|i| (i.oid, mode)));
    }
}

/// The relations `LOCK TABLE name` locks: the table and (without ONLY) its partitions; a
/// view and, recursively, the relations its query reads.
pub fn lock_statement_targets(catalog: &Catalog, name: &ir::ObjectName, only: bool) -> SqlResult<Vec<(Oid, String)>> {
    let mut out = Vec::new();
    lock_target(catalog, name, only, &mut out, 0)?;
    Ok(out)
}

fn lock_target(catalog: &Catalog, name: &ir::ObjectName, only: bool, out: &mut Vec<(Oid, String)>, depth: usize) -> SqlResult<()> {
    let Some(rel) = catalog.find_relation(name)? else {
        return Err(SqlError::undefined_table(&name.to_string()));
    };
    match &rel {
        Relation::Table(t) => {
            out.push((t.oid, t.name.clone()));
            if !only {
                out.extend(catalog.partition_descendants(t.oid).iter().map(|p| (p.oid, p.name.clone())));
            }
        }
        Relation::View(v) => {
            out.push((v.oid, v.name.clone()));
            if depth < 32 {
                for base in pgmem_types::context::with_search_path(&v.search_path, || crate::views::referenced_relations(&v.query)) {
                    if catalog.find_relation(&base).ok().flatten().is_some() {
                        lock_target(catalog, &base, false, out, depth + 1)?;
                    }
                }
            }
        }
        Relation::Sequence(s) => return Err(cannot_lock(&s.name, "sequences")),
        Relation::Index { index, .. } => return Err(cannot_lock(&index.name, "indexes")),
    }
    Ok(())
}

fn cannot_lock(name: &str, what: &str) -> SqlError {
    SqlError::new(code::WRONG_OBJECT_TYPE, format!("cannot lock relation \"{name}\""))
        .with_detail(format!("This operation is not supported for {what}."))
}

/// The lock level of one ALTER TABLE subcommand on the altered table (`AlterTableGetLockLevel`).
fn alter_action_level(a: &AlterTableAction) -> TableLockMode {
    match a {
        AlterTableAction::ValidateConstraint { .. } | AlterTableAction::AttachPartition { .. } => SHARE_UPDATE_EXCLUSIVE,
        AlterTableAction::EnableTrigger { .. } => SHARE_ROW_EXCLUSIVE,
        AlterTableAction::AddConstraint(c) if is_foreign_key(c) => SHARE_ROW_EXCLUSIVE,
        _ => ACCESS_EXCLUSIVE,
    }
}

fn is_foreign_key(c: &ir::TableConstraint) -> bool {
    matches!(c.kind, ir::TableConstraintKind::ForeignKey { .. })
}

fn foreign_table(c: &ir::TableConstraint) -> Option<&ir::ObjectName> {
    match &c.kind {
        ir::TableConstraintKind::ForeignKey { references, .. } => Some(&references.table),
        _ => None,
    }
}

fn relation_oid(catalog: &Catalog, name: &ir::ObjectName) -> Option<Oid> {
    catalog.find_relation(name).ok().flatten().map(|r| r.oid())
}

fn table_of(catalog: &Catalog, name: &ir::ObjectName) -> Option<std::sync::Arc<TableDef>> {
    match catalog.find_relation(name).ok().flatten() {
        Some(Relation::Table(t)) => Some(t),
        _ => None,
    }
}

/// The locks a DDL statement takes on relations that exist before it runs.
pub fn ddl_locks(stmt: &Statement, catalog: &Catalog) -> Vec<(Oid, TableLockMode)> {
    let mut l = Locks { catalog, out: Vec::new() };
    match stmt {
        Statement::AlterTable(at) => l.alter_table(at),
        Statement::CreateIndex(ci) => l.on(&ci.table, if ci.concurrently { SHARE_UPDATE_EXCLUSIVE } else { SHARE }),
        Statement::CreateTrigger(ct) => l.on(&ct.table, SHARE_ROW_EXCLUSIVE),
        Statement::DropTrigger { table, .. } => {
            l.on(table, ACCESS_EXCLUSIVE);
            l.on(table, ACCESS_SHARE);
        }
        Statement::RenameTrigger { table, .. } => l.on(table, ACCESS_EXCLUSIVE),
        Statement::Comment { target, name, .. } => l.comment(target, name),
        Statement::RenameIndex { name, .. } => l.on(name, SHARE_UPDATE_EXCLUSIVE),
        Statement::RenameView { name, .. } | Statement::SetSchema { name, .. } => l.on(name, ACCESS_EXCLUSIVE),
        Statement::AlterOwner {
            name,
            object: ir::OwnedObject::View | ir::OwnedObject::Sequence | ir::OwnedObject::MaterializedView,
            ..
        } => l.on(name, ACCESS_EXCLUSIVE),
        Statement::AlterSequence { name, rename_to: Some(_), .. } => l.on(name, ACCESS_EXCLUSIVE),
        Statement::AlterSequence { name, .. } => {
            l.on(name, SHARE_ROW_EXCLUSIVE);
            l.on(name, ROW_EXCLUSIVE);
        }
        Statement::RefreshMatview { name, concurrently, .. } => l.on(name, if *concurrently { EXCLUSIVE } else { ACCESS_EXCLUSIVE }),
        Statement::Truncate(t) => {
            for n in &t.tables {
                if let Some(def) = table_of(catalog, n) {
                    table_with_indexes(catalog, &def, ACCESS_EXCLUSIVE, &mut l.out);
                }
            }
        }
        Statement::Drop(d) => l.drop(&d.names),
        // Parse analysis of a view's query locks the relations it reads.
        Statement::CreateView(cv) if !cv.materialized => {
            for n in crate::views::referenced_relations(&cv.query) {
                l.on(&n, ACCESS_SHARE);
            }
        }
        _ => {}
    }
    l.out
}

struct Locks<'a> {
    catalog: &'a Catalog,
    out: Vec<(Oid, TableLockMode)>,
}

impl Locks<'_> {
    fn on(&mut self, name: &ir::ObjectName, mode: TableLockMode) {
        if let Some(oid) = relation_oid(self.catalog, name) {
            self.out.push((oid, mode));
        }
    }

    /// The strongest subcommand's level on the table, plus the other tables subcommands lock.
    fn alter_table(&mut self, at: &ir::AlterTable) {
        if let Some(level) = at.actions.iter().map(alter_action_level).max() {
            self.on(&at.name, level);
        }
        for a in &at.actions {
            match a {
                AlterTableAction::AddConstraint(c) => {
                    if let Some(ft) = foreign_table(c) {
                        self.on(&at.name, ACCESS_SHARE);
                        self.on(ft, SHARE_ROW_EXCLUSIVE);
                        self.on(ft, ACCESS_SHARE);
                        if let Some(t) = table_of(self.catalog, ft) {
                            self.out.extend(t.indexes.iter().filter(|i| i.primary).map(|i| (i.oid, ACCESS_SHARE)));
                        }
                    }
                }
                AlterTableAction::AttachPartition { partition, .. } => {
                    self.on(partition, ACCESS_EXCLUSIVE);
                    // Attaching the partition's indexes to the parent's.
                    if let Some(t) = table_of(self.catalog, &at.name) {
                        self.out.extend(t.indexes.iter().map(|i| (i.oid, SHARE_UPDATE_EXCLUSIVE)));
                    }
                }
                AlterTableAction::DetachPartition { partition } => self.on(partition, ACCESS_EXCLUSIVE),
                _ => {}
            }
        }
    }

    /// COMMENT ON a relation (or column): SHARE UPDATE EXCLUSIVE on it; every COMMENT writes
    /// `pg_description`.
    fn comment(&mut self, target: &ir::CommentTarget, name: &ir::ObjectName) {
        use ir::CommentTarget as T;
        match target {
            T::Column => self.on(&ir::ObjectName(name.0[..name.0.len().saturating_sub(1)].to_vec()), SHARE_UPDATE_EXCLUSIVE),
            T::Table | T::View | T::MaterializedView | T::Sequence | T::Index => self.on(name, SHARE_UPDATE_EXCLUSIVE),
            _ => {}
        }
        if let Some(d) = crate::catalog_views::system_relation(Some("pg_catalog"), "pg_description") {
            self.out.push((d.oid, ROW_EXCLUSIVE));
        }
    }

    /// DROP: ACCESS EXCLUSIVE on each relation, and on an index's table.
    fn drop(&mut self, names: &[ir::ObjectName]) {
        for n in names {
            if let Some(Relation::Index { table, .. }) = self.catalog.find_relation(n).ok().flatten()
                && let Some(t) = self.catalog.table_q(&table)
            {
                self.out.push((t.oid, ACCESS_EXCLUSIVE));
            }
            self.on(n, ACCESS_EXCLUSIVE);
        }
    }
}

/// The locks on relations a statement created: ACCESS EXCLUSIVE on each, and an index
/// build's SHARE lock on its table.
pub fn created_locks(stmt: &Statement, before: &Catalog, after: &Catalog) -> Vec<(Oid, TableLockMode)> {
    let mut out = Vec::new();
    let is_new = |oid: Oid| before.relation_by_oid(oid).is_none();
    let new_table = |name: &ir::ObjectName| match after.find_relation(name).ok().flatten() {
        Some(r) if is_new(r.oid()) => Some(r),
        _ => None,
    };
    match stmt {
        Statement::CreateTable(ct) => {
            if let Some(Relation::Table(t)) = new_table(&ct.name) {
                out.push((t.oid, ACCESS_EXCLUSIVE));
                if !t.indexes.is_empty() {
                    out.push((t.oid, SHARE));
                }
                out.extend(t.indexes.iter().map(|i| (i.oid, ACCESS_EXCLUSIVE)));
            }
        }
        Statement::CreateView(cv) => {
            if let Some(r) = new_table(&cv.name).or_else(|| after.find_relation(&cv.name).ok().flatten()) {
                out.push((r.oid(), ACCESS_EXCLUSIVE));
            }
        }
        Statement::CreateSequence(cs) => {
            if let Some(r) = new_table(&cs.name) {
                out.push((r.oid(), ACCESS_EXCLUSIVE));
            }
        }
        Statement::CreateIndex(ci) => out.extend(new_indexes(before, after, &ci.table)),
        Statement::AlterTable(at) => {
            let new = new_indexes(before, after, &at.name);
            if let (false, Some(t)) = (new.is_empty(), table_of(after, &at.name)) {
                out.push((t.oid, SHARE));
            }
            out.extend(new);
        }
        _ => {}
    }
    out
}

/// ACCESS EXCLUSIVE on each index of `table` that `before` did not have.
fn new_indexes(before: &Catalog, after: &Catalog, table: &ir::ObjectName) -> Vec<(Oid, TableLockMode)> {
    let Some(t) = table_of(after, table) else { return Vec::new() };
    t.indexes.iter().filter(|i| before.relation_by_oid(i.oid).is_none()).map(|i| (i.oid, ACCESS_EXCLUSIVE)).collect()
}
