use crate::support::{TestDb, setup};
use postgres::Client;
use std::collections::BTreeMap;

fn update_from_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE products (id int PRIMARY KEY, name text NOT NULL, price int NOT NULL);
         CREATE TABLE pending_changes (product_id int PRIMARY KEY, new_price int NOT NULL);
         INSERT INTO products (id, name, price) VALUES (1, 'a', 100), (2, 'b', 200), (3, 'c', 300);
         INSERT INTO pending_changes (product_id, new_price) VALUES (1, 150), (3, 350)",
    )
    .expect("setup");
    (db, c)
}

fn id_prices(rows: &[postgres::Row]) -> BTreeMap<i32, i32> {
    rows.iter().map(|r| (r.get(0), r.get(1))).collect()
}

/// Products 1 and 3 pick up the staged price via the join; product 2 stays at 200.
#[test]
fn update_from_simple_join() {
    let (_db, mut c) = update_from_setup();
    let n = c
        .execute(
            "UPDATE products SET price = pending_changes.new_price FROM pending_changes WHERE products.id = pending_changes.product_id",
            &[],
        )
        .expect("update");
    assert_eq!(n, 2);
    let rows = c.query("SELECT id, price FROM products ORDER BY id", &[]).expect("select");
    assert_eq!(id_prices(&rows), BTreeMap::from([(1, 150), (2, 200), (3, 350)]));
}

/// RETURNING uses the post-update target row, ignoring the from-side.
#[test]
fn update_from_returning() {
    let (_db, mut c) = update_from_setup();
    let rows = c
        .query(
            "UPDATE products SET price = pending_changes.new_price
             FROM pending_changes
             WHERE products.id = pending_changes.product_id
             RETURNING id, price",
            &[],
        )
        .expect("update");
    assert_eq!(id_prices(&rows), BTreeMap::from([(1, 150), (3, 350)]));
}

#[test]
fn delete_using() {
    let (_db, mut c) = update_from_setup();
    let n = c.execute("DELETE FROM products USING pending_changes WHERE products.id = pending_changes.product_id", &[]).expect("delete");
    assert_eq!(n, 2);
    let remaining: i64 = c.query_one("SELECT count(*) FROM products", &[]).expect("count").get(0);
    assert_eq!(remaining, 1);
}
