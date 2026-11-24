//! pgmem: an in-memory, Postgres-wire-compatible database for tests. Fidelity over
//! performance.
//!
//! ```no_run
//! let server = pgmem::Server::start().expect("start pgmem");
//! // Connect any Postgres client to server.dsn(): tokio-postgres, sqlx, pgx, JDBC, psql...
//! println!("{}", server.dsn());
//! ```
//!
//! Each [`Server`] is an isolated database listening on a free localhost port. It is stopped
//! when dropped.

use pgmem_exec::ServerClock;
use pgmem_storage::Database;
use pgmem_types::{SqlError, Value};
use pgmem_wire::Shared;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use pgmem_types::Value as SqlValue;

/// Error starting or stopping a server.
#[derive(Debug)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error(e.to_string())
    }
}

impl From<SqlError> for Error {
    fn from(e: SqlError) -> Self {
        Error(e.to_string())
    }
}

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
        let host = self.host.unwrap_or_else(|| "127.0.0.1".into());
        let listener = TcpListener::bind((host.as_str(), self.port))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let db = Arc::new(Database::new());
        let clock = Arc::new(ServerClock::default());
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

fn to_pg_micros(t: SystemTime) -> i64 {
    let unix = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_micros()).unwrap_or(i64::MAX),
    };
    unix - pgmem_types::datetime::PG_EPOCH_UNIX_SECS * 1_000_000
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
