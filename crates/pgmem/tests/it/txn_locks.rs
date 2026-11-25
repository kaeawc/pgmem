//! System columns, row and table locks across sessions, REPEATABLE READ conflicts, advisory
//! locks, READ ONLY and AND CHAIN transactions.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn ints(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect()
}

#[test]
fn system_columns() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE kv (k text PRIMARY KEY, v int); INSERT INTO kv VALUES ('a', 1), ('b', 2)").expect("setup");
    let r = rows(
        &mut c,
        "SELECT pg_typeof(ctid)::text, pg_typeof(xmin)::text, pg_typeof(xmax)::text, tableoid::regclass::text, xmax FROM kv WHERE k = 'a'",
    );
    assert_eq!(r, [["tid", "xid", "xid", "kv", "0"]]);
    // `*` and the row type leave them out.
    assert_eq!(rows(&mut c, "SELECT * FROM kv ORDER BY k"), [["a", "1"], ["b", "2"]]);
    assert_eq!(rows(&mut c, "SELECT kv FROM kv ORDER BY k"), [["(a,1)"], ["(b,2)"]]);
    // A new row version has a new ctid.
    let before: Vec<String> = rows(&mut c, "SELECT ctid::text FROM kv WHERE k = 'a'").concat();
    c.batch_execute("UPDATE kv SET v = 10 WHERE k = 'a'").expect("UPDATE");
    let after: Vec<String> = rows(&mut c, "SELECT ctid::text FROM kv WHERE k = 'a'").concat();
    assert_ne!(before, after);
    // The upsert idiom: xmax = 0 tells inserted rows from updated ones.
    let upsert = rows(
        &mut c,
        "INSERT INTO kv VALUES ('a', 5), ('c', 6) ON CONFLICT (k) DO UPDATE SET v = EXCLUDED.v RETURNING k, (xmax = 0) AS inserted",
    );
    assert_eq!(upsert, [["a", "f"], ["c", "t"]]);
    // Deleting by ctid.
    assert_eq!(rows(&mut c, "DELETE FROM kv WHERE ctid = (SELECT max(ctid) FROM kv) RETURNING k"), [["c"]]);
}

#[test]
fn where_current_of() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE emp (id int, name text); INSERT INTO emp VALUES (1, 'ann'), (2, 'bob'), (3, 'cid')").expect("setup");
    // A sorted cursor is not a simple scan of the table unless it is FOR UPDATE.
    c.batch_execute("BEGIN; DECLARE cur CURSOR FOR SELECT id FROM emp ORDER BY id; FETCH cur").expect("DECLARE");
    let e = expect_code(c.batch_execute("UPDATE emp SET name = 'x' WHERE CURRENT OF cur"), "24000");
    assert_eq!(message(&e), "cursor \"cur\" is not a simply updatable scan of table \"emp\"");
    c.batch_execute("ROLLBACK; BEGIN; DECLARE cur CURSOR FOR SELECT id FROM emp ORDER BY id FOR UPDATE").expect("DECLARE again");
    let e = expect_code(c.batch_execute("UPDATE emp SET name = 'x' WHERE CURRENT OF cur"), "24000");
    assert_eq!(message(&e), "cursor \"cur\" is not positioned on a row");
    c.batch_execute("ROLLBACK; BEGIN; DECLARE cur CURSOR FOR SELECT id FROM emp ORDER BY id FOR UPDATE").expect("DECLARE again");
    c.batch_execute("FETCH 2 FROM cur").expect("FETCH");
    assert_eq!(rows(&mut c, "UPDATE emp SET name = 'current' WHERE CURRENT OF cur RETURNING id, name"), [["2", "current"]]);
    c.batch_execute("FETCH NEXT FROM cur").expect("FETCH");
    assert_eq!(rows(&mut c, "DELETE FROM emp WHERE CURRENT OF cur RETURNING id"), [["3"]]);
    c.batch_execute("COMMIT").expect("COMMIT");
    assert_eq!(rows(&mut c, "SELECT id, name FROM emp ORDER BY id"), [["1", "ann"], ["2", "current"]]);
}

