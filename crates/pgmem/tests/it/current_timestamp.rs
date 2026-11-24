use crate::support::{TestDb, Text, setup, utc_ts};
use jiff::Timestamp;
use jiff::civil::{Date, date};
use jiff::tz::TimeZone;

/// Pins pgmem's clock to 2024-07-15 12:30:45 UTC and returns that instant; None on real Postgres,
/// whose clock cannot be pinned (the tests then check the value against the wall clock instead).
fn pin(db: &TestDb) -> Option<Timestamp> {
    let pinned = utc_ts(2024, 7, 15, 12, 30, 45);
    db.server().map(|server| {
        server.set_now(Some(pinned.into()));
        pinned
    })
}

fn assert_near_wall_clock(got: Timestamp) {
    let now = Timestamp::now();
    assert!(now.duration_since(got).abs().as_secs() < 60, "{got} is not near the wall clock {now}");
}

#[test]
fn current_timestamp_no_parens() {
    let (db, mut c) = setup();
    let pinned = pin(&db);
    let got: Timestamp = c.query_one("SELECT current_timestamp", &[]).expect("query").get(0);
    match pinned {
        Some(want) => assert_eq!(got, want),
        None => assert_near_wall_clock(got),
    }
}

#[test]
fn current_date_truncated_to_midnight() {
    let (db, mut c) = setup();
    let pinned = pin(&db);
    // Deviation from pgmem-go: current_date was a timestamptz at midnight vs Postgres `date`.
    let got: Date = c.query_one("SELECT current_date", &[]).expect("query").get(0);
    match pinned {
        Some(_) => assert_eq!(got, date(2024, 7, 15)),
        None => assert_eq!(got, Timestamp::now().to_zoned(TimeZone::UTC).date()),
    }
}

#[test]
fn current_time_date_at_epoch() {
    let (db, mut c) = setup();
    let pinned = pin(&db);
    // Deviation from pgmem-go: current_time was a timestamptz on 1970-01-01 vs Postgres `timetz`.
    let got: Text = c.query_one("SELECT current_time", &[]).expect("query").get(0);
    match pinned {
        Some(_) => assert_eq!(got, "12:30:45+00"),
        None => assert!(got.0.ends_with("+00"), "current_time {got:?} should be in the session time zone (UTC)"),
    }
}

#[test]
fn current_timestamp_as_default() {
    let (db, mut c) = setup();
    let pinned = pin(&db);
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, created timestamptz NOT NULL)").expect("create");
    c.execute("INSERT INTO t (id, created) VALUES (1, current_timestamp)", &[]).expect("insert");
    let got: Timestamp = c.query_one("SELECT created FROM t WHERE id = 1", &[]).expect("readback").get(0);
    match pinned {
        Some(want) => assert_eq!(got, want),
        None => assert_near_wall_clock(got),
    }
}
