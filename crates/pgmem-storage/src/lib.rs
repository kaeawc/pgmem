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

pub mod advisory;
pub mod locks;
pub mod notify;

pub use notify::{Notification, NotifyHub};

use pgmem_catalog::{Catalog, Oid, SequenceDef, TableDef};
use pgmem_types::{SqlError, SqlResult, Value, code, compare_values};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
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

#[derive(Clone, Copy, Debug)]
struct SeqState {
    /// `last_value`: the last value handed out, or the next one when not `called`.
    value: i64,
    /// `is_called`.
    called: bool,
}

/// The first backend pid handed out (Postgres pids are never small).
const FIRST_BACKEND_PID: i32 = 10_000;

/// The shared database. One per pgmem server.
#[derive(Debug)]
pub struct Database {
    state: RwLock<Arc<DbState>>,
    commit_lock: Mutex<()>,
    next_row_id: AtomicU64,
    sequences: Mutex<HashMap<Oid, SeqState>>,
    notify: notify::NotifyHub,
    advisory: advisory::AdvisoryLocks,
    locks: locks::LockTable,
    next_txn_id: AtomicU64,
    /// Bumped by every commit that wrote something.
    commit_seq: AtomicU64,
    /// Read / write sets of SERIALIZABLE transactions, for write-skew detection.
    ssi: Mutex<Ssi>,
    /// Backend process ids handed to sessions (wire connections and embedded sessions).
    next_backend_pid: AtomicI32,
}

/// Serializable transactions in flight (by id: the commit sequence their snapshot saw) and
/// the ones that committed since the oldest of them started.
#[derive(Debug, Default)]
struct Ssi {
    active: HashMap<u64, u64>,
    committed: Vec<SsiRecord>,
}