#[test]
fn row_locks_across_sessions() {
    let (db, mut a) = setup();
    let mut b = db.connect();
    a.batch_execute(
        "CREATE TABLE job (id int PRIMARY KEY, state text NOT NULL DEFAULT 'queued'); INSERT INTO job (id) SELECT generate_series(1, 5)",
    )
    .expect("setup");
    a.batch_execute("BEGIN").expect("BEGIN");
    assert_eq!(ints(&mut a, "SELECT id FROM job WHERE id IN (1, 2) ORDER BY id FOR UPDATE"), [1, 2]);
    assert_eq!(ints(&mut b, "SELECT id FROM job ORDER BY id FOR UPDATE SKIP LOCKED"), [3, 4, 5]);
    assert_eq!(ints(&mut b, "SELECT id FROM job ORDER BY id LIMIT 2 FOR UPDATE SKIP LOCKED"), [3, 4]);
    let e = expect_code(b.batch_execute("SELECT id FROM job WHERE id = 1 FOR UPDATE NOWAIT"), "55P03");
    assert_eq!(message(&e), "could not obtain lock on row in relation \"job\"");
    expect_code(b.batch_execute("SELECT id FROM job WHERE id = 1 FOR KEY SHARE NOWAIT"), "55P03");
    a.batch_execute("COMMIT").expect("COMMIT");
    assert_eq!(ints(&mut b, "SELECT id FROM job WHERE id = 1 FOR UPDATE NOWAIT"), [1]);
    // UPDATE takes FOR NO KEY UPDATE: FOR KEY SHARE is compatible, FOR SHARE is not.
    a.batch_execute("BEGIN; UPDATE job SET state = 'done' WHERE id = 3").expect("UPDATE");
    assert_eq!(ints(&mut b, "SELECT id FROM job WHERE id = 3 FOR KEY SHARE NOWAIT"), [3]);
    expect_code(b.batch_execute("SELECT id FROM job WHERE id = 3 FOR SHARE NOWAIT"), "55P03");
    a.batch_execute("ROLLBACK").expect("ROLLBACK");
    // ROLLBACK TO releases the locks taken after the savepoint.
    a.batch_execute("BEGIN; SAVEPOINT s; SELECT id FROM job WHERE id = 5 FOR UPDATE; ROLLBACK TO s").expect("savepoint");
    assert_eq!(ints(&mut b, "SELECT id FROM job WHERE id = 5 FOR UPDATE NOWAIT"), [5]);
    a.batch_execute("COMMIT").expect("COMMIT");
    // Locking clauses that are not allowed.
    let e = expect_code(a.batch_execute("SELECT count(*) FROM job FOR UPDATE"), "0A000");
    assert_eq!(message(&e), "FOR UPDATE is not allowed with aggregate functions");
    expect_code(a.batch_execute("SELECT DISTINCT state FROM job FOR SHARE"), "0A000");
    expect_code(a.batch_execute("SELECT id FROM job UNION SELECT id FROM job FOR UPDATE"), "0A000");
    // Explicit table locks.
    a.batch_execute("BEGIN; LOCK TABLE job IN SHARE ROW EXCLUSIVE MODE").expect("LOCK");
    b.batch_execute("BEGIN").expect("BEGIN b");
    let e = expect_code(b.batch_execute("LOCK TABLE job IN ROW EXCLUSIVE MODE NOWAIT"), "55P03");
    assert_eq!(message(&e), "could not obtain lock on relation \"job\"");
    b.batch_execute("ROLLBACK; BEGIN; LOCK TABLE job IN ACCESS SHARE MODE NOWAIT; COMMIT").expect("compatible mode");
    a.batch_execute("COMMIT").expect("COMMIT");
}

#[test]
fn repeatable_read_conflicts_fail_at_the_statement() {
    let (db, mut a) = setup();
    let mut b = db.connect();
    a.batch_execute("CREATE TABLE item (id int PRIMARY KEY, n int NOT NULL); INSERT INTO item VALUES (1, 0), (2, 0)").expect("setup");
    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").expect("BEGIN");
    assert_eq!(ints(&mut a, "SELECT n FROM item WHERE id = 1"), [0]);
    b.batch_execute("UPDATE item SET n = n + 1 WHERE id = 1").expect("concurrent UPDATE");
    let e = expect_code(a.batch_execute("UPDATE item SET n = n + 10 WHERE id = 1"), "40001");
    assert_eq!(message(&e), "could not serialize access due to concurrent update");
    a.batch_execute("ROLLBACK").expect("ROLLBACK");
    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").expect("BEGIN");
    assert_eq!(ints(&mut a, "SELECT n FROM item WHERE id = 2"), [0]);
    b.batch_execute("UPDATE item SET n = 7 WHERE id = 2").expect("concurrent UPDATE");
    expect_code(a.batch_execute("SELECT n FROM item WHERE id = 2 FOR UPDATE"), "40001");
    a.batch_execute("ROLLBACK").expect("ROLLBACK");
    // READ COMMITTED sees the latest version instead.
    a.batch_execute("BEGIN").expect("BEGIN");
    assert_eq!(ints(&mut a, "SELECT n FROM item WHERE id = 1"), [1]);
    b.batch_execute("UPDATE item SET n = 100 WHERE id = 1").expect("concurrent UPDATE");
    assert_eq!(ints(&mut a, "UPDATE item SET n = n + 1 WHERE id = 1 RETURNING n"), [101]);
    a.batch_execute("COMMIT").expect("COMMIT");
}

