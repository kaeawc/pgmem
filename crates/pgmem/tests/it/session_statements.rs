//! SQL-level PREPARE / EXECUTE, cursors, LISTEN / NOTIFY, DISCARD, utility statements and
//! extended-protocol transactions per Sync.

use crate::support::{RawConn, expect_code, message, setup};
use postgres::fallible_iterator::FallibleIterator;
use std::time::Duration;

fn ints(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect()
}

/// First-column values of a simple-protocol query.
fn simple_values(c: &mut postgres::Client, sql: &str) -> Vec<String> {
    let msgs = c.simple_query(sql).expect(sql);
    msgs.iter().filter_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { r.get(0).map(str::to_owned) } else { None }).collect()
}

#[test]
fn prepare_and_execute() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE items (id int PRIMARY KEY, name text); INSERT INTO items VALUES (1, 'one'), (2, 'two')").expect("setup");
    c.batch_execute("PREPARE get_item (int) AS SELECT name FROM items WHERE id = $1").expect("PREPARE");
    assert_eq!(simple_values(&mut c, "EXECUTE get_item(2)"), ["two"]);
    // Arguments are coerced by assignment rules: 1.5 becomes 2 for an integer parameter.
    c.batch_execute("PREPARE plus_one AS SELECT $1 + 1").expect("PREPARE untyped");
    assert_eq!(simple_values(&mut c, "EXECUTE plus_one(1.5)"), ["3"]);
    let e = expect_code(c.batch_execute("EXECUTE get_item(1, 2)"), "42601");
    assert_eq!(message(&e), "wrong number of parameters for prepared statement \"get_item\"");
    let e = expect_code(c.batch_execute("EXECUTE get_item('1'::text)"), "42804");
    assert_eq!(message(&e), "parameter $1 of type text cannot be coerced to the expected type integer");
    expect_code(c.batch_execute("PREPARE get_item AS SELECT 1"), "42P05");
    // DML runs through EXECUTE too.
    c.batch_execute("PREPARE add (int, text) AS INSERT INTO items VALUES ($1, $2)").expect("PREPARE insert");
    c.batch_execute("EXECUTE add(3, 'three')").expect("EXECUTE insert");
    assert_eq!(ints(&mut c, "SELECT count(*)::int FROM items"), [3]);
    c.batch_execute("DEALLOCATE get_item").expect("DEALLOCATE");
    expect_code(c.batch_execute("EXECUTE get_item(1)"), "26000");
    expect_code(c.batch_execute("DEALLOCATE get_item"), "26000");
}

#[test]
fn prepared_statements_view_shows_sql_and_protocol_statements() {
    let (_db, mut c) = setup();
    c.batch_execute("PREPARE q (int, text) AS SELECT $1, $2").expect("PREPARE");
    let rows = c
        .query(
            "SELECT statement, parameter_types::text[], result_types::text[], from_sql FROM pg_prepared_statements WHERE name = 'q'",
            &[],
        )
        .expect("view");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), "PREPARE q (int, text) AS SELECT $1, $2");
    assert_eq!(rows[0].get::<_, Vec<String>>(1), ["integer", "text"]);
    assert_eq!(rows[0].get::<_, Vec<String>>(2), ["integer", "text"]);
    assert!(rows[0].get::<_, bool>(3));
    // The client's own protocol-level statement is listed, not from SQL.
    let stmt = c.prepare("SELECT 41 + 1").expect("Parse");
    let protocol: Vec<bool> = c
        .query("SELECT from_sql FROM pg_prepared_statements WHERE statement = 'SELECT 41 + 1'", &[])
        .expect("view")
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(protocol, [false]);
    drop(stmt);
    c.batch_execute("DEALLOCATE ALL").expect("DEALLOCATE ALL");
    assert_eq!(ints(&mut c, "SELECT count(*)::int FROM pg_prepared_statements WHERE from_sql"), [0]);
}

