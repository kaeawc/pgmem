use crate::support::{TestDb, expect_code, setup};
use postgres::Client;
use std::io::{Read, Write};
use std::net::TcpStream;

/// A fresh database with an `accounts` table and one pinned connection.
fn txn_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE accounts (id int PRIMARY KEY, name text NOT NULL)").expect("CREATE");
    (db, c)
}

fn count_accounts(c: &mut Client) -> usize {
    c.query("SELECT id FROM accounts", &[]).expect("SELECT").len()
}

#[test]
fn txn_rollback_undoes_insert() {
    let (_db, mut c) = txn_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice')", &[]).expect("INSERT");
    assert_eq!(count_accounts(&mut c), 1, "in-tx count");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    assert_eq!(count_accounts(&mut c), 0, "post-rollback count");
}

#[test]
fn txn_commit_persists_insert() {
    let (_db, mut c) = txn_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice')", &[]).expect("INSERT");
    c.batch_execute("COMMIT").expect("COMMIT");
    assert_eq!(count_accounts(&mut c), 1, "post-commit count");
}

#[test]
fn txn_aborted_state_poisons_block() {
    let (_db, mut c) = txn_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice')", &[]).expect("first INSERT");
    expect_code(c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice-clone')", &[]), "23505");
    expect_code(c.execute("INSERT INTO accounts (id, name) VALUES (2, 'bob')", &[]), "25P02");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    c.execute("INSERT INTO accounts (id, name) VALUES (2, 'bob')", &[]).expect("post-rollback INSERT");
    assert_eq!(count_accounts(&mut c), 1, "only bob: the first txn's alice was rolled back");
}

/// The value of `key` in a libpq key=value connection string.
fn conninfo_value(conninfo: &str, key: &str) -> Option<String> {
    conninfo.split_whitespace().find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=').map(str::to_owned))
}

/// A minimal raw frontend/backend protocol connection (trust auth only), used where the test
/// needs the CommandComplete tag, which the postgres crate does not expose.
struct RawConn(TcpStream);

impl RawConn {
    fn connect(db: &TestDb, c: &mut Client) -> RawConn {
        let base = match db.server() {
            Some(server) => server.conninfo(),
            None => std::env::var("PGMEM_TEST_DSN").expect("PGMEM_TEST_DSN"),
        };
        let dbname: String = c.query_one("SELECT current_database()::text", &[]).expect("current_database").get(0);
        let host = conninfo_value(&base, "host").unwrap_or_else(|| "127.0.0.1".to_owned());
        let port = conninfo_value(&base, "port").unwrap_or_else(|| "5432".to_owned());
        let user = conninfo_value(&base, "user").unwrap_or_else(|| "postgres".to_owned());
        let mut raw = RawConn(TcpStream::connect(format!("{host}:{port}")).expect("raw connect"));
        let mut body = 196_608i32.to_be_bytes().to_vec();
        for s in ["user", &user, "database", &dbname, ""] {
            body.extend_from_slice(s.as_bytes());
            body.push(0);
        }
        raw.send(None, &body);
        raw.until_ready();
        raw
    }

    fn send(&mut self, tag: Option<u8>, body: &[u8]) {
        let mut msg = tag.map(|t| vec![t]).unwrap_or_default();
        msg.extend_from_slice(&i32::try_from(body.len() + 4).expect("message length").to_be_bytes());
        msg.extend_from_slice(body);
        self.0.write_all(&msg).expect("raw write");
    }

    /// Reads messages up to ReadyForQuery; returns the CommandComplete tags and error SQLSTATEs.
    fn until_ready(&mut self) -> (Vec<String>, Vec<String>) {
        let (mut tags, mut errors) = (Vec::new(), Vec::new());
        loop {
            let mut header = [0u8; 5];
            self.0.read_exact(&mut header).expect("raw read header");
            let len = i32::from_be_bytes(header[1..5].try_into().expect("length")) as usize;
            let mut body = vec![0u8; len - 4];
            self.0.read_exact(&mut body).expect("raw read body");
            match header[0] {
                b'R' => assert_eq!(&body[..4], &[0, 0, 0, 0], "raw connection needs trust authentication"),
                b'C' => tags.push(String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(&body)).into_owned()),
                b'E' => errors
                    .extend(body.split(|b| *b == 0).filter_map(|f| f.strip_prefix(b"C")).map(|c| String::from_utf8_lossy(c).into_owned())),
                b'Z' => return (tags, errors),
                _ => {}
            }
        }
    }

    fn simple_query(&mut self, sql: &str) -> (Vec<String>, Vec<String>) {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.send(Some(b'Q'), &body);
        self.until_ready()
    }
}

#[test]
fn txn_commit_after_error_acts_as_rollback() {
    let (db, mut c) = txn_setup();
    let mut raw = RawConn::connect(&db, &mut c);
    assert_eq!(raw.simple_query("BEGIN"), (vec!["BEGIN".to_owned()], vec![]));
    assert_eq!(raw.simple_query("INSERT INTO accounts (id, name) VALUES (1, 'alice')").1, Vec::<String>::new());
    // Poisons the transaction.
    assert_eq!(raw.simple_query("INSERT INTO accounts (id, name) VALUES (1, 'dup')").1, vec!["23505".to_owned()]);
    let (tags, errors) = raw.simple_query("COMMIT");
    assert!(errors.is_empty(), "COMMIT errored: {errors:?}");
    assert_eq!(tags, vec!["ROLLBACK".to_owned()], "CommandComplete tag");
    assert_eq!(count_accounts(&mut c), 0, "commit-after-error must roll back");
}

#[test]
fn txn_implicit_autocommit() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE bare (n int NOT NULL)").expect("CREATE");
    c.execute("INSERT INTO bare (n) VALUES (1), (2), (3)", &[]).expect("INSERT");
    // A different connection sees the rows, so they were committed.
    let mut other = db.connect();
    let rows = other.query("SELECT n FROM bare", &[]).expect("SELECT");
    let mut got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2, 3]);
}
