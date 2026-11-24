use crate::support::{Interval, setup};

const HOUR: i64 = 3_600_000_000;

fn interval(sql: &str) -> Interval {
    let (_db, mut c) = setup();
    c.query_one(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).get(0)
}

#[test]
fn interval_codec_age_round_trip() {
    let got = interval("SELECT age(make_timestamp(2026, 1, 2, 0, 0, 0), make_timestamp(2026, 1, 1, 0, 0, 0))");
    assert_eq!(got, Interval { months: 0, days: 1, micros: 0 });
    assert_eq!(got.total_micros(), 24 * HOUR);
}

#[test]
fn interval_codec_literal() {
    let got = interval("SELECT interval '2 hours 30 minutes'");
    assert_eq!(got, Interval { months: 0, days: 0, micros: 2 * HOUR + 30 * 60_000_000 });
}

#[test]
fn interval_codec_sub_second() {
    let got = interval("SELECT age(make_timestamp(2026, 1, 1, 0, 0, 1), make_timestamp(2026, 1, 1, 0, 0, 0))");
    assert_eq!(got, Interval { months: 0, days: 0, micros: 1_000_000 });
}

#[test]
fn interval_codec_negative() {
    let got = interval("SELECT age(make_timestamp(2026, 1, 1, 0, 0, 0), make_timestamp(2026, 1, 2, 0, 0, 0))");
    assert_eq!(got, Interval { months: 0, days: -1, micros: 0 });
    assert_eq!(got.total_micros(), -24 * HOUR);
}
