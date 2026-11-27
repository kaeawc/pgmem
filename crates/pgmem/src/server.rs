//! The wire-protocol server (feature `server`).

use crate::{Embedded, Error, to_pg_micros};
use pgmem_exec::ServerClock;
use pgmem_storage::Database;
use pgmem_types::Value;
use pgmem_wire::Shared;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// Configures a [`Server`] before it starts.
#[derive(Debug, Default)]
pub struct Builder {
    port: u16,
    host: Option<String>,
    now: Option<SystemTime>,
}

impl Builder {
    /// Listen on a specific port (default: an ephemeral free port).
    pub fn port(mut self, port: u16) -> Builder {
        self.port = port;
        self
    }

    /// Listen on a specific address (default: 127.0.0.1).
    pub fn host(mut self, host: impl Into<String>) -> Builder {
        self.host = Some(host.into());
        self
    }

    /// Pin `now()` from the start (see [`Server::set_now`]).
    pub fn now(mut self, t: SystemTime) -> Builder {
        self.now = Some(t);
        self
    }

    pub fn start(self) -> Result<Server, Error> {
        self.start_on(&Embedded::new())
    }

    /// Starts a server on an existing in-process database: wire clients and the
    /// [`Embedded`] handle's sessions share its data and clock.
    pub fn start_on(self, database: &Embedded) -> Result<Server, Error> {
        let (db, clock) = database.parts();
        self.serve(db, clock)
    }

    fn serve(self, db: Arc<Database>, clock: Arc<ServerClock>) -> Result<Server, Error> {
        let host = self.host.unwrap_or_else(|| "127.0.0.1".into());
        let listener = TcpListener::bind((host.as_str(), self.port))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        if let Some(t) = self.now {
            clock.pin(Some(to_pg_micros(t)));
        }
        let shared = Arc::new(Shared::new(Arc::clone(&db), Arc::clone(&clock)));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let serve_shared = Arc::clone(&shared);
        let runtime =
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("pgmem").enable_io().enable_time().build()?;
        let thread = std::thread::Builder::new().name("pgmem-server".into()).spawn(move || {
            runtime.block_on(async move {
                match tokio::net::TcpListener::from_std(listener) {
                    Ok(l) => pgmem_wire::serve(l, serve_shared, rx).await,
                    Err(e) => eprintln!("pgmem: cannot listen: {e}"),
                }
            });
            runtime.shutdown_timeout(Duration::from_secs(1));
        })?;
        Ok(Server { addr, db, clock, shutdown: Some(tx), thread: Some(thread), _shared: shared })
    }
}

/// A running pgmem database. Stopped on drop.
pub struct Server {
    addr: SocketAddr,
    db: Arc<Database>,
    clock: Arc<ServerClock>,
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
    thread: Option<JoinHandle<()>>,
    _shared: Arc<Shared>,
}

impl Server {
    /// Starts a server on a free localhost port.
    pub fn start() -> Result<Server, Error> {
        Builder::default().start()
    }

    pub fn builder() -> Builder {
        Builder::default()
    }

    /// A connection string any libpq-compatible client accepts.
    pub fn dsn(&self) -> String {
        format!("postgres://pgmem@{}/pgmem?sslmode=disable", self.addr)
    }

    /// A key=value connection string (for clients like tokio-postgres' `Config` parser).
    pub fn conninfo(&self) -> String {
        format!("host={} port={} user=pgmem dbname=pgmem sslmode=disable", self.addr.ip(), self.addr.port())
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Pins the time `now()` / `CURRENT_TIMESTAMP` report (`None` restores the wall clock).
    /// Like Postgres, `now()` is the start time of the current transaction.
    pub fn set_now(&self, t: Option<SystemTime>) {
        self.clock.pin(t.map(to_pg_micros));
    }

    /// Appends rows to an existing table without going through SQL. Each row has one value
    /// per column, in column order.
    pub fn seed(&self, table: &str, rows: Vec<Vec<Value>>) -> Result<(), Error> {
        Ok(self.db.seed(table, rows)?)
    }

    /// A new server, on its own free port, whose database is an independent copy of this
    /// one's committed state (see [`Embedded::fork`]). The pinned `now()`, if any, is copied.
    pub fn fork(&self) -> Result<Server, Error> {
        let host = self.addr.ip().to_string();
        Builder::default().host(host).start_on(&self.embedded().fork())
    }

    /// An in-process handle on this server's database: its sessions see the same data as
    /// wire clients.
    pub fn embedded(&self) -> Embedded {
        Embedded::from_parts(Arc::clone(&self.db), Arc::clone(&self.clock))
    }

    /// Stops the server and waits for its connections to close.
    pub fn stop(mut self) {
        self.shutdown_now();
    }

    fn shutdown_now(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(true);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown_now();
    }
}
