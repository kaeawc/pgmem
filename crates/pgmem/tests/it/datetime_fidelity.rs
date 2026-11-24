//! Date/time input (Postgres' `ParseDateTime` / `DecodeDateTime`), fractional-second
//! typmods, infinite intervals, OVERLAPS, to_char / to_number / to_date templates and named
//! arguments.

use crate::support::{expect_code, message, setup, text_value, texts};

fn one(c: &mut postgres::Client, sql: &str) -> String {
    text_value(c, sql).unwrap_or_else(|| "NULL".into())
}

#[test]
fn datetime_input_forms() {
    let (_db, mut c) = setup();
    c.batch_execute("SET TimeZone = 'America/New_York'; SET DateStyle = 'ISO, MDY'").expect("set");
    for (input, want) in [
        ("1/2/2024", "2024-01-02"),
        ("15 March 2024", "2024-03-15"),
        ("February 29, 2024", "2024-02-29"),
        ("15-mar-24", "2024-03-15"),
        ("20240315", "2024-03-15"),
        ("2024.075", "2024-03-15"),
        ("J2451545", "2000-01-01"),
        ("Fri Mar 15 2024", "2024-03-15"),
        ("99-01-02", "1999-01-02"),
    ] {
        if input == "99-01-02" {
            // Two-digit years first need YMD order.
            c.batch_execute("SET DateStyle = 'ISO, YMD'").expect("set");
        }
        assert_eq!(one(&mut c, &format!("SELECT '{input}'::date")), want, "{input}");
    }
    c.batch_execute("SET DateStyle = 'ISO, DMY'").expect("set");
    assert_eq!(one(&mut c, "SELECT '1/2/2024'::date"), "2024-02-01");
    c.batch_execute("SET DateStyle = 'ISO, MDY'").expect("set");
    assert_eq!(one(&mut c, "SELECT '2024-03-15 10:00 PM'::timestamp"), "2024-03-15 22:00:00");
    assert_eq!(one(&mut c, "SELECT '20240315T101530'::timestamp"), "2024-03-15 10:15:30");
    // No offset means the session zone; names and abbreviations resolve by date.
    assert_eq!(one(&mut c, "SELECT '2024-07-15 12:00:00'::timestamptz"), "2024-07-15 12:00:00-04");
    assert_eq!(one(&mut c, "SELECT '2024-03-15 12:00 America/Los_Angeles'::timestamptz"), "2024-03-15 15:00:00-04");
    assert_eq!(one(&mut c, "SELECT '2024-03-15 10:00:00 EST'::timestamptz"), "2024-03-15 11:00:00-04");
    assert_eq!(one(&mut c, "SELECT '2024-03-10 02:30:00'::timestamptz"), "2024-03-10 03:30:00-04");
    assert_eq!(one(&mut c, "SELECT '10:00 PST'::timetz"), "10:00:00-08");
    assert_eq!(one(&mut c, "SELECT 'allballs'::time"), "00:00:00");

    let e = expect_code(c.simple_query("SELECT '2023-02-29'::date"), "22008");
    assert_eq!(message(&e), "date/time field value out of range: \"2023-02-29\"");
    expect_code(c.simple_query("SELECT '2024-13-01'::date"), "22008");
    expect_code(c.simple_query("SELECT '2024-03-15 25:00'::timestamp"), "22008");
    let e = expect_code(c.simple_query("SELECT '2024-03-15 10:00 Mars/Phobos'::timestamp"), "22023");
    assert_eq!(message(&e), "time zone \"mars/phobos\" not recognized");
    expect_code(c.simple_query("SELECT '2024-03-15 10:00:00+16'::timestamptz"), "22009");
    expect_code(c.simple_query("SELECT 'abc'::date"), "22007");
}

