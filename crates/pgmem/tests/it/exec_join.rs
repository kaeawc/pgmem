//! Port of exec/join_test.go as SQL-level tests.

use crate::support::{expect_code, setup};
use postgres::Client;

/// Parent `users` and child `orders`; carol (id 3) deliberately has no orders.
pub fn join_fixture(c: &mut Client) {
    c.batch_execute(
        "CREATE TABLE users (id int4 NOT NULL UNIQUE, name text NOT NULL);
         INSERT INTO users VALUES (1, 'alice'), (2, 'bob'), (3, 'carol');
         CREATE TABLE orders (id int4 NOT NULL UNIQUE, user_id int4 NOT NULL, label text);
         INSERT INTO orders VALUES (100, 1, 'alice-order-A'), (101, 1, 'alice-order-B'), (102, 2, 'bob-order');",
    )
    .expect("join fixture");
}

#[test]
fn inner_join_nested_loop() {
    let (_db, mut c) = setup();
    join_fixture(&mut c);
    let rows = c.query("SELECT users.name, orders.label FROM users JOIN orders ON users.id = orders.user_id", &[]).expect("join");
    let mut got: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    got.sort();
    let want = [("alice", "alice-order-A"), ("alice", "alice-order-B"), ("bob", "bob-order")];
    assert_eq!(got, want.map(|(a, b)| (a.to_owned(), b.to_owned())));
}

#[test]
fn inner_join_ambiguous_unqualified_ref_is_rejected() {
    let (_db, mut c) = setup();
    join_fixture(&mut c);
    expect_code(c.query("SELECT id FROM users JOIN orders ON users.id = orders.user_id", &[]), "42702");
}

#[test]
fn inner_join_qualifier_resolves_unambiguously() {
    let (_db, mut c) = setup();
    join_fixture(&mut c);
    let rows = c.query("SELECT orders.id AS order_id FROM users JOIN orders ON users.id = orders.user_id", &[]).expect("join");
    assert_eq!(rows.len(), 3);
}

#[test]
fn inner_join_no_match_emits() {
    let (_db, mut c) = setup();
    join_fixture(&mut c);
    let rows = c.query("SELECT users.name FROM users JOIN orders ON users.id = 999", &[]).expect("join");
    assert!(rows.is_empty());
}
