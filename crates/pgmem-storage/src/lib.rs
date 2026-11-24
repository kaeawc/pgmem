//! In-memory storage with Postgres-like transaction semantics.
//!
//! The committed database is an immutable [`DbState`] (catalog + table contents) behind an
//! `Arc`; readers clone the `Arc` and never block writers. A [`Txn`] records its effects as a
//! row-level overlay — rows it inserted and row ids it deleted (an UPDATE is a delete plus an
//! insert under a fresh row id, so updated rows move to the end of a sequential scan, as in
//! Postgres) — plus a private copy of the catalog when it runs DDL. COMMIT replays the overlay
//! onto the latest committed state under a short commit lock.
//!
//! Visibility follows the isolation level: READ COMMITTED transactions see the latest
//! committed state at the start of each statement; REPEATABLE READ and SERIALIZABLE keep the
//! snapshot taken at their first statement. Two transactions conflict only when they delete
//! or update the same row: the later COMMIT fails with `40001`, standing in for the row lock
//! wait Postgres would perform. Sequences are non-transactional, as in Postgres.

use pgmem_catalog::{Catalog, Oid, SequenceDef, TableDef};
use pgmem_types::{SqlError, SqlResult, Value, code, compare_values};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

pub type RowId = u64;
/// A stored tuple: one value per physical column slot (dropped columns included). Rows written
/// before an ADD COLUMN are shorter; readers pad them with the column's missing value.
pub type Row = Vec<Value>;

#[derive(Clone, Debug, Default)]
pub struct TableData {
    pub rows: BTreeMap<RowId, Row>,
}

/// A committed database version.
#[derive(Clone, Debug, Default)]
pub struct DbState {
    pub catalog: Arc<Catalog>,
    pub tables: HashMap<Oid, Arc<TableData>>,
    /// Bumped on every committed catalog change; used to detect concurrent DDL.
    pub catalog_version: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Isolation {
    #[default]
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl Isolation {
    pub fn name(self) -> &'static str {
        match self {
            Isolation::ReadCommitted => "read committed",
            Isolation::RepeatableRead => "repeatable read",
            Isolation::Serializable => "serializable",
        }
    }
}

#[derive(Debug, Default)]
struct SeqState {
    /// The last value handed out, if any.
    last: Option<i64>,
}

/// The shared database. One per pgmem server.
#[derive(Debug)]
pub struct Database {
    state: RwLock<Arc<DbState>>,
    commit_lock: Mutex<()>,
    next_row_id: AtomicU64,
    sequences: Mutex<HashMap<Oid, SeqState>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for Database {
    fn default() -> Self {
        Database::new()
    }
}

impl Database {
    pub fn new() -> Database {
        Database {
            state: RwLock::new(Arc::new(DbState { catalog: Arc::new(Catalog::new()), ..Default::default() })),
            commit_lock: Mutex::new(()),
            next_row_id: AtomicU64::new(1),
            sequences: Mutex::new(HashMap::new()),
        }
    }

    /// The latest committed state.
    pub fn current(&self) -> Arc<DbState> {
        Arc::clone(&self.state.read().unwrap_or_else(PoisonError::into_inner))
    }

