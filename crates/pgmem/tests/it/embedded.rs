//! The in-process API (`pgmem::Embedded`): simple-query semantics, text output and errors,
//! checked against the wire protocol (pgmem's, or real Postgres with `PGMEM_TEST_DSN`).

use crate::support::{TestDb, sqlstate};
use pgmem::{Embedded, SqlValue};
use postgres::{Client, SimpleQueryMessage};

type Rows = Vec<Vec<Option<String>>>;

/// A simple Query over the wire: each statement's rows, or the error's SQLSTATE.
fn wire(c: &mut Client, sql: &str) -> Result<Vec<Rows>, String> {
    let msgs = c.simple_query(sql).map_err(|e| sqlstate(&e))?;
    let mut out = Vec::new();
    let mut rows = Vec::new();
    for m in msgs {
        match m {
            SimpleQueryMessage::Row(r) => rows.push((0..r.len()).map(|i| r.get(i).map(str::to_owned)).collect()),
            SimpleQueryMessage::CommandComplete(_) => out.push(std::mem::take(&mut rows)),
            _ => {}
        }
    }
    Ok(out)
}

/// The same through `execute_batch`: each statement's rows, or the first error's SQLSTATE.
fn embedded(s: &mut pgmem::EmbeddedSession, sql: &str) -> Result<Vec<Rows>, String> {
    s.execute_batch(sql).into_iter().map(|r| r.map(|o| o.rows).map_err(|e| e.code().to_owned())).collect()
}

/// Runs each script on a fresh wire database and a fresh embedded one; every result matches.
#[track_caller]
fn same_as_wire(scripts: &[&str]) {
    let db = TestDb::new();
    let mut c = db.connect();
    let e = Embedded::new();
    let mut s = e.session();
    for sql in scripts {
        assert_eq!(embedded(&mut s, sql), wire(&mut c, sql), "script: {sql}");
    }
}

#[test]
fn implicit_block_rolls_back_on_error_like_wire() {
    same_as_wire(&[
        "CREATE TABLE t (a int); INSERT INTO t VALUES (1); SELECT 1/0",
        // The whole batch was one transaction: the table is gone.
        "SELECT * FROM t",
        "CREATE TABLE t (a int); INSERT INTO t VALUES (1), (2)",
        "INSERT INTO t VALUES (3); INSERT INTO t VALUES ('x'); INSERT INTO t VALUES (4)",
        "SELECT a FROM t ORDER BY a",
    ]);
}

#[test]
fn multiple_statements_return_each_result_like_wire() {
    same_as_wire(&[
        "CREATE TABLE t (id serial PRIMARY KEY, name text, born date)",
        "INSERT INTO t (name, born) VALUES ('a', '2024-01-02'), (NULL, NULL) RETURNING id, name, born; \
         SELECT count(*) FROM t; SET DateStyle = 'SQL, DMY'; SELECT born FROM t ORDER BY id",
        "SELECT true, 1.50::numeric(4,2), ARRAY[1,NULL,3], '{\"b\": 1, \"a\": [2]}'::jsonb, interval '1 day 2 hours'",
        "SET TimeZone = 'America/New_York'; SELECT timestamptz '2024-07-01 12:00:00+00'",
    ]);
}

#[test]
fn explicit_transactions_in_batches_like_wire() {
    same_as_wire(&[
        "CREATE TABLE t (a int)",
        // An explicit block inside the string survives the batch's error...
        "BEGIN; INSERT INTO t VALUES (1); SELECT 1/0",
        "SELECT 1",
        "ROLLBACK",
        // ...and COMMIT inside the string commits what came before it.
        "INSERT INTO t VALUES (2); COMMIT; INSERT INTO t VALUES (3); SELECT 1/0",
        "SELECT a FROM t ORDER BY a",
    ]);
}

#[test]
fn transaction_status_follows_blocks() {
    let e = Embedded::new();
    let mut s = e.session();
    assert_eq!(s.transaction_status(), 'I');
    s.query("BEGIN").expect("begin");
    assert_eq!(s.transaction_status(), 'T');
    let err = s.query("SELECT 1/0").expect_err("division by zero");
    assert_eq!(err.code(), "22012");
    assert_eq!(s.transaction_status(), 'E');
    assert_eq!(s.query("SELECT 1").expect_err("aborted").code(), "25P02");
    s.query("ROLLBACK").expect("rollback");
    assert_eq!(s.transaction_status(), 'I');
}

