use crate::support::{TestDb, expect_code, setup};
use postgres::Client;

/// customers + orders (FK) with one customer row.
fn fk_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id));
         INSERT INTO customers (id, name) VALUES (1, 'alice');",
    )
    .expect("setup");
    (db, c)
}

#[test]
fn fk_insert_rejects_missing_parent() {
    let (_db, mut c) = fk_setup();
    expect_code(c.execute("INSERT INTO orders (id, customer_id) VALUES (10, 999)", &[]), "23503");
}

#[test]
fn fk_insert_accepts_existing_parent() {
    let (_db, mut c) = fk_setup();
    c.execute("INSERT INTO orders (id, customer_id) VALUES (10, 1)", &[]).expect("insert");
}

#[test]
fn fk_update_rejects_missing_parent() {
    let (_db, mut c) = fk_setup();
    c.execute("INSERT INTO orders (id, customer_id) VALUES (10, 1)", &[]).expect("seed");
    expect_code(c.execute("UPDATE orders SET customer_id = 42 WHERE id = 10", &[]), "23503");
}

#[test]
fn fk_delete_parent_with_children_restricts() {
    let (_db, mut c) = fk_setup();
    c.execute("INSERT INTO orders (id, customer_id) VALUES (10, 1)", &[]).expect("seed");
    expect_code(c.execute("DELETE FROM customers WHERE id = 1", &[]), "23503");
    let id: i32 = c.query_one("SELECT id FROM customers WHERE id = 1", &[]).expect("readback").get(0);
    assert_eq!(id, 1);
}

#[test]
fn fk_delete_parent_without_children_ok() {
    let (_db, mut c) = fk_setup();
    c.execute("DELETE FROM customers WHERE id = 1", &[]).expect("delete");
}

#[test]
fn fk_null_value_allowed() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY);
         CREATE TABLE notes (id int PRIMARY KEY, customer_id int REFERENCES customers(id));",
    )
    .expect("setup");
    // MATCH SIMPLE: a NULL in a nullable FK column skips the existence check.
    c.execute("INSERT INTO notes (id, customer_id) VALUES ($1, $2)", &[&1i32, &None::<i32>]).expect("insert");
}

#[test]
fn fk_visible_within_transaction() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY);
         CREATE TABLE notes (id int PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id));",
    )
    .expect("setup");
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO customers (id) VALUES (1)", &[]).expect("INSERT customer");
    c.execute("INSERT INTO notes (id, customer_id) VALUES (10, 1)", &[]).expect("child INSERT in same tx");
    c.batch_execute("COMMIT").expect("COMMIT");
}