#[test]
fn typmods_infinity_and_overlaps() {
    let (_db, mut c) = setup();
    c.batch_execute("SET TimeZone = 'UTC'").expect("set");
    assert_eq!(
        texts(&mut c, "SELECT '12:34:56.789'::time(1), '2024-06-15 13:45:30.555'::timestamp(2), interval '1.5555 seconds'::interval(2)"),
        vec![vec![Some("12:34:56.8".into()), Some("2024-06-15 13:45:30.56".into()), Some("00:00:01.56".into())]]
    );
    c.batch_execute("CREATE TABLE ev (ts timestamp(3)); INSERT INTO ev VALUES ('1999-12-31 23:59:59.9999')").expect("ddl");
    assert_eq!(one(&mut c, "SELECT ts FROM ev"), "2000-01-01 00:00:00");

    assert_eq!(
        texts(
            &mut c,
            "SELECT 'infinity'::interval, 'infinity'::timestamp - '2024-01-01'::timestamp, timestamp '2024-01-01' + interval '-infinity', isfinite(interval 'infinity')"
        ),
        vec![vec![Some("infinity".into()), Some("infinity".into()), Some("-infinity".into()), Some("f".into())]]
    );
    expect_code(c.simple_query("SELECT 'infinity'::timestamp - 'infinity'::timestamp"), "22008");
    assert_eq!(one(&mut c, "SELECT justify_interval(interval '1 mon -1 hour')"), "29 days 23:00:00");

    assert_eq!(one(&mut c, "SELECT (date '2024-03-15', date '2024-03-20') OVERLAPS (date '2024-03-18', date '2024-03-25')"), "t");
    assert_eq!(one(&mut c, "SELECT (date '2024-01-01', date '2024-01-05') OVERLAPS (date '2024-01-05', date '2024-01-06')"), "f");
    assert_eq!(
        one(&mut c, "SELECT (timestamp '2024-01-01', interval '1 day') OVERLAPS (timestamp '2024-01-01 12:00', interval '1 hour')"),
        "t"
    );
    assert_eq!(one(&mut c, "SELECT (timestamp '2024-01-02', timestamp '2024-01-01') OVERLAPS (timestamp '2024-01-01 06:00', NULL)"), "t");

    assert_eq!(
        texts(
            &mut c,
            "SELECT make_interval(days => 10), make_interval(hours => -3, mins => 30), make_date(year => 2024, month => 1, day => 2)"
        ),
        vec![vec![Some("10 days".into()), Some("-02:30:00".into()), Some("2024-01-02".into())]]
    );
}

#[test]
fn to_char_to_number_and_templates() {
    let (_db, mut c) = setup();
    c.batch_execute("SET TimeZone = 'UTC'").expect("set");
    for (expr, want) in [
        ("to_char(-125.8, '999D99S')", "125.80-"),
        ("to_char(1234567.891, '9,999,999.99')", " 1,234,567.89"),
        ("to_char(12, '0009')", " 0012"),
        ("to_char(485, 'RN')", "        CDLXXXV"),
        ("to_char(1234.5::float8, '99999.999')", "  1234.500"),
        ("to_char(12345, '99')", " ##"),
        ("to_char(42, 'PL999')", "+  42"),
        ("to_char(3, 'FM9th')", "3rd"),
        ("to_char(-42, '999PR')", " <42>"),
        ("to_char(5.2, 'FM9990.00')", "5.20"),
        ("to_char(1234.5, '9.99EEEE')", " 1.23e+03"),
        ("to_char(12, '99V99')", " 1200"),
    ] {
        assert_eq!(one(&mut c, &format!("SELECT {expr}")), want, "{expr}");
    }
    assert_eq!(one(&mut c, "SELECT to_number('12,454.8-', '99G999D9S')"), "-12454.8");
    assert_eq!(one(&mut c, "SELECT to_number('1,234.50', '9G999D99')"), "1234.50");
    let e = expect_code(c.simple_query("SELECT to_char(1, '9D9D9')"), "42601");
    assert_eq!(message(&e), "multiple decimal points");

    assert_eq!(one(&mut c, "SELECT to_date('2024-075', 'YYYY-DDD')"), "2024-03-15");
    assert_eq!(one(&mut c, "SELECT to_date('Friday 15th March 2024', 'Day DDth Month YYYY')"), "2024-03-15");
    assert_eq!(one(&mut c, "SELECT to_date('15/03/89', 'DD/MM/YY')"), "1989-03-15");
    assert_eq!(one(&mut c, "SELECT to_timestamp('2024-03-15 14:30 +05', 'YYYY-MM-DD HH24:MI TZH')"), "2024-03-15 09:30:00+00");
    assert_eq!(one(&mut c, "SELECT to_timestamp('2024-07-05 2:03:09 PM', 'YYYY-MM-DD HH:MI:SS AM')"), "2024-07-05 14:03:09+00");
    let e = expect_code(c.simple_query("SELECT to_timestamp('13:00', 'HH:MI')"), "22007");
    assert_eq!(message(&e), "hour \"13\" is invalid for the 12-hour clock");
    expect_code(c.simple_query("SELECT to_date('2024-02-30', 'YYYY-MM-DD')"), "22008");
}
