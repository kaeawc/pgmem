use crate::support::{setup, utc_ts};
use jiff::Timestamp;

fn trunc(c: &mut postgres::Client, field: &str, ts: Timestamp) -> Timestamp {
    c.query_one(&format!("SELECT date_trunc('{field}', $1::timestamptz)"), &[&ts]).expect("date_trunc").get(0)
}

#[test]
fn date_trunc_day() {
    let (_db, mut c) = setup();
    assert_eq!(trunc(&mut c, "day", utc_ts(2024, 7, 15, 13, 45, 30)), utc_ts(2024, 7, 15, 0, 0, 0));
}

#[test]
fn date_trunc_month() {
    let (_db, mut c) = setup();
    assert_eq!(trunc(&mut c, "month", utc_ts(2024, 7, 15, 13, 45, 30)), utc_ts(2024, 7, 1, 0, 0, 0));
}

#[test]
fn date_trunc_hour() {
    let (_db, mut c) = setup();
    assert_eq!(trunc(&mut c, "hour", utc_ts(2024, 7, 15, 13, 45, 30)), utc_ts(2024, 7, 15, 13, 0, 0));
}

#[test]
fn date_trunc_week_anchors_to_monday() {
    let (_db, mut c) = setup();
    // 2024-07-15 was a Monday already.
    assert_eq!(trunc(&mut c, "week", utc_ts(2024, 7, 15, 13, 45, 30)), utc_ts(2024, 7, 15, 0, 0, 0));
    // Sunday rolls back to the previous Monday.
    assert_eq!(trunc(&mut c, "week", utc_ts(2024, 7, 21, 12, 0, 0)), utc_ts(2024, 7, 15, 0, 0, 0), "Sunday");
}

#[test]
fn date_trunc_null_propagates() {
    let (_db, mut c) = setup();
    let got: Option<Timestamp> = c.query_one("SELECT date_trunc('day', NULL::timestamptz)", &[]).expect("date_trunc").get(0);
    assert_eq!(got, None);
}