    fn alloc_row_id(&self) -> RowId {
        self.next_row_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Starts a transaction. Nothing is snapshotted until the first statement.
    pub fn begin(self: &Arc<Self>, isolation: Isolation) -> Txn {
        let base = self.current();
        Txn {
            db: Arc::clone(self),
            isolation,
            base,
            started: false,
            catalog: None,
            catalog_base_version: 0,
            changes: HashMap::new(),
            savepoints: Vec::new(),
            seq_undo: SeqUndo { db: Arc::clone(self), entries: Vec::new() },
        }
    }

    /// `nextval`: non-transactional, so a rolled-back transaction leaves a gap.
    pub fn nextval(&self, seq: &SequenceDef) -> SqlResult<i64> {
        let mut seqs = lock(&self.sequences);
        let st = seqs.entry(seq.oid).or_default();
        let next = match st.last {
            None => seq.start,
            Some(last) => last.checked_add(seq.increment).ok_or_else(|| reached_limit(seq))?,
        };
        if next > seq.max || next < seq.min {
            return Err(reached_limit(seq));
        }
        st.last = Some(next);
        Ok(next)
    }

    /// `setval(seq, value, is_called)`.
    pub fn setval(&self, seq: &SequenceDef, value: i64, is_called: bool) -> SqlResult<i64> {
        if value > seq.max || value < seq.min {
            return Err(SqlError::new(
                code::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("setval: value {value} is out of bounds for sequence \"{}\" ({}..{})", seq.name, seq.min, seq.max),
            ));
        }
        let mut seqs = lock(&self.sequences);
        let st = seqs.entry(seq.oid).or_default();
        st.last = if is_called { Some(value) } else { value.checked_sub(seq.increment) };
        Ok(value)
    }

    /// The sequence's last value (`pg_sequences.last_value`), if it was ever used.
    pub fn sequence_last_value(&self, seq: &SequenceDef) -> Option<i64> {
        lock(&self.sequences).get(&seq.oid).and_then(|s| s.last)
    }

    /// Resets a sequence to its start (`TRUNCATE ... RESTART IDENTITY`, `ALTER SEQUENCE ... RESTART`).
    /// Returns its previous last value.
    pub fn reset_sequence(&self, seq: &SequenceDef) -> Option<i64> {
        lock(&self.sequences).remove(&seq.oid).and_then(|s| s.last)
    }

    fn restore_sequence(&self, oid: Oid, last: Option<i64>) {
        let mut seqs = lock(&self.sequences);
        match last {
            Some(v) => {
                seqs.insert(oid, SeqState { last: Some(v) });
            }
            None => {
                seqs.remove(&oid);
            }
        }
    }

    /// Appends rows to a table outside any transaction (the `Server::seed` test helper).
    pub fn seed(&self, table: &str, rows: Vec<Row>) -> SqlResult<()> {
        let _guard = lock(&self.commit_lock);
        let cur = self.current();
        let def = cur.catalog.table(table).ok_or_else(|| SqlError::undefined_table(table))?;
        let mut next = (*cur).clone();
        let data = Arc::make_mut(next.tables.entry(def.oid).or_default());
        for row in rows {
            data.rows.insert(self.alloc_row_id(), row);
        }
        *self.state.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(next);
        Ok(())
    }
}

fn reached_limit(seq: &SequenceDef) -> SqlError {
    let bound = if seq.increment > 0 {
        format!("maximum value of sequence \"{}\" ({})", seq.name, seq.max)
    } else {
        format!("minimum value of sequence \"{}\" ({})", seq.name, seq.min)
    };
    SqlError::new("2200H", format!("nextval: reached {bound}"))
}

/// One table's pending changes inside a transaction.
#[derive(Clone, Debug, Default)]
struct TableChanges {
    inserted: BTreeMap<RowId, Row>,
    deleted: BTreeSet<RowId>,
    /// TRUNCATE (or creation in this transaction): committed rows are invisible.
    truncated: bool,
}

#[derive(Clone, Debug)]
struct Savepoint {
    name: String,
    catalog: Option<Catalog>,
    changes: HashMap<Oid, TableChanges>,
    /// `seq_undo` length when the savepoint was set.
    seq_undo: usize,
}

/// Sequence states to restore if the transaction rolls back: a transactional sequence reset
/// (`TRUNCATE ... RESTART IDENTITY`) gives the sequence a new relfilenode in Postgres, so on
/// rollback it returns to its state before the reset, discarding later `nextval` calls too.
#[derive(Debug)]
struct SeqUndo {
    db: Arc<Database>,
    entries: Vec<(Oid, Option<i64>)>,
}

impl SeqUndo {
    fn undo_to(&mut self, len: usize) {
        while self.entries.len() > len {
            if let Some((oid, last)) = self.entries.pop() {
                self.db.restore_sequence(oid, last);
            }
        }
    }
}

impl Drop for SeqUndo {
    fn drop(&mut self) {
        self.undo_to(0);
    }
}

/// A transaction. Dropping it without committing rolls it back.
#[derive(Debug)]
pub struct Txn {
    db: Arc<Database>,
    isolation: Isolation,
    /// The committed state this transaction currently reads from.
    base: Arc<DbState>,
    started: bool,
    /// Private catalog copy, present once this transaction has run DDL.
    catalog: Option<Catalog>,
    catalog_base_version: u64,
    changes: HashMap<Oid, TableChanges>,
    savepoints: Vec<Savepoint>,
    seq_undo: SeqUndo,
}

impl Txn {
    /// Resets a sequence to its start, undone if the transaction (or the enclosing savepoint)
    /// rolls back.
    pub fn reset_sequence(&mut self, seq: &SequenceDef) {
        let prev = self.db.reset_sequence(seq);
        self.seq_undo.entries.push((seq.oid, prev));
    }

