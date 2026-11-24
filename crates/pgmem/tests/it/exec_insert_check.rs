//! Port of exec/insert_check_test.go as SQL-level tests.

use crate::support::{expect_code, message, setup};
use postgres::Client;

fn create(c: &mut Client, check: &str) {
    c.batch_execute(&format!("CREATE TABLE items (id int4 NOT NULL, qty int4 NOT NULL, CONSTRAINT items_qty_check CHECK ({check}))"))
        .expect("create");
}

#[test]
fn insert_check_violation_rejects_bad_row() {
    let (_db, mut c) = setup();
    create(&mut c, "qty > 0");
    let e = expect_code(c.execute("INSERT INTO items (id, qty) VALUES (1, 0)", &[]), "23514");
    assert!(message(&e).contains(r#""items_qty_check""#), "message: {}", message(&e));
}

#[test]
fn insert_check_passes_admits_good_row() {
    let (_db, mut c) = setup();
    create(&mut c, "qty > 0");
    c.execute("INSERT INTO items (id, qty) VALUES (1, 5)", &[]).expect("insert");
}

#[test]
fn insert_check_evaluates_against_other_column() {
    let (_db, mut c) = setup();
    create(&mut c, "id < qty");
    c.execute("INSERT INTO items (id, qty) VALUES (2, 5)", &[]).expect("good row");
    expect_code(c.execute("INSERT INTO items (id, qty) VALUES (7, 3)", &[]), "23514");
}

#[test]
fn insert_check_null_evaluates_as_pass() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE items (id int4 NOT NULL, qty int4, CONSTRAINT items_qty_check CHECK (qty > 0))").expect("create");
    c.execute("INSERT INTO items (id, qty) VALUES (1, NULL)", &[]).expect("NULL CHECK should pass");
    let n: i64 = c.query_one("SELECT count(*) FROM items", &[]).expect("count").get(0);
    assert_eq!(n, 1);
}
