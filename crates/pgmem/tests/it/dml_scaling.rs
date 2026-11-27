//! Per-statement cost of single-row DML must not grow with the table or with the
//! transaction's pending changes (#218): test suites insert thousands of rows one by one.
//!
//! Each test times a batch of statements early on and the same batch after many more rows
//! were written, and bounds the ratio. Quadratic behaviour makes the late batch ~10x+ slower;
//! the bound leaves room for noise on slow CI machines (and for debug builds). Every batch is
//! measured several times and the medians compared. These test pgmem's storage, so they are
//! skipped against real PostgreSQL (`PGMEM_TEST_DSN`).

use crate::support::setup;
use postgres::Client;
use std::time::{Duration, Instant};

/// Rows written between the two timed batches.
const GAP: i32 = 10_000;
/// Statements per timed batch.
const BATCH: i32 = 500;
/// Allowed slowdown of the late batch relative to the early one.
const MAX_RATIO: f64 = 5.0;
/// Repetitions of each timed batch (the median is used).
const REPS: i32 = 5;

fn skip() -> bool {
    std::env::var_os("PGMEM_TEST_DSN").is_some()
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

const SCHEMA: &str = "CREATE TABLE parent (id int4 PRIMARY KEY);
    INSERT INTO parent VALUES (1);
    CREATE TABLE t (id int4 PRIMARY KEY, email text UNIQUE, parent_id int4 REFERENCES parent, note text)";

fn insert_sql(i: i32) -> String {
    format!("INSERT INTO t (id, email, parent_id, note) VALUES ({i}, 'u{i}@example.com', 1, 'row {i}')")
}

fn insert_range(c: &mut Client, from: i32, to: i32) {
    for i in from..to {
        c.execute(&insert_sql(i), &[]).expect("insert");
    }
}

/// Median time of `REPS` batches of `BATCH` statements; batch `r` covers ids
/// `base + r * BATCH ..`, with `sql` building the statement for an id.
fn timed(c: &mut Client, base: i32, sql: impl Fn(i32) -> String) -> Duration {
    let times = (0..REPS)
        .map(|r| {
            let start = Instant::now();
            for i in base + r * BATCH..base + (r + 1) * BATCH {
                c.execute(&sql(i), &[]).expect("statement");
            }
            start.elapsed()
        })
        .collect();
    median(times)
}

fn assert_flat(what: &str, early: Duration, late: Duration) {
    let ratio = late.as_secs_f64() / early.as_secs_f64().max(1e-6);
    eprintln!("{what}: early {early:?}, late {late:?}, ratio {ratio:.2}");
    assert!(ratio < MAX_RATIO, "{what}: {BATCH} statements took {early:?} early and {late:?} after {GAP} more rows ({ratio:.1}x)");
}

const WARM: i32 = REPS * BATCH;

fn insert_flat(c: &mut Client) -> (Duration, Duration) {
    insert_range(c, 0, BATCH); // warm-up
    let early = timed(c, BATCH, insert_sql);
    insert_range(c, BATCH + WARM, BATCH + WARM + GAP);
    let late = timed(c, BATCH + WARM + GAP, insert_sql);
    (early, late)
}

#[test]
fn single_row_inserts_in_one_transaction_stay_flat() {
    if skip() {
        return;
    }
    let (_db, mut c) = setup();
    c.batch_execute(SCHEMA).expect("schema");
    c.batch_execute("BEGIN").expect("begin");
    let (early, late) = insert_flat(&mut c);
    c.batch_execute("COMMIT").expect("commit");
    let n: i64 = c.query_one("SELECT count(*) FROM t", &[]).expect("count").get(0);
    assert_eq!(n, i64::from(BATCH + 2 * WARM + GAP));
    assert_flat("insert in one transaction", early, late);
}

#[test]
fn autocommit_single_row_inserts_stay_flat() {
    if skip() {
        return;
    }
    let (_db, mut c) = setup();
    c.batch_execute(SCHEMA).expect("schema");
    let (early, late) = insert_flat(&mut c);
    assert_flat("autocommit insert", early, late);
}

#[test]
fn update_and_delete_by_primary_key_in_one_transaction_stay_flat() {
    if skip() {
        return;
    }
    let (_db, mut c) = setup();
    c.batch_execute(SCHEMA).expect("schema");
    c.batch_execute("BEGIN").expect("begin");
    // Layout: [0, WARM) early update, [WARM, 2*WARM) early delete, then the gap, then late ones.
    insert_range(&mut c, 0, 2 * WARM);
    let upd = |i: i32| format!("UPDATE t SET note = 'changed' WHERE id = {i}");
    let del = |i: i32| format!("DELETE FROM t WHERE id = {i}");
    let early_upd = timed(&mut c, 0, upd);
    let early_del = timed(&mut c, WARM, del);
    insert_range(&mut c, 2 * WARM, 2 * WARM + GAP + 2 * WARM);
    let late_upd = timed(&mut c, 2 * WARM + GAP, upd);
    let late_del = timed(&mut c, 3 * WARM + GAP, del);
    c.batch_execute("COMMIT").expect("commit");
    let n: i64 = c.query_one("SELECT count(*) FROM t", &[]).expect("count").get(0);
    assert_eq!(n, i64::from(2 * WARM + GAP));
    assert_flat("update by primary key", early_upd, late_upd);
    assert_flat("delete by primary key", early_del, late_del);
}

/// Bulk UPDATE / DELETE of many pending rows sharing one low-cardinality key value must not
/// be quadratic in the bucket size (generous wall-clock bound; the old `Vec` bucket needed
/// minutes at this size in debug builds).
#[test]
fn bulk_update_and_delete_on_low_cardinality_key_stay_fast() {
    if skip() {
        return;
    }
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE q (id int4 PRIMARY KEY, status text, n int4)").expect("schema");
    c.batch_execute("CREATE INDEX q_status ON q (status)").expect("index");
    c.batch_execute("BEGIN").expect("begin");
    let n = 10_000;
    c.batch_execute(&format!("INSERT INTO q SELECT g, 'new', 0 FROM generate_series(1, {n}) g")).expect("insert");
    let start = Instant::now();
    let upd = c.execute("UPDATE q SET n = 1 WHERE status = 'new'", &[]).expect("update");
    let del = c.execute("DELETE FROM q WHERE status = 'new'", &[]).expect("delete");
    let took = start.elapsed();
    c.batch_execute("COMMIT").expect("commit");
    assert_eq!((upd, del), (n as u64, n as u64));
    assert!(took < Duration::from_secs(30), "bulk update + delete of {n} rows took {took:?}");
}
