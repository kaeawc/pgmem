use crate::support::{setup, utc_ts};
use jiff::{SignedDuration, Timestamp};

#[test]
fn timestamptz_now_round_trip() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE events (id int PRIMARY KEY, at timestamptz NOT NULL)").expect("create");
    let before = Timestamp::now();
    c.execute("INSERT INTO events (id, at) VALUES (1, now())", &[]).expect("insert");
    let after = Timestamp::now();
    let got: Timestamp = c.query_one("SELECT at FROM events WHERE id = 1", &[]).expect("readback").get(0);
    // Postgres keeps microseconds; compare against `before` truncated to the same precision.
    let before = before - SignedDuration::from_micros(1);
    assert!(got >= before && got <= after + SignedDuration::from_secs(1), "at: {got} not in [{before}, {after}]");
}

#[test]
fn timestamptz_accepts_explicit_value() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE events (id int PRIMARY KEY, at timestamptz NOT NULL)").expect("create");
    let want = utc_ts(2025, 1, 15, 14, 30, 45);
    c.execute("INSERT INTO events (id, at) VALUES ($1, $2)", &[&1i32, &want]).expect("insert");
    let got: Timestamp = c.query_one("SELECT at FROM events WHERE id = 1", &[]).expect("readback").get(0);
    assert_eq!(got, want);
}

#[test]
fn timestamptz_order_by_works() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE events (id int PRIMARY KEY, at timestamptz NOT NULL)").expect("create");
    let (t1, t2, t3) = (utc_ts(2025, 1, 1, 0, 0, 0), utc_ts(2025, 6, 1, 0, 0, 0), utc_ts(2025, 12, 1, 0, 0, 0));
    for (i, ts) in (1i32..).zip([t3, t1, t2]) {
        c.execute("INSERT INTO events (id, at) VALUES ($1, $2)", &[&i, &ts]).unwrap_or_else(|e| panic!("insert {i}: {e}"));
    }
    let rows = c.query("SELECT at FROM events ORDER BY at ASC", &[]).expect("select");
    let got: Vec<Timestamp> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![t1, t2, t3]);
}
