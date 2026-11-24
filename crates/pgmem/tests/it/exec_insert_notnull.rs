//! Port of exec/insert_notnull_test.go as SQL-level tests.

use crate::support::{expect_code, message, setup};
use postgres::Client;

fn count(c: &mut Client, table: &str) -> i64 {
    c.query_one(&format!("SELECT count(*) FROM {table}"), &[]).expect("count").get(0)
}

#[test]
fn insert_not_null_violation_returns_sql_state() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE items (id int4 NOT NULL, label text NOT NULL)").expect("create");
    let e = expect_code(c.execute("INSERT INTO items (id, label) VALUES (1, NULL)", &[]), "23502");
    let want = r#"null value in column "label" of relation "items""#;
    assert!(message(&e).contains(want), "message: {}", message(&e));
    assert_eq!(count(&mut c, "items"), 0);
}

#[test]
fn insert_not_null_allows_nullable_column() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE notes (id int4 NOT NULL, body text)").expect("create");
    c.execute("INSERT INTO notes (id, body) VALUES (7, NULL)", &[]).expect("insert");
    let rows = c.query("SELECT id, body FROM notes", &[]).expect("select");
    let got: Vec<(i32, Option<String>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(7, None)]);
}

#[test]
fn insert_multi_row_not_null_validated_before_any_insert() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE items (id int4 NOT NULL, label text NOT NULL)").expect("create");
    expect_code(c.execute("INSERT INTO items (id, label) VALUES (1, 'ok'), (2, NULL), (3, 'also-ok')", &[]), "23502");
    assert_eq!(count(&mut c, "items"), 0);
}
