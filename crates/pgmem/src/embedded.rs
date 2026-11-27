//! The in-process API: a database and sessions on it, without the wire protocol or tokio.

use crate::to_pg_micros;
use pgmem_exec::{Clock, OutCol, QueryResult, ServerClock, Session};
use pgmem_storage::Database;
use pgmem_types::{ErrorFields, SqlError, Type, Value};
use std::sync::Arc;
use std::time::SystemTime;

/// The user and database name of embedded sessions (as in [`Server`](crate::Server)'s DSN).
const SESSION_USER: &str = "pgmem";

/// An in-process pgmem database. Cloning the handle shares the database; [`Embedded::fork`]
/// copies it.
///
/// Sessions ([`Embedded::session`]) behave like wire connections: each has its own settings,
/// transaction state and temporary objects, and they run concurrently (an
/// [`EmbeddedSession`] is `Send`, so each thread can own one).
#[derive(Clone, Debug)]
pub struct Embedded {
    db: Arc<Database>,
    clock: Arc<ServerClock>,
}

impl Default for Embedded {
    fn default() -> Self {
        Embedded::new()
    }
}

impl Embedded {
    /// A new, empty database (the state of a fresh `CREATE DATABASE`).
    pub fn new() -> Embedded {
        Embedded::from_parts(Arc::new(Database::new()), Arc::new(ServerClock::default()))
    }

    pub(crate) fn from_parts(db: Arc<Database>, clock: Arc<ServerClock>) -> Embedded {
        Embedded { db, clock }
    }

    #[cfg_attr(not(feature = "server"), allow(dead_code))]
    pub(crate) fn parts(&self) -> (Arc<Database>, Arc<ServerClock>) {
        (Arc::clone(&self.db), Arc::clone(&self.clock))
    }

    /// An independent copy of the committed state: schemas, tables and their rows, views,
    /// types, functions, roles and grants, and sequences' current values. Cheap: the committed
    /// state is immutable and shared until either side writes (copy-on-write per table).
    /// Uncommitted work of open transactions is not copied, nor is session state. A pinned
    /// `now()` ([`Embedded::set_now`]) is copied. Afterwards writes to one database are never
    /// visible in the other.
    pub fn fork(&self) -> Embedded {
        let clock = ServerClock::default();
        clock.pin(self.clock.pinned());
        Embedded::from_parts(Arc::new(self.db.fork()), Arc::new(clock))
    }

    /// A new session, like a new connection as user `pgmem` to database `pgmem`. It starts
    /// with the settings of `ALTER ROLE` / `ALTER DATABASE ... SET` (a stored setting that is
    /// no longer valid is skipped).
    pub fn session(&self) -> EmbeddedSession {
        let mut session = self.new_session();
        let _ = session.apply_startup(&[]);
        EmbeddedSession { session }
    }

    /// A new session with startup packet parameters, as a client sends them: `options`
    /// (`-c name=value`, `--name=value`) and settings by name (`search_path`,
    /// `application_name`, ...). They override `ALTER ROLE` / `ALTER DATABASE ... SET` and are
    /// the session defaults RESET restores. An unknown parameter or invalid value is an
    /// error, as it fails a connection.
    pub fn session_with_params(&self, params: &[(&str, &str)]) -> Result<EmbeddedSession, DbError> {
        let mut session = self.new_session();
        let params: Vec<(String, String)> = params.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        session.apply_startup(&params)?;
        Ok(EmbeddedSession { session })
    }

    fn new_session(&self) -> Session {
        let pid = self.db.next_backend_pid();
        let clock = Arc::clone(&self.clock) as Arc<dyn Clock>;
        Session::new(Arc::clone(&self.db), clock, SESSION_USER, SESSION_USER, pid)
    }

    /// Pins the time `now()` / `CURRENT_TIMESTAMP` report (`None` restores the wall clock).
    /// Like Postgres, `now()` is the start time of the current transaction.
    pub fn set_now(&self, t: Option<SystemTime>) {
        self.clock.pin(t.map(to_pg_micros));
    }

    /// Appends rows to an existing table without going through SQL. Each row has one value
    /// per column, in column order.
    pub fn seed(&self, table: &str, rows: Vec<Vec<Value>>) -> Result<(), DbError> {
        Ok(self.db.seed(table, rows)?)
    }
}

