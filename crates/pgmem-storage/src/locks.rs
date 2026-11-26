//! Row locks (`SELECT ... FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE`, and the locks
//! UPDATE and DELETE take) and relation locks (`LOCK TABLE`, and the locks statements take
//! implicitly), shared by every session and owned by transactions. pgmem never waits for a lock: NOWAIT fails and SKIP LOCKED
//! skips; without them a conflicting lock is granted anyway, and the write conflict surfaces
//! at commit as a serialization failure.

use pgmem_catalog::Oid;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::RowId;

/// Row lock strengths, weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowLockMode {
    KeyShare = 0,
    Share = 1,
    NoKeyUpdate = 2,
    Update = 3,
}

impl RowLockMode {
    /// Postgres' row-level lock conflict table: two modes conflict when their strengths add
    /// up to at least NO KEY UPDATE + SHARE.
    fn conflicts(self, other: RowLockMode) -> bool {
        self as u8 + other as u8 >= 3
    }
}

/// Table lock modes, `ACCESS SHARE` (1) to `ACCESS EXCLUSIVE` (8).
pub type TableLockMode = u8;

/// Postgres' table-level lock conflict table.
fn table_conflicts(a: TableLockMode, b: TableLockMode) -> bool {
    const CONFLICTS: [u8; 9] = [0, 0b1000_0000, 0b1100_0000, 0b1111_0000, 0b1111_1000, 0b1110_1100, 0b1111_1100, 0b1111_1110, 0b1111_1111];
    // Bit (mode - 1) of row `a` is set when `a` conflicts with `mode`.
    a <= 8 && (1..=8).contains(&b) && CONFLICTS[usize::from(a)] & (1 << (b - 1)) != 0
}

/// Postgres' name for a table lock mode, as `pg_locks.mode` shows it.
pub fn table_mode_name(mode: TableLockMode) -> &'static str {
    match mode {
        1 => "AccessShareLock",
        2 => "RowShareLock",
        3 => "RowExclusiveLock",
        4 => "ShareUpdateExclusiveLock",
        5 => "ShareLock",
        6 => "ShareRowExclusiveLock",
        7 => "ExclusiveLock",
        _ => "AccessExclusiveLock",
    }
}

/// Row lock holders by (table, row): (owning transaction, mode).
type RowHolders = HashMap<(Oid, RowId), Vec<(u64, RowLockMode)>>;

/// One granted relation lock, as `pg_locks` lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelationLock {
    pub relation: Oid,
    pub pid: i32,
    pub mode: TableLockMode,
}

#[derive(Default)]
struct TableLocks {
    /// Holders by relation: (owning transaction, backend pid, mode).
    by_relation: HashMap<Oid, Vec<(u64, i32, TableLockMode)>>,
    /// Each transaction's relation locks in the order taken (ROLLBACK TO releases the later
    /// ones); one entry per (relation, mode), as Postgres counts a re-acquired lock once.
    by_owner: HashMap<u64, Vec<(Oid, TableLockMode)>>,
}

#[derive(Default)]
pub struct LockTable {
    rows: Mutex<RowHolders>,
    tables: Mutex<TableLocks>,
}

