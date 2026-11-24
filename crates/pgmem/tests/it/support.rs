//! Test harness: a fresh database per test, on pgmem or (with `PGMEM_TEST_DSN`) real Postgres.

use postgres::{Client, NoTls};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct TestDb {
    server: Option<pgmem::Server>,
    /// Real Postgres: admin connection string and the per-test database name.
    real: Option<(String, String)>,
    conninfo: String,
}

impl TestDb {
    pub fn new() -> TestDb {
        match std::env::var("PGMEM_TEST_DSN") {
            Ok(admin) if !admin.is_empty() => {
                let name = format!("pgmem_test_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
                let mut c = Client::connect(&admin, NoTls).expect("connect to PGMEM_TEST_DSN");
                c.batch_execute(&format!("CREATE DATABASE {name}")).expect("create test database");
                let conninfo = format!("{admin} dbname={name}");
                TestDb { server: None, real: Some((admin, name)), conninfo }
            }
            _ => {
                let server = pgmem::Server::start().expect("start pgmem");
                let conninfo = server.conninfo();
                TestDb { server: Some(server), real: None, conninfo }
            }
        }
    }

    pub fn connect(&self) -> Client {
        Client::connect(&self.conninfo, NoTls).expect("connect")
    }

    /// The pgmem server, or None when running against real Postgres.
    pub fn server(&self) -> Option<&pgmem::Server> {
        self.server.as_ref()
    }

    pub fn is_real_postgres(&self) -> bool {
        self.real.is_some()
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if let Some((admin, name)) = &self.real
            && let Ok(mut c) = Client::connect(admin, NoTls)
        {
            let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
        }
    }
}

/// A fresh database and a connected client.
pub fn setup() -> (TestDb, Client) {
    let db = TestDb::new();
    let client = db.connect();
    (db, client)
}

/// The SQLSTATE of a database error.
pub fn sqlstate(e: &postgres::Error) -> String {
    e.code().map(|c| c.code().to_owned()).unwrap_or_else(|| format!("<no sqlstate: {e}>"))
}

/// The server's message of a database error.
pub fn message(e: &postgres::Error) -> String {
    e.as_db_error().map(|d| d.message().to_owned()).unwrap_or_else(|| e.to_string())
}

/// Asserts a statement fails with the given SQLSTATE.
#[track_caller]
pub fn expect_code<T: std::fmt::Debug>(r: Result<T, postgres::Error>, code: &str) -> postgres::Error {
    match r {
        Ok(v) => panic!("expected SQLSTATE {code}, got success: {v:?}"),
        Err(e) => {
            assert_eq!(sqlstate(&e), code, "unexpected error: {e:?}");
            e
        }
    }
}

/// Any column value as its Postgres text form (decoded from the binary wire format), e.g.
/// numeric `1.50`, interval `1 day 02:00:00`, `timestamptz` `2024-01-01 00:00:00+00`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text(pub String);

impl<'a> postgres::types::FromSql<'a> for Text {
    fn from_sql(ty: &postgres::types::Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        let t = pgmem_types::Type::from_oid(ty.oid()).ok_or_else(|| format!("unsupported type {ty}"))?;
        let v = pgmem_types::decode(raw, &t, pgmem_types::Format::Binary).map_err(|e| e.to_string())?;
        Ok(Text(pgmem_types::text::encode(&v, &t).map_err(|e| e.to_string())?))
    }

    fn accepts(ty: &postgres::types::Type) -> bool {
        pgmem_types::Type::from_oid(ty.oid()).is_some()
    }
}

impl PartialEq<&str> for Text {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// A Postgres `interval` as its three components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    /// Total microseconds assuming 30-day months and 24-hour days (Go's time.Duration view).
    pub fn total_micros(&self) -> i64 {
        (i64::from(self.months) * 30 + i64::from(self.days)) * 86_400_000_000 + self.micros
    }
}

impl<'a> postgres::types::FromSql<'a> for Interval {
    fn from_sql(_: &postgres::types::Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        if raw.len() != 16 {
            return Err("bad interval".into());
        }
        let micros = i64::from_be_bytes(raw[0..8].try_into()?);
        let days = i32::from_be_bytes(raw[8..12].try_into()?);
        let months = i32::from_be_bytes(raw[12..16].try_into()?);
        Ok(Interval { months, days, micros })
    }

    fn accepts(ty: &postgres::types::Type) -> bool {
        *ty == postgres::types::Type::INTERVAL
    }
}

/// Runs a query over the simple (text) protocol and returns every row as text values.
pub fn texts(c: &mut Client, sql: &str) -> Vec<Vec<Option<String>>> {
    let msgs = c.simple_query(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    msgs.into_iter()
        .filter_map(|m| match m {
            postgres::SimpleQueryMessage::Row(r) => Some((0..r.len()).map(|i| r.get(i).map(str::to_owned)).collect()),
            _ => None,
        })
        .collect()
}

/// The single text value of a one-row, one-column query (simple protocol).
pub fn text_value(c: &mut Client, sql: &str) -> Option<String> {
    let rows = texts(c, sql);
    assert_eq!(rows.len(), 1, "{sql}: expected one row, got {rows:?}");
    rows[0][0].clone()
}

/// The UTC instant `y-mo-d h:mi:s`.
pub fn utc_ts(y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8) -> jiff::Timestamp {
    jiff::civil::date(y, mo, d).at(h, mi, s, 0).to_zoned(jiff::tz::TimeZone::UTC).expect("valid timestamp").timestamp()
}