#[derive(Debug)]
struct SsiRecord {
    seq: u64,
    reads: BTreeSet<Oid>,
    writes: BTreeSet<Oid>,
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
            notify: notify::NotifyHub::default(),
            advisory: advisory::AdvisoryLocks::default(),
            locks: locks::LockTable::default(),
            next_txn_id: AtomicU64::new(1),
            commit_seq: AtomicU64::new(0),
            ssi: Mutex::new(Ssi::default()),
            next_backend_pid: AtomicI32::new(FIRST_BACKEND_PID),
        }
    }

    /// An independent copy of the committed state: catalog (schemas, tables, views, types,
    /// functions, roles, grants...), table contents and sequence values. Cheap: the committed
    /// state is immutable and shared until either copy writes to it (copy-on-write per
    /// table). Work in transactions that have not committed is not included, nor are session
    /// state, LISTEN registrations, advisory or row locks. Afterwards the two databases are
    /// fully independent.
    pub fn fork(&self) -> Database {
        // Under the commit lock, so the state and the sequences come from the same moment.
        let _guard = lock(&self.commit_lock);
        let fork = Database::new();
        *fork.state.write().unwrap_or_else(PoisonError::into_inner) = self.current();
        *lock(&fork.sequences) = lock(&self.sequences).clone();
        // Row ids only need to be unique within a database: continue after the source's.
        fork.next_row_id.store(self.next_row_id.load(Ordering::Relaxed), Ordering::Relaxed);
        fork.commit_seq.store(self.commit_seq.load(Ordering::SeqCst), Ordering::SeqCst);
        fork
    }

    /// A fresh backend process id for a new session (`pg_backend_pid()`, temp schema names).
    pub fn next_backend_pid(&self) -> i32 {
        self.next_backend_pid.fetch_add(1, Ordering::Relaxed)
    }

    /// The advisory lock table shared by every session.
    pub fn advisory(&self) -> &advisory::AdvisoryLocks {
        &self.advisory
    }

    /// The LISTEN / NOTIFY queue shared by every session.
    pub fn notifications(&self) -> &notify::NotifyHub {
        &self.notify
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
            id: self.next_txn_id.fetch_add(1, Ordering::Relaxed),
            held_rows: std::cell::RefCell::new(Vec::new()),
            snapshot_seq: 0,
            reads: std::cell::RefCell::new(BTreeSet::new()),
            writes: BTreeSet::new(),
            db: Arc::clone(self),
            isolation,
            base,
            started: false,
            catalog: None,
            catalog_base_version: 0,
            changes: HashMap::new(),
            frozen: None,
            savepoints: Vec::new(),
            seq_undo: SeqUndo { db: Arc::clone(self), entries: Vec::new() },
            side: std::cell::RefCell::new(None),
            forked: false,
        }
    }

    /// `nextval`: non-transactional, so a rolled-back transaction leaves a gap.
    pub fn nextval(&self, seq: &SequenceDef) -> SqlResult<i64> {
        let mut seqs = lock(&self.sequences);
        let st = seqs.entry(seq.oid).or_insert(SeqState { value: seq.start, called: false });
        let next = if st.called { st.value.checked_add(seq.increment) } else { Some(st.value) };
        let next = match next {
            Some(n) if (seq.min..=seq.max).contains(&n) => n,
            // CYCLE wraps to the other end.
            _ if seq.cycle => {
                if seq.increment > 0 {
                    seq.min
                } else {
                    seq.max
                }
            }
            _ => return Err(reached_limit(seq)),
        };
        *st = SeqState { value: next, called: true };
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
        lock(&self.sequences).insert(seq.oid, SeqState { value, called: is_called });
        Ok(value)
    }

    /// The sequence's last value (`pg_sequences.last_value`), if it was ever used.
    pub fn sequence_last_value(&self, seq: &SequenceDef) -> Option<i64> {
        lock(&self.sequences).get(&seq.oid).filter(|s| s.called).map(|s| s.value)
    }

    /// `SELECT last_value, is_called FROM seq`.
    pub fn sequence_state(&self, seq: &SequenceDef) -> (i64, bool) {
        lock(&self.sequences).get(&seq.oid).map_or((seq.start, false), |s| (s.value, s.called))
    }

    /// Restarts a sequence at `at` (its start when None): the next nextval returns it.
    /// Returns its previous state.
    fn restart_sequence(&self, seq: &SequenceDef, at: Option<i64>) -> Option<SeqState> {
        let mut seqs = lock(&self.sequences);
        match at {
            Some(value) => seqs.insert(seq.oid, SeqState { value, called: false }),
            None => seqs.remove(&seq.oid),
        }
    }

    fn restore_sequence(&self, oid: Oid, state: Option<SeqState>) {
        let mut seqs = lock(&self.sequences);
        match state {
            Some(st) => {
                seqs.insert(oid, st);
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
        // `schema.table`, or a table in public.
        let (schema, name) = table.split_once('.').unwrap_or((pgmem_catalog::PUBLIC_SCHEMA, table));
        let def = cur.catalog.table(schema, name).ok_or_else(|| SqlError::undefined_table(table))?;
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
    /// `held_rows` length when the savepoint was set.
    row_locks: usize,
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
    entries: Vec<(Oid, Option<SeqState>)>,
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
    /// Owner of the transaction's row and table locks.
    id: u64,
    /// Row locks taken, in order (ROLLBACK TO releases the later ones).
    held_rows: std::cell::RefCell<Vec<(Oid, RowId, locks::RowLockMode)>>,
    /// The commit sequence the snapshot saw (SERIALIZABLE write-skew detection).
    snapshot_seq: u64,
    /// Tables this transaction's queries read and tables it wrote.
    reads: std::cell::RefCell<BTreeSet<Oid>>,
    writes: BTreeSet<Oid>,
    db: Arc<Database>,
    isolation: Isolation,
    /// The committed state this transaction currently reads from.
    base: Arc<DbState>,
    started: bool,
    /// Private catalog copy, present once this transaction has run DDL.
    catalog: Option<Catalog>,
    catalog_base_version: u64,
    changes: HashMap<Oid, TableChanges>,
    /// The changes reads see while a statement with data-modifying WITH queries runs: all its
    /// parts read the snapshot from the statement's start ([`Txn::freeze_reads`]).
    frozen: Option<HashMap<Oid, TableChanges>>,
    savepoints: Vec<Savepoint>,
    seq_undo: SeqUndo,
    /// Writes made by volatile functions a statement called while it read this transaction
    /// ([`Txn::with_side`]): applied on this transaction's next change, so the calling
    /// statement keeps its snapshot.
    side: std::cell::RefCell<Option<Box<Txn>>>,
    /// A [`Txn::with_side`] copy: dropping it releases nothing (the transaction owns its locks).
    forked: bool,
}

impl Txn {
    /// Runs `f` on a writable copy of this transaction while a statement reads it: a volatile
    /// function called from a query may modify data. Later calls during the same statement
    /// see the copy's changes; the statement itself does not (its snapshot is fixed). The
    /// copy replaces this transaction's state on its next change ([`Txn::settle`]), which
    /// is sound because a transaction cannot change while it is borrowed for reading.
    pub fn with_side<R>(&self, f: impl FnOnce(&mut Txn) -> SqlResult<R>) -> SqlResult<R> {
        let mut side = self.side.try_borrow_mut().map_err(|_| SqlError::internal("nested write in a function call"))?;
        let fork = side.get_or_insert_with(|| {
            Box::new(Txn {
                id: self.id,
                held_rows: std::cell::RefCell::new(Vec::new()),
                snapshot_seq: self.snapshot_seq,
                reads: std::cell::RefCell::new(BTreeSet::new()),
                writes: self.writes.clone(),
                db: Arc::clone(&self.db),
                isolation: self.isolation,
                base: Arc::clone(&self.base),
                started: self.started,
                catalog: self.catalog.clone(),
                catalog_base_version: self.catalog_base_version,
                changes: self.changes.clone(),
                frozen: None,
                savepoints: Vec::new(),
                seq_undo: SeqUndo { db: Arc::clone(&self.db), entries: Vec::new() },
                side: std::cell::RefCell::new(None),
                forked: true,
            })
        });
        f(fork)
    }

    /// Whether functions have written through [`Txn::with_side`] since this transaction's
    /// last change.
    pub fn has_side(&self) -> bool {
        self.side.try_borrow().map_or(true, |s| s.is_some())
    }

    /// Starts a PL/pgSQL exception block that runs while a statement reads this transaction:
    /// writes functions make in it go to the [`Txn::with_side`] copy, which gets savepoint
    /// `name` if it exists. Returns whether it did; pass that to [`Txn::side_end`].
    pub fn side_savepoint(&self, name: &str) -> SqlResult<bool> {
        let mut side = self.side.try_borrow_mut().map_err(|_| SqlError::internal("nested write in a function call"))?;
        Ok(match side.as_mut() {
            Some(fork) => {
                fork.savepoint(name);
                true
            }
            None => false,
        })
    }

    /// Ends a [`Txn::side_savepoint`] block: keeps its writes, or undoes them (`rollback`)
    /// like the subtransaction Postgres runs the block in.
    pub fn side_end(&self, name: &str, existed: bool, rollback: bool) -> SqlResult<()> {
        let mut side = self.side.try_borrow_mut().map_err(|_| SqlError::internal("nested write in a function call"))?;
        match side.as_mut() {
            Some(fork) if existed => {
                if rollback {
                    fork.rollback_to(name)?;
                }
                fork.release(name)
            }
            // The copy was made inside the block: all of it is the block's.
            _ if rollback => {
                side.take();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Takes over the changes functions made through [`Txn::with_side`].
    fn settle(&mut self) {
        let Some(mut fork) = self.side.get_mut().take() else { return };
        fork.settle();
        self.changes = std::mem::take(&mut fork.changes);
        self.catalog = fork.catalog.take();
        self.catalog_base_version = fork.catalog_base_version;
        self.writes = std::mem::take(&mut fork.writes);
        self.base = Arc::clone(&fork.base);
        self.started = fork.started;
        self.snapshot_seq = fork.snapshot_seq;
        self.held_rows.get_mut().append(fork.held_rows.get_mut());
        self.reads.get_mut().append(fork.reads.get_mut());
        self.seq_undo.entries.append(&mut fork.seq_undo.entries);
    }

    /// Until [`Txn::thaw_reads`], reads see this transaction's changes as of now; writes and
    /// the `*_latest` reads (constraint checks) see everything.
    pub fn freeze_reads(&mut self) {
        self.settle();
        self.frozen = Some(self.changes.clone());
    }

    pub fn thaw_reads(&mut self) {
        self.settle();
        self.frozen = None;
    }

    fn read_changes(&self) -> &HashMap<Oid, TableChanges> {
        self.frozen.as_ref().unwrap_or(&self.changes)
    }

    /// [`Txn::scan`] ignoring [`Txn::freeze_reads`]: every change made so far.
    pub fn scan_latest(&self, table: Oid) -> Vec<(RowId, Row)> {
        scan_with(&self.base, &self.changes, table)
    }

    /// Resets a sequence to its start, undone if the transaction (or the enclosing savepoint)
    /// rolls back.
    /// Resets a sequence to its start (`TRUNCATE ... RESTART IDENTITY`, `ALTER SEQUENCE ...
    /// RESTART`), undone if the transaction rolls back.
    pub fn reset_sequence(&mut self, seq: &SequenceDef) {
        self.restart_sequence(seq, None);
    }

    /// `ALTER SEQUENCE ... RESTART [WITH at]`.
    pub fn restart_sequence(&mut self, seq: &SequenceDef, at: Option<i64>) {
        self.settle();
        let prev = self.db.restart_sequence(seq, at);
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
        self.settle();
        if !self.started || self.isolation == Isolation::ReadCommitted {
            self.base = self.db.current();
        }
        if !self.started {
            self.snapshot_seq = self.db.commit_seq.load(Ordering::SeqCst);
            if self.isolation == Isolation::Serializable {
                lock(&self.db.ssi).active.insert(self.id, self.snapshot_seq);
            }
        }
        self.started = true;
    }

    /// Records that a query read `table` (SERIALIZABLE read set; Postgres' relation-level
    /// predicate lock of a sequential scan).
    pub fn note_read(&self, table: Oid) {
        if self.isolation == Isolation::Serializable {
            self.reads.borrow_mut().insert(table);
        }
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
        self.settle();
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
        scan_with(&self.base, self.read_changes(), table)
    }

    /// Fetches one visible row.
    pub fn get(&self, table: Oid, id: RowId) -> Option<Row> {
        let changes = self.read_changes().get(&table);
        if let Some(r) = changes.and_then(|c| c.inserted.get(&id)) {
            return Some(r.clone());
        }
        if changes.is_some_and(|c| c.truncated || c.deleted.contains(&id)) {
            return None;
        }
        self.base.tables.get(&table).and_then(|d| d.rows.get(&id).cloned())
    }

    pub fn insert(&mut self, table: Oid, row: Row) -> RowId {
        self.settle();
        self.writes.insert(table);
        let id = self.db.alloc_row_id();
        self.changes.entry(table).or_default().inserted.insert(id, row);
        id
    }

    pub fn delete(&mut self, table: Oid, id: RowId) {
        self.settle();
        self.writes.insert(table);
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
        self.settle();
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
        self.settle();
        let ch = self.changes.entry(table).or_default();
        ch.truncated = true;
        ch.inserted.clear();
        ch.deleted.clear();
    }

    /// Marks a table created by this transaction (no committed rows can be visible).
    pub fn created_table(&mut self, table: Oid) {
        self.settle();
        self.changes.entry(table).or_default().truncated = true;
    }

    pub fn savepoint(&mut self, name: &str) {
        self.settle();
        self.savepoints.push(Savepoint {
            name: name.to_owned(),
            row_locks: self.held_rows.borrow().len(),
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
        self.settle();
        let idx = self.find_savepoint(name)?;
        let sp = self.savepoints[idx].clone();
        self.catalog = sp.catalog;
        self.changes = sp.changes;
        self.seq_undo.undo_to(sp.seq_undo);
        self.savepoints.truncate(idx + 1);
        let released: Vec<_> = self.held_rows.borrow_mut().drain(sp.row_locks..).collect();
        self.db.locks.release_rows(self.id, &released);
        Ok(())
    }

    /// RELEASE: forgets the savepoint and every later one; effects are kept.
    pub fn release(&mut self, name: &str) -> SqlResult<()> {
        self.settle();
        let idx = self.find_savepoint(name)?;
        self.savepoints.truncate(idx);
        Ok(())
    }

    /// Applies this transaction's effects to the latest committed state.
    pub fn commit(mut self) -> SqlResult<()> {
        self.settle();
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
        if self.isolation == Isolation::Serializable {
            self.check_write_skew()?;
        }
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
        let seq = db.commit_seq.fetch_add(1, Ordering::SeqCst) + 1;
        if self.isolation == Isolation::Serializable {
            let reads = self.reads.borrow().clone();
            lock(&db.ssi).committed.push(SsiRecord { seq, reads, writes: self.writes.clone() });
        }
        Ok(())
    }

    /// SERIALIZABLE: a concurrent serializable transaction that committed after this one's
    /// snapshot wrote what this one read and read what this one writes (a write skew).
    fn check_write_skew(&self) -> SqlResult<()> {
        let reads = self.reads.borrow();
        let ssi = lock(&self.db.ssi);
        let skew = ssi.committed.iter().any(|other| {
            other.seq > self.snapshot_seq
                && other.writes.iter().any(|t| reads.contains(t))
                && self.writes.iter().any(|t| other.reads.contains(t))
        });
        if skew {
            return Err(SqlError::new(
                code::SERIALIZATION_FAILURE,
                "could not serialize access due to read/write dependencies among transactions",
            )
            .with_detail("Reason code: Canceled on identification as a pivot, during commit attempt.")
            .with_hint("The transaction might succeed if retried."));
        }
        Ok(())
    }

    /// Discards this transaction's effects.
    pub fn rollback(self) {}

    /// Locks a row of `table` (named `table_name` in errors) for this transaction. In
    /// REPEATABLE READ and SERIALIZABLE, a row another transaction changed since the snapshot
    /// is a serialization failure. Returns false when SKIP LOCKED skips it.
    pub fn lock_row(&self, table: Oid, table_name: &str, id: RowId, mode: locks::RowLockMode, wait: LockWait) -> SqlResult<bool> {
        if self.isolation != Isolation::ReadCommitted && !self.row_is_current(table, id) {
            return Err(SqlError::new(code::SERIALIZATION_FAILURE, "could not serialize access due to concurrent update"));
        }
        if self.db.locks.row_conflict(self.id, table, id, mode) {
            match wait {
                LockWait::NoWait => {
                    return Err(SqlError::new(
                        code::LOCK_NOT_AVAILABLE,
                        format!("could not obtain lock on row in relation \"{table_name}\""),
                    ));
                }
                LockWait::SkipLocked => return Ok(false),
                LockWait::Block => {}
            }
        }
        self.db.locks.grant_row(self.id, table, id, mode);
        self.held_rows.borrow_mut().push((table, id, mode));
        Ok(true)
    }

    /// `LOCK TABLE ... IN mode MODE [NOWAIT]`.
    pub fn lock_table(&self, table: Oid, table_name: &str, mode: locks::TableLockMode, nowait: bool) -> SqlResult<()> {
        if nowait && self.db.locks.table_conflict(self.id, table, mode) {
            return Err(SqlError::new(code::LOCK_NOT_AVAILABLE, format!("could not obtain lock on relation \"{table_name}\"")));
        }
        self.db.locks.grant_table(self.id, table, mode);
        Ok(())
    }

    /// Whether the row version this transaction sees is still the latest committed one (or
    /// one this transaction wrote).
    fn row_is_current(&self, table: Oid, id: RowId) -> bool {
        if self.changes.get(&table).is_some_and(|c| c.inserted.contains_key(&id)) {
            return true;
        }
        self.db.current().tables.get(&table).is_some_and(|d| d.rows.contains_key(&id))
    }
}

/// What a lock request does when another transaction holds a conflicting lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockWait {
    /// Postgres would wait; pgmem proceeds (the conflict surfaces at commit).
    Block,
    NoWait,
    SkipLocked,
}

impl Drop for Txn {
    fn drop(&mut self) {
        // An unsettled copy's sequence undo runs first: it holds the later states.
        drop(self.side.get_mut().take());
        if self.forked {
            return;
        }
        self.db.locks.release_all(self.id);
        if self.isolation == Isolation::Serializable {
            let mut ssi = lock(&self.db.ssi);
            ssi.active.remove(&self.id);
            // Records no in-flight serializable transaction can conflict with are done.
            match ssi.active.values().min().copied() {
                Some(oldest) => ssi.committed.retain(|r| r.seq > oldest),
                None => ssi.committed.clear(),
            }
        }
    }
}

fn scan_with(base: &DbState, changes: &HashMap<Oid, TableChanges>, table: Oid) -> Vec<(RowId, Row)> {
    let changes = changes.get(&table);
    let mut out = Vec::new();
    let truncated = changes.is_some_and(|c| c.truncated);
    if !truncated && let Some(data) = base.tables.get(&table) {
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
