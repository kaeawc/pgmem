use crate::support::setup;

fn setup_join_tables(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL, total bigint NOT NULL);
         INSERT INTO customers (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'carol');
         INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100), (11, 2, 200);",
    )
    .expect("setup join tables");
    // Carol intentionally has no orders.
}

#[test]
fn left_join_over_wire() {
    let (_db, mut c) = setup();
    setup_join_tables(&mut c);
    let rows = c
        .query("SELECT customers.name, orders.total FROM customers LEFT JOIN orders ON customers.id = orders.customer_id", &[])
        .expect("left join");
    let mut got: Vec<(String, Option<i64>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    got.sort();
    assert_eq!(got, vec![("alice".to_owned(), Some(100)), ("bob".to_owned(), Some(200)), ("carol".to_owned(), None)]);
}

#[test]
fn cross_join_over_wire() {
    let (_db, mut c) = setup();
    setup_join_tables(&mut c);
    let rows = c.query("SELECT customers.id FROM customers CROSS JOIN orders", &[]).expect("cross join");
    let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids.len(), 6);
}
