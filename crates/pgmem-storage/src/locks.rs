//! Row locks (`SELECT ... FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE`, and the locks
//! UPDATE and DELETE take) and explicit table locks (`LOCK TABLE`), shared by every session
//! and owned by transactions. pgmem never waits for a lock: NOWAIT fails and SKIP LOCKED
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

/// Row lock holders by (table, row): (owning transaction, mode).
type RowHolders = HashMap<(Oid, RowId), Vec<(u64, RowLockMode)>>;

#[derive(Default)]
pub struct LockTable {
    rows: Mutex<RowHolders>,
    tables: Mutex<HashMap<Oid, Vec<(u64, TableLockMode)>>>,
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
        guard(&self.tables).get(&table).is_some_and(|hs| hs.iter().any(|(o, m)| *o != owner && table_conflicts(*m, mode)))
    }

    pub fn grant_table(&self, owner: u64, table: Oid, mode: TableLockMode) {
        guard(&self.tables).entry(table).or_default().push((owner, mode));
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
        guard(&self.tables).retain(|_, hs| {
            hs.retain(|(o, _)| *o != owner);
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
}
