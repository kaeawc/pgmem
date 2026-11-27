//! `Embedded::fork` / `Server::fork`: an independent copy of the committed state.

use crate::support::TestDb;
use pgmem::{Embedded, EmbeddedSession};
use postgres::{Client, NoTls};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn rows(s: &mut EmbeddedSession, sql: &str) -> Vec<Vec<Option<String>>> {
    s.query(sql).unwrap_or_else(|e| panic!("{sql}: {e}")).rows
}

fn one(s: &mut EmbeddedSession, sql: &str) -> String {
    rows(s, sql).remove(0).remove(0).expect("non-null")
}

fn base() -> Embedded {
    let e = Embedded::new();
    e.session()
        .query(
            "CREATE SCHEMA app; CREATE TYPE app.mood AS ENUM ('ok', 'meh'); \
             CREATE TABLE app.items (id serial PRIMARY KEY, name text NOT NULL, mood app.mood DEFAULT 'ok'); \
             CREATE VIEW app.names AS SELECT name FROM app.items; CREATE SEQUENCE app.counter; \
             CREATE ROLE reader; GRANT USAGE ON SCHEMA app TO reader; \
             INSERT INTO app.items (name) VALUES ('a'), ('b'); SELECT nextval('app.counter'), nextval('app.counter')",
        )
        .expect("base schema");
    e
}

#[test]
fn fork_copies_catalog_rows_and_sequences() {
    let e = base();
    let fork = e.fork();
    let mut f = fork.session();
    assert_eq!(rows(&mut f, "SELECT name FROM app.names ORDER BY 1"), vec![vec![Some("a".into())], vec![Some("b".into())]]);
    assert_eq!(one(&mut f, "SELECT mood FROM app.items WHERE id = 1"), "ok");
    assert_eq!(one(&mut f, "SELECT has_schema_privilege('reader', 'app', 'USAGE')"), "t");
    // Sequences continue from their current values.
    assert_eq!(one(&mut f, "SELECT nextval('app.counter')"), "3");
    assert_eq!(one(&mut f, "INSERT INTO app.items (name) VALUES ('c') RETURNING id"), "3");
}

#[test]
fn forks_are_independent_both_ways() {
    let e = base();
    let fork = e.fork();
    let (mut o, mut f) = (e.session(), fork.session());

    f.query("INSERT INTO app.items (name) VALUES ('fork'); ALTER TABLE app.items ADD COLUMN extra int; DROP ROLE reader")
        .expect_err("reader still owns privileges");
    f.query("INSERT INTO app.items (name) VALUES ('fork'); ALTER TABLE app.items ADD COLUMN extra int; CREATE TABLE app.only_fork ()")
        .expect("fork writes");
    o.query("DELETE FROM app.items WHERE name = 'a'; CREATE TABLE app.only_origin ()").expect("origin writes");

    assert_eq!(one(&mut o, "SELECT string_agg(name, ',' ORDER BY id) FROM app.items"), "b");
    assert_eq!(one(&mut f, "SELECT string_agg(name, ',' ORDER BY id) FROM app.items"), "a,b,fork");
    assert_eq!(o.query("SELECT extra FROM app.items").expect_err("no column in origin").code(), "42703");
    assert_eq!(o.query("SELECT * FROM app.only_fork").expect_err("fork's table").code(), "42P01");
    assert_eq!(f.query("SELECT * FROM app.only_origin").expect_err("origin's table").code(), "42P01");
    // Each side's serial / sequence advances on its own.
    assert_eq!(one(&mut o, "INSERT INTO app.items (name) VALUES ('o') RETURNING id"), "3");
    assert_eq!(one(&mut o, "SELECT nextval('app.counter')"), "3");
    assert_eq!(one(&mut f, "SELECT nextval('app.counter')"), "3");
    assert_eq!(one(&mut f, "SELECT nextval('app.counter')"), "4");
    assert_eq!(one(&mut o, "SELECT currval('app.counter')"), "3");
}