impl std::fmt::Debug for LockTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockTable").finish_non_exhaustive()
    }
}

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl LockTable {
    /// Whether another transaction holds a conflicting lock on the row.
    pub fn row_conflict(&self, owner: u64, table: Oid, id: RowId, mode: RowLockMode) -> bool {
        guard(&self.rows).get(&(table, id)).is_some_and(|hs| hs.iter().any(|(o, m)| *o != owner && m.conflicts(mode)))
    }

    pub fn grant_row(&self, owner: u64, table: Oid, id: RowId, mode: RowLockMode) {
        guard(&self.rows).entry((table, id)).or_default().push((owner, mode));
    }

    pub fn table_conflict(&self, owner: u64, table: Oid, mode: TableLockMode) -> bool {
        guard(&self.tables).by_relation.get(&table).is_some_and(|hs| hs.iter().any(|(o, _, m)| *o != owner && table_conflicts(*m, mode)))
    }

    /// Grants `owner` (a transaction of backend `pid`) a relation lock, once per mode.
    pub fn grant_table(&self, owner: u64, pid: i32, table: Oid, mode: TableLockMode) {
        let mut t = guard(&self.tables);
        let held = t.by_owner.entry(owner).or_default();
        if held.contains(&(table, mode)) {
            return;
        }
        held.push((table, mode));
        t.by_relation.entry(table).or_default().push((owner, pid, mode));
    }

    /// How many relation locks `owner` holds (a savepoint's mark).
    pub fn table_lock_count(&self, owner: u64) -> usize {
        guard(&self.tables).by_owner.get(&owner).map_or(0, Vec::len)
    }

    /// Releases the relation locks `owner` took after it held `keep` of them.
    pub fn release_tables_from(&self, owner: u64, keep: usize) {
        let mut t = guard(&self.tables);
        let released: Vec<_> = t.by_owner.get_mut(&owner).map(|h| h.drain(keep.min(h.len())..).collect()).unwrap_or_default();
        for (table, mode) in released {
            if let Some(hs) = t.by_relation.get_mut(&table) {
                hs.retain(|(o, _, m)| !(*o == owner && *m == mode));
                if hs.is_empty() {
                    t.by_relation.remove(&table);
                }
            }
        }
    }

    /// Every granted relation lock, ordered by relation, pid and mode.
    pub fn relation_locks(&self) -> Vec<RelationLock> {
        let t = guard(&self.tables);
        let mut out: Vec<RelationLock> = t
            .by_relation
            .iter()
            .flat_map(|(rel, hs)| hs.iter().map(|(_, pid, mode)| RelationLock { relation: *rel, pid: *pid, mode: *mode }))
            .collect();
        out.sort_by_key(|l| (l.relation, l.pid, l.mode));
        out
    }

    /// Releases the row locks `owner` took (one entry per grant).
    pub fn release_rows(&self, owner: u64, held: &[(Oid, RowId, RowLockMode)]) {
        let mut rows = guard(&self.rows);
        for (t, id, mode) in held {
            if let Some(hs) = rows.get_mut(&(*t, *id)) {
                if let Some(i) = hs.iter().position(|(o, m)| *o == owner && m == mode) {
                    hs.remove(i);
                }
                if hs.is_empty() {
                    rows.remove(&(*t, *id));
                }
            }
        }
    }

    /// Releases every lock `owner` holds (transaction end).
    pub fn release_all(&self, owner: u64) {
        guard(&self.rows).retain(|_, hs| {
            hs.retain(|(o, _)| *o != owner);
            !hs.is_empty()
        });
        let mut t = guard(&self.tables);
        t.by_owner.remove(&owner);
        t.by_relation.retain(|_, hs| {
            hs.retain(|(o, _, _)| *o != owner);
            !hs.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_conflicts_follow_postgres() {
        use RowLockMode::*;
        assert!(!KeyShare.conflicts(NoKeyUpdate));
        assert!(KeyShare.conflicts(Update));
        assert!(!Share.conflicts(Share));
        assert!(Share.conflicts(NoKeyUpdate));
        assert!(NoKeyUpdate.conflicts(NoKeyUpdate));
    }

    #[test]
    fn table_conflicts_follow_postgres() {
        // ACCESS SHARE only conflicts with ACCESS EXCLUSIVE.
        assert!(!table_conflicts(1, 7));
        assert!(table_conflicts(1, 8));
        // SHARE ROW EXCLUSIVE conflicts with ROW EXCLUSIVE, not with ROW SHARE.
        assert!(table_conflicts(6, 3));
        assert!(!table_conflicts(6, 2));
        assert!(!table_conflicts(5, 5));
        assert!(table_conflicts(5, 3));
    }

    #[test]
    fn relation_locks_are_counted_once_and_released_from_a_mark() {
        let t = LockTable::default();
        t.grant_table(1, 10, 100, 5);
        t.grant_table(1, 10, 100, 5);
        let mark = t.table_lock_count(1);
        assert_eq!(mark, 1);
        t.grant_table(1, 10, 100, 8);
        t.grant_table(1, 10, 101, 1);
        assert!(t.table_conflict(2, 100, 1));
        t.release_tables_from(1, mark);
        assert_eq!(t.relation_locks(), vec![RelationLock { relation: 100, pid: 10, mode: 5 }]);
        assert!(!t.table_conflict(2, 100, 1));
        t.release_all(1);
        assert!(t.relation_locks().is_empty());
    }
}