    pub fn isolation(&self) -> Isolation {
        self.isolation
    }

    /// Allowed only before the first statement (`SET TRANSACTION ISOLATION LEVEL`).
    pub fn set_isolation(&mut self, isolation: Isolation) -> SqlResult<()> {
        if self.started {
            return Err(SqlError::new(code::ACTIVE_SQL_TRANSACTION, "SET TRANSACTION ISOLATION LEVEL must be called before any query"));
        }
        self.isolation = isolation;
        Ok(())
    }

    /// Call at the start of every statement: READ COMMITTED refreshes its snapshot, the
    /// stricter levels take theirs on the first statement only.
    pub fn begin_statement(&mut self) {
        if !self.started || self.isolation == Isolation::ReadCommitted {
            self.base = self.db.current();
        }
        self.started = true;
    }

    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    pub fn catalog(&self) -> &Catalog {
        match &self.catalog {
            Some(c) => c,
            None => &self.base.catalog,
        }
    }

    /// A mutable catalog for DDL. The first call copies the visible catalog.
    pub fn catalog_mut(&mut self) -> &mut Catalog {
        if self.catalog.is_none() {
            self.catalog_base_version = self.base.catalog_version;
            self.catalog = Some((*self.base.catalog).clone());
        }
        self.catalog.get_or_insert_with(Catalog::new)
    }

    /// True if this transaction has written anything (rows or DDL).
    pub fn has_writes(&self) -> bool {
        self.catalog.is_some() || self.changes.values().any(|c| c.truncated || !c.inserted.is_empty() || !c.deleted.is_empty())
    }

    /// The rows of a table visible to this transaction, in row-id (heap) order.
    pub fn scan(&self, table: Oid) -> Vec<(RowId, Row)> {
        let changes = self.changes.get(&table);
        let mut out = Vec::new();
        let truncated = changes.is_some_and(|c| c.truncated);
        if !truncated && let Some(data) = self.base.tables.get(&table) {
            for (id, row) in &data.rows {
                if changes.is_some_and(|c| c.deleted.contains(id)) {
                    continue;
                }
                out.push((*id, row.clone()));
            }
        }
        if let Some(c) = changes {
            out.extend(c.inserted.iter().map(|(id, r)| (*id, r.clone())));
            if !truncated && !c.inserted.is_empty() {
                out.sort_by_key(|(id, _)| *id);
            }
        }
        out
    }

    /// Fetches one visible row.
    pub fn get(&self, table: Oid, id: RowId) -> Option<Row> {
        let changes = self.changes.get(&table);
        if let Some(r) = changes.and_then(|c| c.inserted.get(&id)) {
            return Some(r.clone());
        }
        if changes.is_some_and(|c| c.truncated || c.deleted.contains(&id)) {
            return None;
        }
        self.base.tables.get(&table).and_then(|d| d.rows.get(&id).cloned())
    }

    pub fn insert(&mut self, table: Oid, row: Row) -> RowId {
        let id = self.db.alloc_row_id();
        self.changes.entry(table).or_default().inserted.insert(id, row);
        id
    }

    pub fn delete(&mut self, table: Oid, id: RowId) {
        let ch = self.changes.entry(table).or_default();
        if ch.inserted.remove(&id).is_none() {
            ch.deleted.insert(id);
        }
    }

    /// Replaces a row; the new version gets a fresh row id (moves to the end of the heap).
    pub fn update(&mut self, table: Oid, id: RowId, row: Row) -> RowId {
        self.delete(table, id);
        self.insert(table, row)
    }

    /// Rewrites every visible row of a table in place, keeping row ids (used by ALTER TABLE
    /// operations that change the physical row shape inside this transaction).
    pub fn rewrite(&mut self, table: Oid, mut f: impl FnMut(&mut Row)) {
        let rows = self.scan(table);
        let ch = self.changes.entry(table).or_default();
        ch.truncated = true;
        ch.deleted.clear();
        ch.inserted.clear();
        for (id, mut row) in rows {
            f(&mut row);
            ch.inserted.insert(id, row);
        }
    }

    pub fn truncate(&mut self, table: Oid) {
        let ch = self.changes.entry(table).or_default();
        ch.truncated = true;
        ch.inserted.clear();
        ch.deleted.clear();
    }

    /// Marks a table created by this transaction (no committed rows can be visible).
    pub fn created_table(&mut self, table: Oid) {
        self.changes.entry(table).or_default().truncated = true;
    }