#[test]
fn errors_carry_sqlstate_message_detail_and_fields() {
    let e = Embedded::new();
    let mut s = e.session();
    s.query("CREATE TABLE users (id int PRIMARY KEY)").expect("create");
    let results = s.execute_batch("INSERT INTO users VALUES (1); INSERT INTO users VALUES (1); SELECT 1");
    assert_eq!(results.len(), 2, "execution stops at the first error: {results:?}");
    assert_eq!(results[0].as_ref().expect("first insert").tag, "INSERT 0 1");
    let err = results[1].clone().expect_err("duplicate key");
    assert_eq!(err.code(), "23505");
    assert_eq!(err.message(), "duplicate key value violates unique constraint \"users_pkey\"");
    assert_eq!(err.detail(), Some("Key (id)=(1) already exists."));
    assert_eq!(err.table(), Some("users"));
    assert_eq!(err.constraint(), Some("users_pkey"));
    assert_eq!(s.query("SELECT count(*) FROM users").expect("count").rows, vec![vec![Some("0".to_owned())]]);

    let err = s.query("SELECT * FROM missing").expect_err("undefined table");
    assert_eq!((err.code(), err.message()), ("42P01", "relation \"missing\" does not exist"));
    let err = s.query("SELECT nosuchfn(1)").expect_err("undefined function");
    assert_eq!(err.code(), "42883");
    assert!(err.hint().is_some(), "{err:?}");
}

#[test]
fn syntax_error_rejects_the_whole_string() {
    let e = Embedded::new();
    let mut s = e.session();
    let results = s.execute_batch("CREATE TABLE t (a int); SELEC 1");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].clone().expect_err("syntax error").code(), "42601");
    assert_eq!(s.query("SELECT * FROM t").expect_err("never created").code(), "42P01");
}

#[test]
fn outcome_describes_columns_tags_and_notices() {
    let e = Embedded::new();
    let mut s = e.session();
    s.query("CREATE DOMAIN posint AS int4 CHECK (VALUE > 0); CREATE TABLE t (id posint, price numeric(8,2), tags text[])").expect("ddl");
    let out = s.query("INSERT INTO t VALUES (1, 2.5, '{a,b}') RETURNING *").expect("insert");
    assert_eq!(out.tag, "INSERT 0 1");
    let cols: Vec<_> = out.columns.iter().map(|c| (c.name.as_str(), c.type_oid, c.type_name.as_str(), c.typmod, c.attnum)).collect();
    // A domain column is described by its base type, as by printtup.
    assert_eq!(cols, vec![("id", 23, "integer", -1, 1), ("price", 1700, "numeric", 524_294, 2), ("tags", 1009, "text[]", -1, 3)]);
    assert_eq!(out.rows, vec![vec![Some("1".into()), Some("2.50".into()), Some("{a,b}".into())]]);

    let out = s.query("DROP TABLE IF EXISTS nothing_here").expect("drop");
    assert_eq!(out.tag, "DROP TABLE");
    assert!(out.columns.is_empty());
    assert_eq!(out.notices.len(), 1, "{out:?}");
    assert_eq!(out.notices[0].severity, "NOTICE");
    assert_eq!(out.notices[0].message, "table \"nothing_here\" does not exist, skipping");

    let empty = s.query("").expect("empty");
    assert!(empty.empty && empty.tag.is_empty() && empty.rows.is_empty(), "{empty:?}");
    assert_eq!(s.setting("DateStyle"), Some("ISO, MDY"));
}

#[test]
fn copy_to_stdout_returns_data_and_copy_from_stdin_fails() {
    let e = Embedded::new();
    let mut s = e.session();
    s.query("CREATE TABLE t (a int, b text); INSERT INTO t VALUES (1, 'x'), (2, NULL)").expect("setup");
    let out = s.query("COPY t TO STDOUT").expect("copy out");
    assert_eq!(out.tag, "COPY 2");
    assert_eq!(out.copy_data.as_deref(), Some(&b"1\tx\n2\t\\N\n"[..]));
    let err = s.query("COPY t FROM STDIN").expect_err("no stdin");
    assert_eq!(err.code(), "57014");
    assert_eq!(s.transaction_status(), 'I');
}