/// One session on an [`Embedded`] database. Dropping it ends the session: an open
/// transaction is rolled back and temporary objects are dropped, as on disconnect.
pub struct EmbeddedSession {
    session: Session,
}

impl std::fmt::Debug for EmbeddedSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedSession").field("status", &self.transaction_status()).finish_non_exhaustive()
    }
}

impl EmbeddedSession {
    /// Runs a string of `;`-separated statements with the semantics of the wire protocol's
    /// simple Query message (what `psql -c` and `batch_execute` send):
    ///
    /// - outside an explicit transaction, several statements run in one implicit transaction
    ///   block, committed at the end, or rolled back if any statement fails;
    /// - execution stops at the first error, so the result has one entry per statement run,
    ///   the last one an `Err` when something failed;
    /// - `BEGIN` / `COMMIT` / `ROLLBACK` inside the string end or open blocks as in Postgres,
    ///   and a failed explicit transaction stays failed (`25P02`) until `ROLLBACK`.
    ///
    /// A syntax error rejects the whole string before anything runs (one `Err`). An error
    /// committing the implicit block is appended as a final `Err`. `COPY ... FROM STDIN` fails,
    /// as when a client sends CopyFail; `COPY ... TO STDOUT` returns its data in
    /// [`QueryOutcome::copy_data`].
    pub fn execute_batch(&mut self, sql: &str) -> Vec<Result<QueryOutcome, DbError>> {
        let stmts = match pgmem_parse::parse(sql) {
            Ok(s) => s,
            Err(e) => return vec![Err(e.into())],
        };
        self.session.begin_implicit_block(stmts.len());
        let mut results = Vec::with_capacity(stmts.len());
        let mut failed = false;
        for stmt in &stmts {
            let outcome = self.session.execute(stmt, &[], &[]).and_then(|r| self.finish(r));
            failed = outcome.is_err();
            results.push(outcome.map_err(DbError::from));
            if failed {
                break;
            }
        }
        if let Err(e) = self.session.end_implicit_block(failed) {
            results.push(Err(e.into()));
        }
        results
    }

    /// Runs `sql` like [`execute_batch`](Self::execute_batch) and returns the last statement's
    /// outcome (its rows, for a query), or the first error.
    pub fn query(&mut self, sql: &str) -> Result<QueryOutcome, DbError> {
        let mut last = None;
        for r in self.execute_batch(sql) {
            last = Some(r?);
        }
        Ok(last.unwrap_or_default())
    }

    /// The transaction status the server would report in ReadyForQuery: `'I'` idle, `'T'` in
    /// a transaction block, `'E'` in a failed transaction block.
    pub fn transaction_status(&self) -> char {
        char::from(self.session.status().byte())
    }

    /// The current value of a setting (`SHOW name`), if it is set.
    pub fn setting(&self, name: &str) -> Option<&str> {
        self.session.settings.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// Turns an executed statement's result into its client-visible outcome.
    fn finish(&mut self, mut r: QueryResult) -> Result<QueryOutcome, SqlError> {
        if let Some(pending) = r.copy_in.take() {
            // No CopyData can follow: end the COPY the way a client's CopyFail does.
            let reason = "COPY FROM STDIN is not supported by the embedded API".to_owned();
            r = self.session.copy_from(&pending, Err(reason))?;
        }
        // Output depends on the session's TimeZone, DateStyle and user types.
        let _ctx = pgmem_types::context::enter(self.session.format_context());
        let columns = if r.returns_rows { r.columns.iter().map(Column::describe).collect() } else { Vec::new() };
        let mut rows = Vec::with_capacity(r.rows.len());
        for row in &r.rows {
            rows.push(text_row(&r.columns, row)?);
        }
        let notices = r
            .notices
            .iter()
            .map(|n| Notice { severity: n.severity.to_owned(), code: n.code.to_owned(), message: n.message.clone() })
            .collect();
        let copy_data = r.copy_out.map(|c| c.data.concat());
        Ok(QueryOutcome { tag: r.tag, empty: r.empty, columns, rows, notices, copy_data })
    }
}

/// One row in text format, as a DataRow would carry it.
fn text_row(cols: &[OutCol], row: &[Value]) -> Result<Vec<Option<String>>, SqlError> {
    row.iter()
        .enumerate()
        .map(|(i, v)| {
            if v.is_null() {
                return Ok(None);
            }
            let ty = cols.get(i).map_or_else(|| v.natural_type(), |c| c.ty.clone());
            pgmem_types::text::encode(v, &ty).map(Some)
        })
        .collect()
}

/// The client-visible outcome of one statement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueryOutcome {
    /// The CommandComplete tag (`SELECT 2`, `INSERT 0 1`, `CREATE TABLE`...). Empty for an
    /// empty statement.
    pub tag: String,
    /// The statement was empty (EmptyQueryResponse).
    pub empty: bool,
    /// Result columns (RowDescription); empty for statements that return no rows.
    pub columns: Vec<Column>,
    /// Rows in Postgres text output format; `None` is SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// Notices and warnings the statement raised, in order.
    pub notices: Vec<Notice>,
    /// The data `COPY ... TO STDOUT` produced.
    pub copy_data: Option<Vec<u8>>,
}

