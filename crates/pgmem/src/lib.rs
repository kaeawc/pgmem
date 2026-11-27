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
//!
//! # Embedding
//!
//! [`Embedded`] runs the same database in-process, without the wire protocol. With
//! `default-features = false` (no `server` feature) pgmem builds without tokio and offers
//! only this API.
//!
//! ```no_run
//! let db = pgmem::Embedded::new();
//! let mut session = db.session();
//! for result in session.execute_batch("CREATE TABLE t (id int); INSERT INTO t VALUES (1)") {
//!     result.expect("statement");
//! }
//! let out = session.query("SELECT id, 'x' AS label FROM t").expect("query");
//! assert_eq!(out.rows, vec![vec![Some("1".to_owned()), Some("x".to_owned())]]);
//!
//! // An independent copy of the committed state, for trying changes against one base.
//! let copy = db.fork();
//! ```

mod embedded;
#[cfg(feature = "server")]
mod server;

pub use embedded::{Column, DbError, Embedded, EmbeddedSession, Notice, QueryOutcome};
#[cfg(feature = "server")]
pub use server::{Builder, Server};

use pgmem_types::SqlError;
use std::time::{SystemTime, UNIX_EPOCH};

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

impl From<DbError> for Error {
    fn from(e: DbError) -> Self {
        Error(e.to_string())
    }
}

/// Renders a value in its Postgres text output form (what a client receives in a text-format
/// DataRow), with the default session settings (`TimeZone` UTC, `DateStyle` ISO). `None` for
/// SQL NULL. The type is the value's own (an `int4` value renders as `int4`, a timestamp
/// without time zone as `timestamp`...).
pub fn to_pg_text(value: &SqlValue) -> Result<Option<String>, DbError> {
    if value.is_null() {
        return Ok(None);
    }
    Ok(Some(pgmem_types::text::encode(value, &value.natural_type())?))
}

pub(crate) fn to_pg_micros(t: SystemTime) -> i64 {
    let unix = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_micros()).unwrap_or(i64::MAX),
    };
    unix - pgmem_types::datetime::PG_EPOCH_UNIX_SECS * 1_000_000
}