#[test]
fn cursor_fetch_and_move() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE n (v int); INSERT INTO n SELECT generate_series(1, 5)").expect("setup");
    let mut raw = RawConn::connect(&db, &mut c);
    raw.simple_query("BEGIN");
    assert_eq!(raw.simple_query("DECLARE c SCROLL CURSOR FOR SELECT v FROM n ORDER BY v").0, ["DECLARE CURSOR"]);
    let fetch = |raw: &mut RawConn, sql: &str| {
        let r = raw.query(sql);
        assert!(r.errors.is_empty(), "{sql}: {:?}", r.errors);
        (r.tags, r.values.into_iter().map(|v| v.unwrap_or_default()).collect::<Vec<_>>())
    };
    assert_eq!(fetch(&mut raw, "FETCH 2 FROM c"), (vec!["FETCH 2".into()], vec!["1".into(), "2".into()]));
    assert_eq!(fetch(&mut raw, "MOVE BACKWARD ALL IN c").0, ["MOVE 1"]);
    assert_eq!(fetch(&mut raw, "MOVE ALL IN c").0, ["MOVE 5"]);
    assert_eq!(fetch(&mut raw, "FETCH BACKWARD 2 FROM c").1, ["5", "4"]);
    assert_eq!(fetch(&mut raw, "FETCH RELATIVE 0 FROM c").1, ["4"]);
    assert_eq!(fetch(&mut raw, "FETCH ABSOLUTE -1 FROM c").1, ["5"]);
    assert_eq!(fetch(&mut raw, "FETCH -1 FROM c").1, ["4"]);
    assert_eq!(fetch(&mut raw, "MOVE ABSOLUTE 9 IN c").0, ["MOVE 0"]);
    assert_eq!(fetch(&mut raw, "FETCH PRIOR FROM c").1, ["5"]);
    assert_eq!(raw.simple_query("DECLARE c CURSOR FOR SELECT 1").1, ["42P03"]);
    raw.simple_query("ROLLBACK");
    assert_eq!(raw.simple_query("CLOSE c"), (vec![], vec!["34000".into()]));
}

#[test]
fn no_scroll_cursor_cannot_go_back() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE n (v int); INSERT INTO n SELECT generate_series(1, 5)").expect("setup");
    c.batch_execute("BEGIN; DECLARE ns NO SCROLL CURSOR FOR SELECT v FROM n").expect("DECLARE");
    c.batch_execute("FETCH ABSOLUTE 3 FROM ns").expect("forward is fine");
    let e = expect_code(c.batch_execute("FETCH PRIOR FROM ns"), "55000");
    assert_eq!(message(&e), "cursor can only scan forward");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
}

#[test]
fn cursor_lifetimes() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE n (v int); INSERT INTO n SELECT generate_series(1, 3)").expect("setup");
    let e = expect_code(c.batch_execute("DECLARE outside CURSOR FOR SELECT 1"), "25P01");
    assert_eq!(message(&e), "DECLARE CURSOR can only be used in transaction blocks");
    // A plain cursor ends with its transaction; WITH HOLD survives commit, not rollback.
    c.batch_execute(
        "BEGIN; DECLARE plain CURSOR FOR SELECT v FROM n; DECLARE held CURSOR WITH HOLD FOR SELECT v FROM n ORDER BY v DESC; COMMIT",
    )
    .expect("DECLARE");
    expect_code(c.batch_execute("FETCH plain"), "34000");
    assert_eq!(ints(&mut c, "FETCH 2 FROM held"), [3, 2]);
    c.batch_execute("CLOSE held").expect("CLOSE");
    c.batch_execute("BEGIN; DECLARE gone CURSOR WITH HOLD FOR SELECT 1; ROLLBACK").expect("rolled back");
    expect_code(c.batch_execute("FETCH gone"), "34000");
    // ROLLBACK TO a savepoint closes cursors declared after it.
    c.batch_execute("BEGIN; SAVEPOINT a; DECLARE inner_c CURSOR FOR SELECT 1; ROLLBACK TO a").expect("savepoint");
    expect_code(c.batch_execute("FETCH inner_c"), "34000");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    // The cursor sees the rows as of DECLARE.
    c.batch_execute("BEGIN; DECLARE snap CURSOR FOR SELECT v FROM n ORDER BY v; DELETE FROM n").expect("DECLARE then DELETE");
    assert_eq!(ints(&mut c, "FETCH ALL FROM snap"), [1, 2, 3]);
    // The extended-protocol portal running this query is listed too, as the unnamed cursor.
    let open: Vec<(String, String, bool, bool, bool)> = c
        .query("SELECT name, statement, is_holdable, is_binary, is_scrollable FROM pg_cursors ORDER BY name", &[])
        .expect("pg_cursors")
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        .collect();
    assert_eq!(
        open,
        [
            (
                "".to_owned(),
                "SELECT name, statement, is_holdable, is_binary, is_scrollable FROM pg_cursors ORDER BY name".to_owned(),
                false,
                false,
                false
            ),
            ("snap".to_owned(), "BEGIN; DECLARE snap CURSOR FOR SELECT v FROM n ORDER BY v; DELETE FROM n".to_owned(), false, false, true),
        ]
    );
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
}

