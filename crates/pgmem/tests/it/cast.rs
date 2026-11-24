use crate::support::{expect_code, message, setup};

#[test]
fn cast_int_to_text() {
    let (_db, mut c) = setup();
    let got: String = c.query_one("SELECT 42::text", &[]).expect("select").get(0);
    assert_eq!(got, "42");
}

#[test]
fn cast_text_to_int() {
    let (_db, mut c) = setup();
    let got: i32 = c.query_one("SELECT '42'::int", &[]).expect("select").get(0);
    assert_eq!(got, 42);
}

#[test]
fn cast_chains() {
    let (_db, mut c) = setup();
    let got: String = c.query_one("SELECT '7'::int::text", &[]).expect("select").get(0);
    assert_eq!(got, "7");
}

#[test]
fn cast_null_propagates() {
    let (_db, mut c) = setup();
    let got: Option<String> = c.query_one("SELECT NULL::text", &[]).expect("select").get(0);
    assert_eq!(got, None);
}

#[test]
fn cast_int4_to_int8() {
    let (_db, mut c) = setup();
    let got: i64 = c.query_one("SELECT 1::bigint", &[]).expect("select").get(0);
    assert_eq!(got, 1);
}

#[test]
fn cast_bad_int_text() {
    let (_db, mut c) = setup();
    // Go accepted any server error (its comment mentions XX000); Postgres reports invalid_text_representation.
    let e = expect_code(c.execute("SELECT 'notanint'::int", &[]), "22P02");
    assert_eq!(message(&e), r#"invalid input syntax for type integer: "notanint""#);
}

#[test]
fn cast_timestamptz_text() {
    let (db, mut c) = setup();
    // The text form of a timestamptz, checked on both pgmem and real Postgres.
    let got: String = c.query_one("SELECT '2025-01-02 03:04:05Z'::timestamptz::text", &[]).expect("select").get(0);
    assert_eq!(got, "2025-01-02 03:04:05+00");
    let Some(server) = db.server() else { return };
    let pinned = "2025-01-02T03:04:05Z".parse::<jiff::Timestamp>().expect("parse");
    server.set_now(Some(std::time::SystemTime::from(pinned)));
    let got: String = c.query_one("SELECT now()::text", &[]).expect("select").get(0);
    assert_eq!(got, "2025-01-02 03:04:05+00");
}
