//! `pg_sleep`, `pg_sleep_for` and `pg_sleep_until` block the calling session.

use crate::support::{expect_code, message, setup};
use std::time::{Duration, Instant};

fn timed(c: &mut postgres::Client, sql: &str) -> Duration {
    let start = Instant::now();
    c.simple_query(sql).expect(sql);
    start.elapsed()
}

#[test]
fn pg_sleep_blocks() {
    let (_db, mut c) = setup();
    assert!(timed(&mut c, "SELECT pg_sleep(0.3)") >= Duration::from_millis(300));
    assert!(timed(&mut c, "SELECT pg_sleep_for('0.3 seconds')") >= Duration::from_millis(300));
    assert!(timed(&mut c, "SELECT pg_sleep_until(now() + interval '0.3 seconds')") >= Duration::from_millis(250));
}

#[test]
fn pg_sleep_until_uses_wall_clock_not_transaction_start() {
    let (_db, mut c) = setup();
    c.batch_execute("BEGIN").expect("begin");
    c.simple_query("SELECT pg_sleep(0.3)").expect("sleep");
    // clock_timestamp() has moved on: a deadline 0.2 s after the transaction start is past.
    let d = timed(&mut c, "SELECT pg_sleep_until(now() + interval '0.2 seconds')");
    assert!(d < Duration::from_millis(150), "{d:?}");
    c.batch_execute("ROLLBACK").expect("rollback");
}

#[test]
fn pg_sleep_nonpositive_and_null_return_at_once() {
    let (_db, mut c) = setup();
    for sql in [
        "SELECT pg_sleep(0)",
        "SELECT pg_sleep(-5)",
        "SELECT pg_sleep(NULL)",
        "SELECT pg_sleep_for('-5 seconds')",
        "SELECT pg_sleep_for(NULL)",
        "SELECT pg_sleep_until(now() - interval '1 hour')",
        "SELECT pg_sleep_until(NULL)",
    ] {
        assert!(timed(&mut c, sql) < Duration::from_millis(150), "{sql}");
    }
}

#[test]
fn pg_sleep_result_is_void() {
    let (_db, mut c) = setup();
    let r = c.query_one("SELECT pg_typeof(pg_sleep(0))::text, pg_typeof(pg_sleep_for('0s'))::text, pg_sleep(0)::text", &[]).expect("q");
    assert_eq!(r.get::<_, String>(0), "void");
    assert_eq!(r.get::<_, String>(1), "void");
    assert_eq!(r.get::<_, String>(2), "");
}

#[test]
fn other_sessions_progress_while_one_sleeps() {
    let (db, mut a) = setup();
    let mut b = db.connect();
    let sleeper = std::thread::spawn(move || timed(&mut a, "SELECT pg_sleep(1)"));
    std::thread::sleep(Duration::from_millis(200));
    let start = Instant::now();
    for _ in 0..5 {
        b.simple_query("SELECT 1").expect("select");
    }
    assert!(start.elapsed() < Duration::from_millis(500), "{:?}", start.elapsed());
    assert!(sleeper.join().expect("join") >= Duration::from_secs(1));
}

#[test]
fn pg_sleep_honours_statement_timeout() {
    let (_db, mut c) = setup();
    c.batch_execute("SET statement_timeout = 200").expect("set");
    let start = Instant::now();
    let e = expect_code(c.simple_query("SELECT pg_sleep(5)"), "57014");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(message(&e), "canceling statement due to statement timeout");
}

#[test]
fn pg_sleep_until_counts_from_the_server_clock() {
    let db = crate::support::TestDb::new();
    let Some(server) = db.server() else { return };
    let pinned = crate::support::utc_ts(2025, 6, 15, 12, 0, 0);
    server.set_now(Some(std::time::SystemTime::from(pinned)));
    let mut c = db.connect();
    // The pinned clock does not advance: a deadline 0.3 s ahead of it is a 0.3 s sleep...
    let d = timed(&mut c, "SELECT pg_sleep_until('2025-06-15 12:00:00.3+00')");
    assert!(d >= Duration::from_millis(300), "{d:?}");
    // ...and one before it returns at once, however far ahead of real time it is.
    let d = timed(&mut c, "SELECT pg_sleep_until('2025-06-15 11:00:00+00')");
    assert!(d < Duration::from_millis(150), "{d:?}");
}
