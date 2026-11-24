use crate::support::{Text, expect_code, setup};
use postgres::Client;

fn float_setup() -> (crate::support::TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE measurements (id int PRIMARY KEY, ratio float8 NOT NULL, name text);
         INSERT INTO measurements (id, ratio, name) VALUES (1, 0.5, 'half'), (2, 1.5, 'one-and-half'), (3, 3.14, 'pi')",
    )
    .expect("setup");
    (db, c)
}

#[test]
#[allow(clippy::approx_constant)] // 3.14 is test data, not an approximation of pi.
fn float_roundtrip() {
    let (_db, mut c) = float_setup();
    let rows = c.query("SELECT id, ratio FROM measurements ORDER BY id", &[]).expect("query");
    let got: Vec<f64> = rows.iter().map(|r| r.get(1)).collect();
    assert_eq!(got, vec![0.5, 1.5, 3.14]);
}

#[test]
fn float_comparison() {
    let (_db, mut c) = float_setup();
    let rows = c.query("SELECT id FROM measurements WHERE ratio > 1.0 ORDER BY id", &[]).expect("query");
    let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![2, 3]);
}

#[test]
fn float_arithmetic() {
    let (_db, mut c) = float_setup();
    // Deviation from pgmem-go: 0.1 + 0.2 was float8 ~0.3 vs Postgres numeric, exactly 0.3.
    let got: Text = c.query_one("SELECT 0.1 + 0.2", &[]).expect("query").get(0);
    assert_eq!(got, "0.3");
}

#[test]
fn float_int_plus_float() {
    let (_db, mut c) = float_setup();
    // Deviation from pgmem-go: 1 + 0.5 was float8 vs Postgres numeric.
    let got: Text = c.query_one("SELECT 1 + 0.5", &[]).expect("query").get(0);
    assert_eq!(got, "1.5");
}

#[test]
fn float_double_precision_alias() {
    let (_db, mut c) = float_setup();
    let got: f64 = c.query_one("SELECT $1::double precision + 1.0", &[&2.5f64]).expect("query").get(0);
    assert_eq!(got, 3.5);
}

#[test]
fn float_div_by_zero() {
    let (_db, mut c) = float_setup();
    // Go only asserted an error; Postgres raises division_by_zero for numeric 1.0 / 0.0.
    expect_code(c.query_one("SELECT 1.0 / 0.0", &[]), "22012");
}
