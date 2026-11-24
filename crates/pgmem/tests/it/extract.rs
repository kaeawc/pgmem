use crate::support::{Text, expect_code, setup, utc_ts};
use jiff::Timestamp;

// extract() returns numeric (pgx scanned it into int64); read the text form.

#[test]
fn extract_year_month_day() {
    let (_db, mut c) = setup();
    let row = c
        .query_one(
            "SELECT extract(year from $1::timestamptz), extract(month from $1::timestamptz), extract(day from $1::timestamptz)",
            &[&utc_ts(2024, 7, 15, 13, 45, 0)],
        )
        .expect("extract");
    let got: (Text, Text, Text) = (row.get(0), row.get(1), row.get(2));
    assert_eq!(got, (Text("2024".into()), Text("7".into()), Text("15".into())));
}

#[test]
fn extract_hour_minute_second() {
    let (_db, mut c) = setup();
    let row = c
        .query_one(
            "SELECT extract(hour from $1::timestamptz), extract(minute from $1::timestamptz), extract(second from $1::timestamptz)",
            &[&utc_ts(2024, 7, 15, 13, 45, 30)],
        )
        .expect("extract");
    let got: (Text, Text, Text) = (row.get(0), row.get(1), row.get(2));
    // second carries microsecond scale (numeric(.., 6)).
    assert_eq!(got, (Text("13".into()), Text("45".into()), Text("30.000000".into())));
}

#[test]
fn extract_epoch() {
    let (_db, mut c) = setup();
    let ts = utc_ts(2024, 1, 1, 0, 0, 0);
    let got: Text = c.query_one("SELECT extract(epoch from $1::timestamptz)", &[&ts]).expect("extract").get(0);
    assert_eq!(got.0, format!("{}.000000", ts.as_second()));
}

#[test]
fn date_part_function_form() {
    let (_db, mut c) = setup();
    let got: f64 = c.query_one("SELECT date_part('year', $1::timestamptz)", &[&utc_ts(2024, 7, 15, 0, 0, 0)]).expect("date_part").get(0);
    assert_eq!(got, 2024.0);
}

#[test]
fn extract_null_propagates() {
    let (_db, mut c) = setup();
    let got: Option<Text> = c.query_one("SELECT extract(year from NULL::timestamptz)", &[]).expect("extract").get(0);
    assert_eq!(got, None);
}

#[test]
fn extract_unknown_field_errors() {
    let (_db, mut c) = setup();
    let e = expect_code(c.query_one("SELECT extract(nonsense from $1::timestamptz)", &[&Timestamp::now()]), "22023");
    assert_eq!(crate::support::message(&e), r#"unit "nonsense" not recognized for type timestamp with time zone"#);
}