#[test]
fn sessions_are_independent_and_share_committed_data() {
    let e = Embedded::new();
    let mut a = e.session();
    let mut b = e.session();
    a.query("CREATE TABLE t (a int); CREATE TEMP TABLE mine (x int); SET search_path = pg_catalog").expect("a");
    assert_eq!(b.query("SHOW search_path").expect("show").rows, vec![vec![Some("\"$user\", public".into())]]);
    assert_eq!(b.query("SELECT * FROM mine").expect_err("temp tables are per session").code(), "42P01");
    a.query("BEGIN; INSERT INTO public.t VALUES (1)").expect("insert");
    assert_eq!(b.query("SELECT count(*) FROM t").expect("count").rows, vec![vec![Some("0".into())]]);
    a.query("COMMIT").expect("commit");
    assert_eq!(b.query("SELECT count(*) FROM t").expect("count").rows, vec![vec![Some("1".into())]]);
    let pids = (a.query("SELECT pg_backend_pid()").expect("pid").rows, b.query("SELECT pg_backend_pid()").expect("pid").rows);
    assert_ne!(pids.0, pids.1);
}

/// Embedded sessions start like connections: `ALTER ROLE` / `ALTER DATABASE ... SET`, then the
/// startup parameters given to `session_with_params`, are their session defaults.
#[test]
fn embedded_sessions_take_startup_parameters_and_stored_settings() {
    let e = Embedded::new();
    e.session().query("ALTER DATABASE pgmem SET search_path = db_s, public; ALTER ROLE pgmem SET work_mem = '3MB'").expect("alter");
    let show = |s: &mut pgmem::EmbeddedSession| {
        s.query("SELECT current_setting('search_path') || ' ' || current_setting('work_mem')").expect("show").rows
    };
    assert_eq!(show(&mut e.session()), vec![vec![Some("db_s, public 3MB".into())]]);
    let mut s = e.session_with_params(&[("options", "-c work_mem=2MB"), ("search_path", "opt")]).expect("params");
    assert_eq!(show(&mut s), vec![vec![Some("opt 2MB".into())]]);
    s.query("SET search_path = public; RESET search_path").expect("reset");
    assert_eq!(show(&mut s), vec![vec![Some("opt 2MB".into())]]);
    let err = e.session_with_params(&[("options", "-c nosuch=1")]).expect_err("unknown parameter");
    assert_eq!((err.code(), err.message()), ("42704", "unrecognized configuration parameter \"nosuch\""));
}

#[test]
fn server_and_embedded_share_one_database() {
    let db = TestDb::new();
    let Some(server) = db.server() else { return };
    let mut c = db.connect();
    c.batch_execute("CREATE TABLE t (a int); INSERT INTO t VALUES (7)").expect("setup");
    let mut s = server.embedded().session();
    assert_eq!(s.query("SELECT a FROM t").expect("select").rows, vec![vec![Some("7".into())]]);
    s.query("INSERT INTO t VALUES (8)").expect("insert");
    let n: i64 = c.query_one("SELECT count(*) FROM t", &[]).expect("count").get(0);
    assert_eq!(n, 2);

    // A server can also be started on an embedded database.
    let e = Embedded::new();
    e.session().query("CREATE TABLE u (b text); INSERT INTO u VALUES ('hi')").expect("setup");
    let served = pgmem::Server::builder().start_on(&e).expect("start");
    let mut c = Client::connect(&served.conninfo(), postgres::NoTls).expect("connect");
    let b: String = c.query_one("SELECT b FROM u", &[]).expect("select").get(0);
    assert_eq!(b, "hi");
}

#[test]
fn to_pg_text_renders_values_like_postgres() {
    let cases: Vec<(SqlValue, Option<&str>)> = vec![
        (SqlValue::Null, None),
        (SqlValue::Bool(true), Some("t")),
        (SqlValue::Int8(-42), Some("-42")),
        (SqlValue::Float8(0.1), Some("0.1")),
        (SqlValue::Text("a b".into()), Some("a b")),
        (SqlValue::Bytea(vec![0xde, 0xad]), Some("\\xdead")),
        (SqlValue::Date(0), Some("2000-01-01")),
        (SqlValue::Timestamptz(0), Some("2000-01-01 00:00:00+00")),
        (SqlValue::Array(vec![SqlValue::Text("a".into()), SqlValue::Null, SqlValue::Text("b c".into())]), Some("{a,NULL,\"b c\"}")),
    ];
    for (v, want) in cases {
        assert_eq!(pgmem::to_pg_text(&v).expect("encode").as_deref(), want, "{v:?}");
    }
}