/// A result column, as RowDescription describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Column {
    pub name: String,
    /// The type OID (`pg_type.oid`). A domain column is described by its base type.
    pub type_oid: u32,
    /// The type's name as `format_type(oid, NULL)` prints it (`integer`, `text[]`...).
    pub type_name: String,
    /// The type modifier (`-1` for none), e.g. `numeric(10,2)`'s.
    pub typmod: i32,
    /// The source table's OID and the column's attribute number, when the column is a plain
    /// table column (else 0).
    pub table_oid: u32,
    pub attnum: i16,
}

impl Column {
    /// Must run inside the session's format context (user types).
    fn describe(c: &OutCol) -> Column {
        // Like printtup, a domain column is described by its base type and typmod.
        let (ty, typmod) = match &c.ty {
            Type::Domain(oid) => {
                let (base, dmod) = pgmem_types::usertype::domain_base(*oid).unwrap_or((Type::Unknown, -1));
                (base, if c.typmod >= 0 { c.typmod } else { dmod })
            }
            other => (other.clone(), c.typmod),
        };
        Column { name: c.name.clone(), type_oid: ty.oid(), type_name: ty.name(), typmod, table_oid: c.table_oid, attnum: c.attnum }
    }
}

/// A NOTICE, WARNING or other non-error message (NoticeResponse).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Notice {
    pub severity: String,
    pub code: String,
    pub message: String,
}

/// An error as Postgres reports it (ErrorResponse): SQLSTATE, message and the optional fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError(Box<ErrorParts>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct ErrorParts {
    code: String,
    message: String,
    fields: ErrorFields,
}

impl DbError {
    /// The SQLSTATE, e.g. `23505`.
    pub fn code(&self) -> &str {
        &self.0.code
    }

    /// The primary message, as Postgres words it.
    pub fn message(&self) -> &str {
        &self.0.message
    }

    pub fn detail(&self) -> Option<&str> {
        self.0.fields.detail.as_deref()
    }

    pub fn hint(&self) -> Option<&str> {
        self.0.fields.hint.as_deref()
    }

    /// The schema of the object an integrity error is about.
    pub fn schema(&self) -> Option<&str> {
        self.0.fields.schema.as_deref()
    }

    /// The table an integrity error is about.
    pub fn table(&self) -> Option<&str> {
        self.0.fields.table.as_deref()
    }

    /// The column an integrity error is about.
    pub fn column(&self) -> Option<&str> {
        self.0.fields.column.as_deref()
    }

    /// The constraint an integrity error is about.
    pub fn constraint(&self) -> Option<&str> {
        self.0.fields.constraint.as_deref()
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ERROR: {} (SQLSTATE {})", self.0.message, self.0.code)
    }
}

impl std::error::Error for DbError {}

impl From<SqlError> for DbError {
    fn from(e: SqlError) -> Self {
        let fields = e.fields().cloned().unwrap_or_default();
        DbError(Box::new(ErrorParts { code: e.code.to_owned(), message: e.message, fields }))
    }
}