    pub fn savepoint(&mut self, name: &str) {
        self.savepoints.push(Savepoint {
            name: name.to_owned(),
            catalog: self.catalog.clone(),
            changes: self.changes.clone(),
            seq_undo: self.seq_undo.entries.len(),
        });
    }

    fn find_savepoint(&self, name: &str) -> SqlResult<usize> {
        self.savepoints
            .iter()
            .rposition(|s| s.name == name)
            .ok_or_else(|| SqlError::new(code::INVALID_SAVEPOINT_SPECIFICATION, format!("savepoint \"{name}\" does not exist")))
    }

    /// ROLLBACK TO: restores the state at the savepoint, which stays defined.
    pub fn rollback_to(&mut self, name: &str) -> SqlResult<()> {
        let idx = self.find_savepoint(name)?;
        let sp = self.savepoints[idx].clone();
        self.catalog = sp.catalog;
        self.changes = sp.changes;
        self.seq_undo.undo_to(sp.seq_undo);
        self.savepoints.truncate(idx + 1);
        Ok(())
    }

    /// RELEASE: forgets the savepoint and every later one; effects are kept.
    pub fn release(&mut self, name: &str) -> SqlResult<()> {
        let idx = self.find_savepoint(name)?;
        self.savepoints.truncate(idx);
        Ok(())
    }

    /// Applies this transaction's effects to the latest committed state.
    pub fn commit(mut self) -> SqlResult<()> {
        let undo = std::mem::take(&mut self.seq_undo.entries);
        let result = self.commit_changes();
        if result.is_err() {
            self.seq_undo.entries = undo;
        }
        result
    }

    fn commit_changes(&mut self) -> SqlResult<()> {
        if !self.has_writes() {
            return Ok(());
        }
        let db = Arc::clone(&self.db);
        let _guard = lock(&db.commit_lock);
        let cur = db.current();
        let mut next = (*cur).clone();
        if let Some(catalog) = self.catalog.take() {
            if cur.catalog_version != self.catalog_base_version {
                return Err(SqlError::new(code::SERIALIZATION_FAILURE, "could not serialize access due to concurrent schema change"));
            }
            next.tables.retain(|oid, _| catalog.table_by_oid(*oid).is_some());
            next.catalog = Arc::new(catalog);
            next.catalog_version += 1;
        }
        for (oid, ch) in std::mem::take(&mut self.changes) {
            let Some(def) = next.catalog.table_by_oid(oid).cloned() else {
                continue; // dropped in this transaction
            };
            let data = Arc::make_mut(next.tables.entry(oid).or_default());
            if ch.truncated {
                data.rows.clear();
            }
            for id in &ch.deleted {
                if data.rows.remove(id).is_none() {
                    return Err(SqlError::new(code::SERIALIZATION_FAILURE, "could not serialize access due to concurrent update"));
                }
            }
            for (id, row) in &ch.inserted {
                data.rows.insert(*id, row.clone());
            }
            if !ch.inserted.is_empty() {
                recheck_unique(&def, data, &ch.inserted)?;
            }
        }
        *db.state.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(next);
        Ok(())
    }

    /// Discards this transaction's effects.
    pub fn rollback(self) {}
}

/// Re-validates plain-column unique indexes for rows inserted by a committing transaction
/// against everything committed concurrently (Postgres would have blocked on the conflicting
/// insert and then raised the same error).
fn recheck_unique(def: &TableDef, data: &TableData, inserted: &BTreeMap<RowId, Row>) -> SqlResult<()> {
    for idx in def.indexes.iter().filter(|i| i.unique && i.exprs.is_empty() && i.predicate.is_none()) {
        for (id, row) in inserted {
            let key: Vec<&Value> = idx.columns.iter().map(|c| row.get(*c).unwrap_or(&Value::Null)).collect();
            if !idx.nulls_not_distinct && key.iter().any(|v| v.is_null()) {
                continue;
            }
            let dup = data.rows.iter().any(|(other_id, other)| {
                other_id != id
                    && idx.columns.iter().zip(&key).all(|(c, v)| {
                        let o = other.get(*c).unwrap_or(&Value::Null);
                        if o.is_null() || v.is_null() {
                            o.is_null() && v.is_null()
                        } else {
                            compare_values(o, v).is_ok_and(std::cmp::Ordering::is_eq)
                        }
                    })
            });
            if dup {
                return Err(SqlError::new(
                    code::UNIQUE_VIOLATION,
                    format!("duplicate key value violates unique constraint \"{}\"", idx.name),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
