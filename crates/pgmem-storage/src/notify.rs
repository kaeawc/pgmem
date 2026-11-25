//! LISTEN / NOTIFY: the server-wide queue (Postgres' `async.c`). Sessions register the
//! channels they listen on; a committing transaction's notifications are queued for every
//! listening session (the sender included), which delivers them when it is not inside a
//! transaction block.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// One delivered notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub sender_pid: i32,
    pub channel: String,
    pub payload: String,
}

/// Called when a session's queue becomes non-empty (the wire layer wakes the connection).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

struct Listener {
    channels: BTreeSet<String>,
    queue: VecDeque<Notification>,
    waker: Option<Waker>,
}

#[derive(Default)]
pub struct NotifyHub {
    listeners: Mutex<HashMap<i32, Listener>>,
}

impl std::fmt::Debug for NotifyHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyHub").finish_non_exhaustive()
    }
}

impl NotifyHub {
    fn lock(&self) -> MutexGuard<'_, HashMap<i32, Listener>> {
        self.listeners.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the function called when notifications arrive for `pid`.
    pub fn set_waker(&self, pid: i32, waker: Waker) {
        let mut l = self.lock();
        let entry = l.entry(pid).or_insert_with(|| Listener { channels: BTreeSet::new(), queue: VecDeque::new(), waker: None });
        entry.waker = Some(waker);
    }

    /// Replaces the channels `pid` listens on (at commit of its LISTEN / UNLISTEN).
    pub fn set_channels(&self, pid: i32, channels: &BTreeSet<String>) {
        let mut l = self.lock();
        let entry = l.entry(pid).or_insert_with(|| Listener { channels: BTreeSet::new(), queue: VecDeque::new(), waker: None });
        channels.clone_into(&mut entry.channels);
    }

    /// Queues a committed transaction's notifications for every session listening.
    pub fn publish(&self, sender_pid: i32, notifications: &[(String, String)]) {
        let mut wake = Vec::new();
        {
            let mut l = self.lock();
            for listener in l.values_mut() {
                let before = listener.queue.len();
                for (channel, payload) in notifications {
                    if listener.channels.contains(channel) {
                        listener.queue.push_back(Notification { sender_pid, channel: channel.clone(), payload: payload.clone() });
                    }
                }
                if listener.queue.len() > before
                    && let Some(w) = &listener.waker
                {
                    wake.push(Arc::clone(w));
                }
            }
        }
        for w in wake {
            w();
        }
    }

    /// Takes the notifications queued for `pid`.
    pub fn take(&self, pid: i32) -> Vec<Notification> {
        self.lock().get_mut(&pid).map(|l| l.queue.drain(..).collect()).unwrap_or_default()
    }

    /// Forgets a session (at disconnect).
    pub fn remove(&self, pid: i32) {
        self.lock().remove(&pid);
    }
}
