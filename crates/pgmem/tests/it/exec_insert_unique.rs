//! Port of exec/insert_unique_test.go as SQL-level tests.

use crate::support::{expect_code, message, setup};
use postgres::Client;

fn count(c: &mut Client, table: &str) -> i64 {
    c.query_one(&format!("SELECT count(*) FROM {table}"), &[]).expect("count").get(0)
}

#[test]
fn insert_unique_violation_against_existing_row() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE users (id int4 NOT NULL UNIQUE, name text)").expect("create");
    c.execute("INSERT INTO users (id, name) VALUES (1, 'alice')", &[]).expect("first insert");
    let e = expect_code(c.execute("INSERT INTO users (id, name) VALUES (1, 'alice-again')", &[]), "23505");
    assert!(message(&e).contains(r#""users_id_key""#), "message: {}", message(&e));
    assert_eq!(count(&mut c, "users"), 1);
}

#[test]
fn insert_unique_violation_within_same_statement() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE tags (name text NOT NULL UNIQUE)").expect("create");
    expect_code(c.execute("INSERT INTO tags (name) VALUES ('go'), ('rust'), ('go')", &[]), "23505");
    assert_eq!(count(&mut c, "tags"), 0);
}

#[test]
fn insert_unique_allows_multiple_nulls() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE logs (trace_id text UNIQUE)").expect("create");
    c.execute("INSERT INTO logs (trace_id) VALUES (NULL), (NULL), ('abc')", &[]).expect("insert");
    assert_eq!(count(&mut c, "logs"), 3);
}

#[test]
fn insert_primary_key_is_not_null_and_unique() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE accounts (id int4 PRIMARY KEY, name text)").expect("create");
    c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice')", &[]).expect("insert");
    expect_code(c.execute("INSERT INTO accounts (id, name) VALUES (1, 'bob')", &[]), "23505");
    expect_code(c.execute("INSERT INTO accounts (id, name) VALUES (NULL, 'carol')", &[]), "23502");
}
