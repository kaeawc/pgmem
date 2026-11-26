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

/// The value of `key` in a libpq key=value connection string.
fn conninfo_value(conninfo: &str, key: &str) -> Option<String> {
    conninfo.split_whitespace().find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=').map(str::to_owned))
}

/// What a raw connection read up to ReadyForQuery.
#[derive(Debug, Default)]
pub struct RawReply {
    /// CommandComplete tags.
    pub tags: Vec<String>,
    /// Error SQLSTATEs.
    pub errors: Vec<String>,
    /// NotificationResponse (channel, payload) pairs.
    pub notifications: Vec<(String, String)>,
    /// NoticeResponse messages (the `M` field).
    pub notices: Vec<String>,
    /// DataRow first-column values (text format).
    pub values: Vec<Option<String>>,
    /// The ReadyForQuery transaction status.
    pub status: u8,
}

/// A minimal raw frontend/backend protocol connection (trust auth only), used where the test
/// needs what the postgres crate does not expose: CommandComplete tags, several Executes
/// before one Sync, NotificationResponse ordering.
pub struct RawConn(std::net::TcpStream);

impl RawConn {
    pub fn connect(db: &TestDb, c: &mut Client) -> RawConn {
        let base = match db.server() {
            Some(server) => server.conninfo(),
            None => std::env::var("PGMEM_TEST_DSN").expect("PGMEM_TEST_DSN"),
        };
        let dbname: String = c.query_one("SELECT current_database()::text", &[]).expect("current_database").get(0);
        let host = conninfo_value(&base, "host").unwrap_or_else(|| "127.0.0.1".to_owned());
        let port = conninfo_value(&base, "port").unwrap_or_else(|| "5432".to_owned());
        let user = conninfo_value(&base, "user").unwrap_or_else(|| "postgres".to_owned());
        let mut raw = RawConn(std::net::TcpStream::connect(format!("{host}:{port}")).expect("raw connect"));
        let mut body = 196_608i32.to_be_bytes().to_vec();
        for s in ["user", &user, "database", &dbname, ""] {
            body.extend_from_slice(s.as_bytes());
            body.push(0);
        }
        raw.send(None, &body);
        raw.reply();
        raw
    }

    pub fn send(&mut self, tag: Option<u8>, body: &[u8]) {
        use std::io::Write;
        let mut msg = tag.map(|t| vec![t]).unwrap_or_default();
        msg.extend_from_slice(&i32::try_from(body.len() + 4).expect("message length").to_be_bytes());
        msg.extend_from_slice(body);
        self.0.write_all(&msg).expect("raw write");
    }

    /// Reads messages up to ReadyForQuery.
    pub fn reply(&mut self) -> RawReply {
        use std::io::Read;
        let mut r = RawReply::default();
        let cstrs = |b: &[u8]| b.split(|x| *x == 0).map(|s| String::from_utf8_lossy(s).into_owned()).collect::<Vec<_>>();
        loop {
            let mut header = [0u8; 5];
            self.0.read_exact(&mut header).expect("raw read header");
            let len = i32::from_be_bytes(header[1..5].try_into().expect("length")) as usize;
            let mut body = vec![0u8; len - 4];
            self.0.read_exact(&mut body).expect("raw read body");
            match header[0] {
                b'R' => assert_eq!(&body[..4], &[0, 0, 0, 0], "raw connection needs trust authentication"),
                b'C' => r.tags.push(String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(&body)).into_owned()),
                b'E' => r
                    .errors
                    .extend(body.split(|b| *b == 0).filter_map(|f| f.strip_prefix(b"C")).map(|c| String::from_utf8_lossy(c).into_owned())),
                b'N' => r
                    .notices
                    .extend(body.split(|b| *b == 0).filter_map(|f| f.strip_prefix(b"M")).map(|m| String::from_utf8_lossy(m).into_owned())),
                b'A' => {
                    let s = cstrs(&body[4..]);
                    r.notifications.push((s[0].clone(), s[1].clone()));
                }
                b'D' => {
                    let n = i32::from_be_bytes(body[2..6].try_into().expect("length"));
                    r.values.push((n >= 0).then(|| String::from_utf8_lossy(&body[6..6 + n as usize]).into_owned()));
                }
                b'Z' => {
                    r.status = body[0];
                    return r;
                }
                _ => {}
            }
        }
    }

    /// The (tags, error SQLSTATEs) of a simple Query.
    pub fn simple_query(&mut self, sql: &str) -> (Vec<String>, Vec<String>) {
        let r = self.query(sql);
        (r.tags, r.errors)
    }

    /// A simple Query's full reply.
    pub fn query(&mut self, sql: &str) -> RawReply {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send(Some(b'Q'), &body);
        self.reply()
    }

    /// Parse (unnamed), Bind (unnamed, no parameters) and Execute for each statement, then
    /// one Sync: the extended-protocol pipeline.
    pub fn pipeline(&mut self, sqls: &[&str]) -> RawReply {
        for sql in sqls {
            let mut parse = vec![0u8];
            parse.extend_from_slice(sql.as_bytes());
            parse.extend_from_slice(&[0, 0, 0]);
            self.send(Some(b'P'), &parse);
            self.send(Some(b'B'), &[0, 0, 0, 0, 0, 0, 0, 0]);
            self.send(Some(b'E'), &[0, 0, 0, 0, 0]);
        }
        self.send(Some(b'S'), &[]);
        self.reply()
    }
}