#[test]
fn binary_cursor_sends_binary_rows() {
    let (db, mut c) = setup();
    let mut raw = RawConn::connect(&db, &mut c);
    raw.simple_query("BEGIN");
    raw.simple_query("DECLARE b BINARY CURSOR FOR SELECT 258::int4");
    let r = raw.query("FETCH b");
    assert_eq!(r.values, [Some("\0\0\u{1}\u{2}".to_owned())]);
    raw.simple_query("ROLLBACK");
}

#[test]
fn notify_reaches_listeners_at_commit() {
    let (db, mut c) = setup();
    let mut listener = db.connect();
    listener.batch_execute("LISTEN events").expect("LISTEN");
    c.batch_execute("NOTIFY events, 'one'").expect("NOTIFY");
    // Inside a transaction, notifications wait for COMMIT and duplicates fold; ROLLBACK drops them.
    c.batch_execute("BEGIN; NOTIFY events, 'two'; NOTIFY events, 'two'; SELECT pg_notify('events', 'three'); COMMIT").expect("txn");
    c.batch_execute("BEGIN; NOTIFY events, 'dropped'; ROLLBACK").expect("rolled back");
    c.batch_execute("NOTIFY other_channel, 'ignored'").expect("NOTIFY other");
    let mut got = Vec::new();
    let mut notes = listener.notifications();
    let mut it = notes.timeout_iter(Duration::from_secs(2));
    while got.len() < 3 {
        match it.next() {
            Ok(Some(n)) => got.push((n.channel().to_owned(), n.payload().to_owned())),
            other => panic!("expected a notification, got {other:?} after {got:?}"),
        }
    }
    let expect = |p: &str| ("events".to_owned(), p.to_owned());
    assert_eq!(got, [expect("one"), expect("two"), expect("three")]);
    drop(it);
    drop(notes);
    let channels: Vec<String> = listener.query("SELECT pg_listening_channels()", &[]).expect("channels").iter().map(|r| r.get(0)).collect();
    assert_eq!(channels, ["events"]);
    listener.batch_execute("UNLISTEN *").expect("UNLISTEN");
    assert_eq!(ints(&mut listener, "SELECT count(*)::int FROM pg_listening_channels()"), [0]);
}

#[test]
fn own_notification_arrives_before_ready_for_query() {
    let (db, mut c) = setup();
    let mut raw = RawConn::connect(&db, &mut c);
    raw.simple_query("LISTEN me");
    let r = raw.query("NOTIFY me, 'hi'");
    assert_eq!(r.tags, ["NOTIFY"]);
    assert_eq!(r.notifications, [("me".to_owned(), "hi".to_owned())]);
    // A listener inside a transaction block gets it after its own transaction ends.
    raw.simple_query("BEGIN");
    c.batch_execute("NOTIFY me, 'later'").expect("NOTIFY");
    std::thread::sleep(Duration::from_millis(100));
    assert!(raw.query("SELECT 1").notifications.is_empty());
    assert_eq!(raw.query("COMMIT").notifications, [("me".to_owned(), "later".to_owned())]);
}

#[test]
fn notify_argument_checks() {
    let (_db, mut c) = setup();
    let e = expect_code(c.batch_execute("SELECT pg_notify('', 'x')"), "22023");
    assert_eq!(message(&e), "channel name cannot be empty");
    let e = expect_code(c.batch_execute("SELECT pg_notify(repeat('c', 64), 'x')"), "22023");
    assert_eq!(message(&e), "channel name too long");
    let e = expect_code(c.batch_execute("SELECT pg_notify('c', repeat('x', 8000))"), "22023");
    assert_eq!(message(&e), "payload string too long");
}