#[test]
fn fork_of_a_fork_and_uncommitted_work() {
    let e = base();
    let mut o = e.session();
    o.query("BEGIN; INSERT INTO app.items (name) VALUES ('pending'); CREATE TABLE app.pending ()").expect("open txn");
    let fork = e.fork();
    o.query("COMMIT").expect("commit");
    let mut f = fork.session();
    // Work in an open transaction is not part of the committed state.
    assert_eq!(one(&mut f, "SELECT count(*) FROM app.items"), "2");
    assert_eq!(f.query("SELECT * FROM app.pending").expect_err("uncommitted DDL").code(), "42P01");
    assert_eq!(one(&mut o, "SELECT count(*) FROM app.items"), "3");

    f.query("INSERT INTO app.items (name) VALUES ('f1')").expect("insert");
    let grandchild = fork.fork();
    f.query("INSERT INTO app.items (name) VALUES ('f2')").expect("insert");
    assert_eq!(one(&mut grandchild.session(), "SELECT string_agg(name, ',' ORDER BY id) FROM app.items"), "a,b,f1");
}

#[test]
fn forks_run_concurrent_sessions() {
    let e = base();
    let forks: Vec<Embedded> = (0..4).map(|_| e.fork()).collect();
    std::thread::scope(|scope| {
        for (i, fork) in forks.iter().enumerate() {
            for _ in 0..2 {
                scope.spawn(move || {
                    let mut s = fork.session();
                    for n in 0..25 {
                        s.query(&format!("INSERT INTO app.items (name) VALUES ('f{i}-{n}')")).expect("insert");
                    }
                });
            }
        }
        // The origin keeps working while the forks are written to.
        scope.spawn(|| {
            let mut s = e.session();
            for _ in 0..25 {
                s.query("UPDATE app.items SET name = name || '!' WHERE id = 1").expect("update");
            }
        });
    });
    for (i, fork) in forks.iter().enumerate() {
        let mut s = fork.session();
        assert_eq!(one(&mut s, "SELECT count(*) FROM app.items"), "52");
        assert_eq!(one(&mut s, &format!("SELECT count(*) FROM app.items WHERE name LIKE 'f{i}-%'")), "50");
        assert_eq!(one(&mut s, "SELECT max(id) FROM app.items"), "52");
        assert_eq!(one(&mut s, "SELECT name FROM app.items WHERE id = 1"), "a");
    }
    let mut s = e.session();
    assert_eq!(one(&mut s, "SELECT count(*) FROM app.items"), "2");
    assert_eq!(one(&mut s, "SELECT length(name) FROM app.items WHERE id = 1"), "26");
}

#[test]
fn fork_copies_pinned_clock() {
    let e = Embedded::new();
    let pinned = UNIX_EPOCH + Duration::from_secs(1_720_000_000);
    e.set_now(Some(pinned));
    let fork = e.fork();
    e.set_now(None);
    assert_eq!(one(&mut fork.session(), "SELECT extract(epoch FROM now())::bigint"), "1720000000");
}

#[test]
fn server_fork_serves_an_independent_copy() {
    let db = TestDb::new();
    let Some(server) = db.server() else { return };
    let mut c = db.connect();
    c.batch_execute("CREATE TABLE t (id bigserial PRIMARY KEY, v text); INSERT INTO t (v) VALUES ('base')").expect("setup");
    server.set_now(Some(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400)));
    let fork = server.fork().expect("fork");
    assert_ne!(fork.port(), server.port());
    let mut fc = Client::connect(&fork.conninfo(), NoTls).expect("connect to fork");
    let id: i64 = fc.query_one("INSERT INTO t (v) VALUES ('fork') RETURNING id", &[]).expect("insert").get(0);
    assert_eq!(id, 2);
    c.batch_execute("TRUNCATE t").expect("truncate origin");
    let n: i64 = fc.query_one("SELECT count(*) FROM t", &[]).expect("count").get(0);
    assert_eq!(n, 2);
    let epoch: i64 = fc.query_one("SELECT extract(epoch FROM now())::bigint", &[]).expect("now").get(0);
    assert_eq!(epoch, 86_400);
}

#[test]
fn fork_keeps_the_public_schema_comment() {
    let e = Embedded::new();
    let comment = "SELECT obj_description('public'::regnamespace, 'pg_namespace')";
    assert_eq!(one(&mut e.fork().session(), comment), "standard public schema");
    // A changed comment is copied, and changing it in the fork leaves the origin alone.
    e.session().query("COMMENT ON SCHEMA public IS 'app data'").expect("comment");
    let fork = e.fork();
    let mut f = fork.session();
    assert_eq!(one(&mut f, comment), "app data");
    f.query("COMMENT ON SCHEMA public IS NULL").expect("uncomment");
    assert_eq!(rows(&mut f, comment), vec![vec![None]]);
    assert_eq!(one(&mut e.session(), comment), "app data");
}
