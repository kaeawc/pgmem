//! Port of exec/funcs_test.go as SQL-level tests.

use crate::support::{expect_code, setup};
use uuid::Uuid;

#[test]
fn gen_random_uuid_produces_v4() {
    let (_db, mut c) = setup();
    let u: Uuid = c.query_one("SELECT gen_random_uuid() AS u", &[]).expect("gen_random_uuid").get(0);
    let b = u.as_bytes();
    assert_eq!(b[6] >> 4, 0x4, "version nibble");
    assert_eq!(b[8] >> 6, 0b10, "variant bits");
}

#[test]
fn gen_random_uuid_two_calls_differ() {
    let (_db, mut c) = setup();
    let mut get = || -> Uuid { c.query_one("SELECT gen_random_uuid() AS u", &[]).expect("gen_random_uuid").get(0) };
    let (a, b) = (get(), get());
    assert_ne!(a, b);
}

#[test]
fn unknown_function_errors() {
    let (_db, mut c) = setup();
    // Postgres rejects an unknown function at parse analysis with undefined_function.
    expect_code(c.query("SELECT no_such_function() AS x", &[]), "42883");
}

#[test]
fn uuid_column_round_trip_through_insert() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE rows (id uuid NOT NULL UNIQUE, label text)").expect("create");
    let want = Uuid::from_bytes([0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    c.execute("INSERT INTO rows (id, label) VALUES ($1, $2)", &[&want, &"x"]).expect("insert");
    let rows = c.query("SELECT id FROM rows", &[]).expect("select");
    let got: Vec<Uuid> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![want]);
}