#[test]
fn pipeline_commits_at_sync() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE p (id int PRIMARY KEY)").expect("CREATE");
    let mut raw = RawConn::connect(&db, &mut c);
    // A failure rolls back the whole pipeline, including statements before it.
    let r = raw.pipeline(&["INSERT INTO p VALUES (1)", "INSERT INTO p VALUES (1)", "INSERT INTO p VALUES (2)"]);
    assert_eq!(r.tags, ["INSERT 0 1"]);
    assert_eq!(r.errors, ["23505"]);
    assert_eq!(r.status, b'I');
    assert_eq!(ints(&mut c, "SELECT count(*)::int FROM p"), [0]);
    // Statements of one pipeline share a transaction: one now(), visible to other sessions after Sync.
    let r = raw.pipeline(&["INSERT INTO p VALUES (1)", "INSERT INTO p VALUES (2)", "SELECT count(DISTINCT now())::text FROM p"]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.values, [Some("1".to_owned())]);
    assert_eq!(ints(&mut c, "SELECT count(*)::int FROM p"), [2]);
    // BEGIN inside a pipeline turns it into an explicit block.
    let r = raw.pipeline(&["INSERT INTO p VALUES (3)", "BEGIN", "INSERT INTO p VALUES (4)"]);
    assert_eq!(r.status, b'T');
    assert_eq!(ints(&mut c, "SELECT count(*)::int FROM p"), [2]);
    raw.simple_query("ROLLBACK");
    assert_eq!(ints(&mut c, "SELECT count(*)::int FROM p"), [2]);
}

#[test]
fn discard_resets_session_state() {
    let (_db, mut c) = setup();
    c.batch_execute("PREPARE p AS SELECT 1; SET application_name = 'before'; CREATE TEMP TABLE scratch (x int)").expect("setup");
    c.batch_execute("CREATE SEQUENCE s; SELECT nextval('s')").expect("sequence");
    c.batch_execute("DISCARD SEQUENCES").expect("DISCARD SEQUENCES");
    expect_code(c.batch_execute("SELECT currval('s')"), "55000");
    c.batch_execute("BEGIN").expect("BEGIN");
    let e = expect_code(c.batch_execute("DISCARD ALL"), "25001");
    assert_eq!(message(&e), "DISCARD ALL cannot run inside a transaction block");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    c.batch_execute("DISCARD ALL").expect("DISCARD ALL");
    expect_code(c.batch_execute("EXECUTE p"), "26000");
    expect_code(c.batch_execute("SELECT * FROM scratch"), "42P01");
    let name: String = c.query_one("SHOW application_name", &[]).expect("SHOW").get(0);
    assert_eq!(name, "");
}

#[test]
fn utility_statement_checks() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY)").expect("CREATE");
    for sql in ["VACUUM (VERBOSE, ANALYZE) t", "ANALYZE t (id)", "REINDEX TABLE t", "CLUSTER t USING t_pkey", "CHECKPOINT"] {
        c.batch_execute(sql).expect(sql);
    }
    // A multi-statement query string is a transaction block from its first statement.
    expect_code(c.batch_execute("VACUUM t; SELECT 1"), "25001");
    expect_code(c.batch_execute("VACUUM missing_table"), "42P01");
    c.batch_execute("BEGIN").expect("BEGIN");
    let e = expect_code(c.batch_execute("VACUUM t"), "25001");
    assert_eq!(message(&e), "VACUUM cannot run inside a transaction block");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    let e = expect_code(c.batch_execute("LOCK TABLE t IN ACCESS EXCLUSIVE MODE"), "25P01");
    assert_eq!(message(&e), "LOCK TABLE can only be used in transaction blocks");
    c.batch_execute("BEGIN; LOCK TABLE t IN SHARE MODE NOWAIT; COMMIT").expect("LOCK in a block");
    expect_code(c.batch_execute("EXPLAIN SELECT * FROM missing_table"), "42P01");
    // EXPLAIN runs nothing; EXPLAIN ANALYZE runs the statement.
    c.batch_execute("EXPLAIN INSERT INTO t VALUES (1)").expect("EXPLAIN");
    assert_eq!(ints(&mut c, "SELECT count(*)::int FROM t"), [0]);
    c.batch_execute("EXPLAIN ANALYZE INSERT INTO t VALUES (1)").expect("EXPLAIN ANALYZE");
    assert_eq!(ints(&mut c, "SELECT count(*)::int FROM t"), [1]);
    let plan = c.query("EXPLAIN (FORMAT JSON) SELECT 1", &[]).expect("EXPLAIN JSON");
    assert_eq!(plan[0].columns()[0].name(), "QUERY PLAN");
    assert_eq!(plan[0].columns()[0].type_(), &postgres::types::Type::JSON);
}
