//! Advisory locks (`pg_advisory_lock` and friends): one lock table shared by every session.
//! Locks are re-entrant per session and either session-level (held until unlocked or the
//! session ends) or transaction-level (released when the transaction ends). A session never
//! conflicts with itself; exclusive conflicts with anything another session holds, shared
//! only with another session's exclusive.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// `pg_locks`' identification of an advisory lock: a bigint key is (high, low, 1), a pair of
/// int4 keys is (first, second, 2).
pub type AdvisoryKey = (u32, u32, u16);

#[derive(Clone, Debug)]
struct Holder {
    pid: i32,
    shared: bool,
    xact: bool,
    count: u32,
}

/// One held lock, as `pg_locks` lists it.
#[derive(Clone, Debug)]
pub struct HeldLock {
    pub key: AdvisoryKey,
    pub pid: i32,
    pub shared: bool,
}

#[derive(Default)]
pub struct AdvisoryLocks {
    table: Mutex<HashMap<AdvisoryKey, Vec<Holder>>>,
    released: Condvar,
}

impl std::fmt::Debug for AdvisoryLocks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvisoryLocks").finish_non_exhaustive()
    }
}

fn conflicts(holders: &[Holder], pid: i32, shared: bool) -> bool {
    holders.iter().any(|h| h.pid != pid && (!shared || !h.shared))
}

impl AdvisoryLocks {
    fn lock_table(&self) -> MutexGuard<'_, HashMap<AdvisoryKey, Vec<Holder>>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn grant(holders: &mut Vec<Holder>, pid: i32, shared: bool, xact: bool) {
        match holders.iter_mut().find(|h| h.pid == pid && h.shared == shared && h.xact == xact) {
            Some(h) => h.count += 1,
            None => holders.push(Holder { pid, shared, xact, count: 1 }),
        }
    }

    /// `pg_try_advisory_*lock*`: takes the lock if no other session's lock conflicts.
    pub fn try_lock(&self, key: AdvisoryKey, pid: i32, shared: bool, xact: bool) -> bool {
        let mut t = self.lock_table();
        let holders = t.entry(key).or_default();
        if conflicts(holders, pid, shared) {
            return false;
        }
        Self::grant(holders, pid, shared, xact);
        true
    }

    /// `pg_advisory_*lock*`: waits until the lock can be taken (`timeout`: `lock_timeout`).
    /// Returns false when the wait timed out.
    pub fn lock(&self, key: AdvisoryKey, pid: i32, shared: bool, xact: bool, timeout: Option<Duration>) -> bool {
        let deadline = timeout.map(|d| Instant::now() + d);
        let mut t = self.lock_table();
        loop {
            let holders = t.entry(key).or_default();
            if !conflicts(holders, pid, shared) {
                Self::grant(holders, pid, shared, xact);
                return true;
            }
            t = match deadline {
                None => self.released.wait(t).unwrap_or_else(PoisonError::into_inner),
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return false;
                    }
                    self.released.wait_timeout(t, d - now).unwrap_or_else(PoisonError::into_inner).0
                }
            };
        }
    }

    /// `pg_advisory_unlock[_shared]`: releases one session-level hold; false if none.
    pub fn unlock(&self, key: AdvisoryKey, pid: i32, shared: bool) -> bool {
        let mut t = self.lock_table();
        let Some(holders) = t.get_mut(&key) else { return false };
        let Some(i) = holders.iter().position(|h| h.pid == pid && h.shared == shared && !h.xact) else { return false };
        holders[i].count -= 1;
        if holders[i].count == 0 {
            holders.remove(i);
        }
        if holders.is_empty() {
            t.remove(&key);
        }
        drop(t);
        self.released.notify_all();
        true
    }

    /// Releases `pid`'s session-level locks (`pg_advisory_unlock_all`, session end) or its
    /// transaction-level ones (transaction end).
    pub fn release(&self, pid: i32, xact: bool) {
        let mut t = self.lock_table();
        for holders in t.values_mut() {
            holders.retain(|h| h.pid != pid || h.xact != xact);
        }
        t.retain(|_, h| !h.is_empty());
        drop(t);
        self.released.notify_all();
    }

    pub fn held(&self) -> Vec<HeldLock> {
        let t = self.lock_table();
        let mut out: Vec<HeldLock> =
            t.iter().flat_map(|(key, hs)| hs.iter().map(|h| HeldLock { key: *key, pid: h.pid, shared: h.shared })).collect();
        out.sort_by_key(|l| (l.key, l.pid, l.shared));
        out.dedup_by_key(|l| (l.key, l.pid, l.shared));
        out
    }
}
