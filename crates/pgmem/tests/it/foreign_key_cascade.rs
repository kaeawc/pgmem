use crate::support::{TestDb, expect_code, setup};
use postgres::Client;

/// customers (1, 2) and orders 10, 11 -> customer 1, 12 -> customer 2, with the given FK clause.
fn cascade_setup(fk_clause: &str) -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(&format!(
        "CREATE TABLE customers (id int PRIMARY KEY);
         CREATE TABLE orders (id int PRIMARY KEY, customer_id int {fk_clause});
         INSERT INTO customers (id) VALUES (1), (2);
         INSERT INTO orders (id, customer_id) VALUES (10, 1), (11, 1), (12, 2);"
    ))
    .expect("setup");
    (db, c)
}

fn order_customer_ids(c: &mut Client) -> Vec<Option<i32>> {
    c.query("SELECT customer_id FROM orders ORDER BY id", &[]).expect("SELECT").iter().map(|r| r.get(0)).collect()
}

#[test]
fn fk_on_delete_cascade() {
    let (_db, mut c) = cascade_setup("NOT NULL REFERENCES customers(id) ON DELETE CASCADE");
    c.execute("DELETE FROM customers WHERE id = 1", &[]).expect("DELETE customer");
    assert_eq!(order_customer_ids(&mut c), vec![Some(2)], "only customer 2's order survives");
}

#[test]
fn fk_on_delete_set_null() {
    let (_db, mut c) = cascade_setup("REFERENCES customers(id) ON DELETE SET NULL");
    c.execute("DELETE FROM customers WHERE id = 1", &[]).expect("DELETE customer");
    // Ordered by order id: 10 and 11 are nulled, 12 still points at 2.
    assert_eq!(order_customer_ids(&mut c), vec![None, None, Some(2)]);
}

#[test]
fn fk_default_is_restrict() {
    let (_db, mut c) = cascade_setup("NOT NULL REFERENCES customers(id)");
    expect_code(c.execute("DELETE FROM customers WHERE id = 1", &[]), "23503");
    let id: i32 = c.query_one("SELECT id FROM customers WHERE id = 1", &[]).expect("parent still present").get(0);
    assert_eq!(id, 1);
}

#[test]
fn fk_on_delete_cascade_recursive() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE a (id int PRIMARY KEY);
         CREATE TABLE b (id int PRIMARY KEY, a_id int NOT NULL REFERENCES a(id) ON DELETE CASCADE);
         CREATE TABLE c (id int PRIMARY KEY, b_id int NOT NULL REFERENCES b(id) ON DELETE CASCADE);
         INSERT INTO a (id) VALUES (1);
         INSERT INTO b (id, a_id) VALUES (10, 1);
         INSERT INTO c (id, b_id) VALUES (100, 10);",
    )
    .expect("setup");
    c.execute("DELETE FROM a WHERE id = 1", &[]).expect("cascade delete");
    for table in ["b", "c"] {
        let rows = c.query(&format!("SELECT id FROM {table} LIMIT 1"), &[]).expect("select");
        assert!(rows.is_empty(), "table {table}: expected empty, found {} row(s)", rows.len());
    }
}

#[test]
fn fk_on_delete_set_null_only_affects_matching() {
    let (_db, mut c) = cascade_setup("REFERENCES customers(id) ON DELETE SET NULL");
    c.execute("DELETE FROM customers WHERE id = 2", &[]).expect("DELETE");
    // Order 12 (customer 2) is nulled; orders 10 and 11 (customer 1) are untouched.
    assert_eq!(order_customer_ids(&mut c), vec![Some(1), Some(1), None]);
}
