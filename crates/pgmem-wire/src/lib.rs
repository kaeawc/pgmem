//! The Postgres frontend/backend protocol (version 3.0) for pgmem: startup with trust
//! authentication, the simple and extended query protocols, and Postgres-shaped errors and
//! notices. One task per connection.

mod codec;
mod conn;

use pgmem_exec::{Clock, ServerClock};
use pgmem_storage::Database;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// State shared by every connection to one server.
#[derive(Debug)]
pub struct Shared {
    pub db: Arc<Database>,
    pub clock: Arc<ServerClock>,
}

impl Shared {
    pub fn new(db: Arc<Database>, clock: Arc<ServerClock>) -> Shared {
        Shared { db, clock }
    }

    fn next_pid(&self) -> i32 {
        self.db.next_backend_pid()
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock) as Arc<dyn Clock>
    }
}

/// Accepts connections until `shutdown` turns true.
pub async fn serve(listener: TcpListener, shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let shared = Arc::clone(&shared);
                tasks.spawn(async move {
                    // Connection errors (client went away, malformed frames) end that
                    // connection only.
                    let _ = conn::handle(stream, shared).await;
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}
