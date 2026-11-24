//! The session TimeZone: output, casts, AT TIME ZONE, interval arithmetic across DST, and the
//! date/time functions that work in local time.

use crate::support::{expect_code, setup, text_value, texts};

fn one(c: &mut postgres::Client, sql: &str) -> String {
    text_value(c, sql).unwrap_or_else(|| "NULL".into())
}

#[test]
fn set_time_zone_forms() {
    let (_db, mut c) = setup();
    c.batch_execute("SET TIME ZONE 'america/new_york'").expect("set");
    assert_eq!(one(&mut c, "SHOW TimeZone"), "America/New_York");
    c.batch_execute("SET TIME ZONE 5").expect("set");
    assert_eq!(one(&mut c, "SHOW TimeZone"), "<+05>-05");
    c.batch_execute("SET TIME ZONE INTERVAL '+05:30' HOUR TO MINUTE").expect("set");
    assert_eq!(one(&mut c, "SHOW TimeZone"), "<+05:30>-05:30");
    c.batch_execute("SET timezone = 'UTC+3'").expect("set");
    assert_eq!(one(&mut c, "SELECT timestamptz '2024-01-01 12:00:00+00'"), "2024-01-01 09:00:00-03");
    let e = expect_code(c.simple_query("SET TIME ZONE 'Nowhere/Land'"), "22023");
    assert_eq!(
        e.as_db_error().map(|d| d.message().to_owned()).as_deref(),
        Some("invalid value for parameter \"TimeZone\": \"Nowhere/Land\"")
    );
}

#[test]
fn output_and_casts_use_the_session_zone() {
    let (_db, mut c) = setup();
    c.batch_execute("SET TIME ZONE 'America/New_York'").expect("set");
    assert_eq!(
        texts(
            &mut c,
            "SELECT timestamptz '2024-07-01 12:00:00+00', timestamptz '2024-01-01 12:00:00+00', \
                    (timestamptz '2024-07-01 02:00:00+00')::date, (timestamptz '2024-07-01 12:00:00+00')::timestamp, \
                    (timestamp '2024-07-01 12:00:00')::timestamptz, timestamptz '1883-01-01 00:00:00+00'"
        ),
        vec![vec![
            Some("2024-07-01 08:00:00-04".to_owned()),
            Some("2024-01-01 07:00:00-05".to_owned()),
            Some("2024-06-30".to_owned()),
            Some("2024-07-01 08:00:00".to_owned()),
            Some("2024-07-01 12:00:00-04".to_owned()),
            Some("1882-12-31 19:03:58-04:56:02".to_owned()),
        ]]
    );
}

#[test]
fn at_time_zone() {
    let (_db, mut c) = setup();
    assert_eq!(
        texts(
            &mut c,
            "SELECT timestamptz '2024-03-15 12:00:00+00' AT TIME ZONE 'America/New_York', \
                    timestamp '2024-07-01 12:00:00' AT TIME ZONE 'Europe/Paris', \
                    timestamptz '2024-03-15 12:00:00+00' AT TIME ZONE 'EST', \
                    timestamptz '2024-03-15 12:00:00+00' AT TIME ZONE '+05', \
                    timestamptz '2024-03-15 12:00:00+00' AT TIME ZONE INTERVAL '-03:30', \
                    timestamp '2024-03-10 02:30:00' AT TIME ZONE 'America/New_York', \
                    timestamp '2024-11-03 01:30:00' AT TIME ZONE 'America/New_York'"
        ),
        vec![vec![
            Some("2024-03-15 08:00:00".to_owned()),
            Some("2024-07-01 10:00:00+00".to_owned()),
            Some("2024-03-15 07:00:00".to_owned()),
            Some("2024-03-15 07:00:00".to_owned()),
            Some("2024-03-15 08:30:00".to_owned()),
            Some("2024-03-10 07:30:00+00".to_owned()),
            Some("2024-11-03 06:30:00+00".to_owned()),
        ]]
    );
    let names: Vec<String> =
        c.prepare("SELECT now() AT TIME ZONE 'UTC'").expect("prepare").columns().iter().map(|c| c.name().to_owned()).collect();
    assert_eq!(names, vec!["timezone"]);
    expect_code(c.simple_query("SELECT now() AT TIME ZONE 'Nowhere/Land'"), "22023");
}

#[test]
fn interval_arithmetic_and_functions_work_in_local_time() {
    let (_db, mut c) = setup();
    c.batch_execute("SET TIME ZONE 'America/New_York'").expect("set");
    assert_eq!(
        texts(
            &mut c,
            "SELECT timestamptz '2024-03-09 17:00:00+00' + interval '1 day', \
                    timestamptz '2024-03-09 17:00:00+00' + interval '24 hours', \
                    date_trunc('day', timestamptz '2024-07-01 02:00:00+00'), \
                    date_trunc('day', timestamptz '2024-07-01 02:00:00+00', 'Asia/Tokyo'), \
                    extract(hour FROM timestamptz '2024-07-01 12:00:00+00'), \
                    extract(timezone FROM timestamptz '2024-07-01 12:00:00+00'), \
                    to_char(timestamptz '2024-01-01 10:00:00+00', 'HH24:MI TZ OF'), \
                    make_timestamptz(2024, 7, 1, 12, 0, 0), \
                    age(timestamptz '2024-03-11 04:00:00+00', timestamptz '2024-03-10 05:00:00+00')"
        ),
        vec![vec![
            Some("2024-03-10 12:00:00-04".to_owned()),
            Some("2024-03-10 13:00:00-04".to_owned()),
            Some("2024-06-30 00:00:00-04".to_owned()),
            Some("2024-06-30 11:00:00-04".to_owned()),
            Some("8".to_owned()),
            Some("-14400".to_owned()),
            Some("05:00 EST -05".to_owned()),
            Some("2024-07-01 12:00:00-04".to_owned()),
            Some("1 day".to_owned()),
        ]]
    );
    assert_eq!(
        texts(
            &mut c,
            "SELECT * FROM generate_series(timestamptz '2024-03-09 05:00:00+00', timestamptz '2024-03-11 04:00:00+00', interval '1 day')"
        ),
        vec![
            vec![Some("2024-03-09 00:00:00-05".to_owned())],
            vec![Some("2024-03-10 00:00:00-05".to_owned())],
            vec![Some("2024-03-11 00:00:00-04".to_owned())]
        ]
    );
}

#[test]
fn date_bin() {
    let (_db, mut c) = setup();
    assert_eq!(
        one(&mut c, "SELECT date_bin('15 minutes', timestamp '2020-02-11 15:44:17', timestamp '2001-01-01 00:02:30')"),
        "2020-02-11 15:32:30"
    );
    expect_code(c.simple_query("SELECT date_bin('1 month', timestamp '2024-03-15', timestamp '2024-01-01')"), "0A000");
}