#[test]
fn advisory_locks_across_sessions() {
    let (db, mut a) = setup();
    let mut b = db.connect();
    let one = |c: &mut postgres::Client, sql: &str| -> String { rows(c, sql).concat().concat() };
    assert_eq!(one(&mut a, "SELECT pg_try_advisory_lock(1001)"), "t");
    assert_eq!(one(&mut b, "SELECT pg_try_advisory_lock(1001)"), "f");
    // Re-entrant: two locks need two unlocks.
    assert_eq!(one(&mut a, "SELECT pg_try_advisory_lock(1001)"), "t");
    assert_eq!(one(&mut a, "SELECT pg_advisory_unlock(1001)"), "t");
    assert_eq!(one(&mut b, "SELECT pg_try_advisory_lock(1001)"), "f");
    assert_eq!(one(&mut a, "SELECT pg_advisory_unlock(1001)"), "t");
    assert_eq!(one(&mut b, "SELECT pg_try_advisory_lock(1001)"), "t");
    assert_eq!(one(&mut a, "SELECT pg_advisory_unlock(1001)"), "f");
    // Shared locks share; the bigint and (int, int) key spaces are separate.
    assert_eq!(one(&mut a, "SELECT pg_try_advisory_lock_shared(7)"), "t");
    assert_eq!(one(&mut b, "SELECT pg_try_advisory_lock_shared(7)"), "t");
    assert_eq!(one(&mut b, "SELECT pg_try_advisory_lock(7)"), "f");
    assert_eq!(one(&mut a, "SELECT pg_try_advisory_lock(0, 7)"), "t");
    // Transaction-level locks go with the transaction; session locks survive ROLLBACK.
    a.batch_execute("BEGIN").expect("BEGIN");
    assert_eq!(one(&mut a, "SELECT pg_try_advisory_xact_lock(hashtext('migrations'))"), "t");
    assert_eq!(one(&mut b, "SELECT pg_try_advisory_xact_lock(hashtext('migrations'))"), "f");
    a.batch_execute("ROLLBACK").expect("ROLLBACK");
    assert_eq!(one(&mut b, "SELECT pg_try_advisory_xact_lock(hashtext('migrations'))"), "t");
    assert_eq!(one(&mut a, "SELECT hashtext('migrations')"), "331260518");
    let held = rows(
        &mut a,
        "SELECT classid, objid, objsubid, mode FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid() ORDER BY objsubid, objid",
    );
    assert_eq!(held, [["0", "7", "1", "ShareLock"], ["0", "7", "2", "ExclusiveLock"]]);
    a.batch_execute("SELECT pg_advisory_unlock_all()").expect("unlock all");
    assert_eq!(one(&mut b, "SELECT pg_try_advisory_lock(0, 7)"), "t");
}

#[test]
fn read_only_and_chained_transactions() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY)").expect("CREATE");
    c.batch_execute("BEGIN READ ONLY").expect("BEGIN");
    let e = expect_code(c.batch_execute("INSERT INTO t VALUES (1)"), "25006");
    assert_eq!(message(&e), "cannot execute INSERT in a read-only transaction");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    c.batch_execute("BEGIN; SET TRANSACTION READ ONLY").expect("SET TRANSACTION");
    let e = expect_code(c.batch_execute("CREATE TABLE u (a int)"), "25006");
    assert_eq!(message(&e), "cannot execute CREATE TABLE in a read-only transaction");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    // Temporary tables stay writable.
    c.batch_execute("CREATE TEMP TABLE scratch (a int); BEGIN READ ONLY; INSERT INTO scratch VALUES (1); COMMIT").expect("temp table");
    c.batch_execute("SET default_transaction_read_only = on").expect("SET");
    expect_code(c.batch_execute("UPDATE t SET id = 1"), "25006");
    c.batch_execute("SET default_transaction_read_only = off").expect("SET off");
    // AND CHAIN keeps the characteristics; work before COMMIT AND CHAIN is committed.
    c.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ; INSERT INTO t VALUES (1); COMMIT AND CHAIN").expect("chain");
    assert_eq!(
        rows(&mut c, "SELECT current_setting('transaction_isolation'), current_setting('transaction_read_only')"),
        [["repeatable read", "off"]]
    );
    c.batch_execute("INSERT INTO t VALUES (2); ROLLBACK AND CHAIN; INSERT INTO t VALUES (3); COMMIT").expect("rollback chain");
    assert_eq!(ints(&mut c, "SELECT id FROM t ORDER BY id"), [1, 3]);
    let e = expect_code(c.batch_execute("COMMIT AND CHAIN"), "25P01");
    assert_eq!(message(&e), "COMMIT AND CHAIN can only be used in transaction blocks");
}

#[test]
fn blocking_advisory_lock_waits_for_the_holder() {
    let (db, mut a) = setup();
    a.batch_execute("SELECT pg_advisory_lock(42)").expect("lock");
    let mut b = db.connect();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        b.batch_execute("SELECT pg_advisory_lock(42)").expect("waiting lock");
        tx.send(()).expect("send");
        b.batch_execute("SELECT pg_advisory_unlock(42)").expect("unlock");
    });
    assert!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "the second session must wait");
    a.batch_execute("SELECT pg_advisory_unlock(42)").expect("release");
    rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the waiter gets the lock once it is released");
    waiter.join().expect("waiter");
    // lock_timeout bounds the wait.
    let mut c = db.connect();
    a.batch_execute("SELECT pg_advisory_lock(43)").expect("lock");
    c.batch_execute("SET lock_timeout = '100ms'").expect("SET");
    let e = expect_code(c.batch_execute("SELECT pg_advisory_lock(43)"), "55P03");
    assert_eq!(message(&e), "canceling statement due to lock timeout");
}
