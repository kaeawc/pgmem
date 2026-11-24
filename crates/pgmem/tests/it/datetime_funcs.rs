use crate::support::{Interval, expect_code, setup, utc_ts};
use jiff::Timestamp;
use jiff::civil::{Date, DateTime, Time, date, time};

fn naive(y: i16, mo: i8, d: i8, h: i8, mi: i8, s: i8) -> DateTime {
    date(y, mo, d).at(h, mi, s, 0)
}

#[test]
fn make_date() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: scanned as a timestamptz vs Postgres returning `date`.
    let got: Date = c.query_one("SELECT make_date(2026, 5, 4)", &[]).expect("make_date").get(0);
    assert_eq!(got, date(2026, 5, 4));
}

#[test]
fn make_time() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: scanned as a timestamptz vs Postgres returning `time`.
    let got: Time = c.query_one("SELECT make_time(13, 45, 30)", &[]).expect("make_time").get(0);
    assert_eq!(got, time(13, 45, 30, 0));
}

#[test]
fn make_timestamp() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: scanned as a timestamptz vs Postgres returning `timestamp`.
    let got: DateTime = c.query_one("SELECT make_timestamp(2026, 5, 4, 12, 0, 0)", &[]).expect("make_timestamp").get(0);
    assert_eq!(got, naive(2026, 5, 4, 12, 0, 0));
}

#[test]
fn to_timestamp_unix_seconds() {
    let (_db, mut c) = setup();
    let got: Timestamp = c.query_one("SELECT to_timestamp(1735689600)", &[]).expect("to_timestamp").get(0);
    // 1735689600 = 2025-01-01 00:00:00 UTC
    assert_eq!(got, utc_ts(2025, 1, 1, 0, 0, 0));
}

#[test]
fn age_two_args() {
    let (_db, mut c) = setup();
    // age(t1, t2) composes with t2: t2 + age(t1, t2) = t1.
    // Deviation from pgmem-go: result scanned as a timestamptz vs Postgres returning `timestamp`.
    let got: DateTime = c
        .query_one(
            "SELECT make_timestamp(2026, 1, 1, 0, 0, 0) + age(make_timestamp(2026, 1, 2, 0, 0, 0), make_timestamp(2026, 1, 1, 0, 0, 0))",
            &[],
        )
        .expect("age")
        .get(0);
    assert_eq!(got, naive(2026, 1, 2, 0, 0, 0));
    // The interval itself is now encodable over the wire: exactly one day.
    let age: Interval =
        c.query_one("SELECT age(make_timestamp(2026, 1, 2, 0, 0, 0), make_timestamp(2026, 1, 1, 0, 0, 0))", &[]).expect("age").get(0);
    assert_eq!(age, Interval { months: 0, days: 1, micros: 0 });
}

#[test]
fn age_one_arg() {
    let (_db, mut c) = setup();
    // One-arg age(t) is age(current_date, t), i.e. current_date - t symbolically.
    // Deviation from pgmem-go: `current_date + age(t)` = t vs Postgres, where age(t) is positive for a past t, so
    // `current_date - age(t)` = t (exact here since t is the first of a month, so no day borrowing occurs).
    let got: DateTime = c.query_one("SELECT current_date - age(make_timestamp(2020, 1, 1, 0, 0, 0))", &[]).expect("age").get(0);
    assert_eq!(got, naive(2020, 1, 1, 0, 0, 0));
    let positive: bool = c.query_one("SELECT age(make_timestamp(2020, 1, 1, 0, 0, 0)) > interval '0'", &[]).expect("age sign").get(0);
    assert!(positive, "age of a past timestamp must be positive");
}

#[test]
fn make_timestamp_bad_arity() {
    let (_db, mut c) = setup();
    // undefined_function: no make_timestamp(integer, integer, integer)
    expect_code(c.query_one("SELECT make_timestamp(2026, 5, 4)", &[]), "42883");
}
