use crate::support::setup;

fn create_tables(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL, total bigint NOT NULL);",
    )
    .expect("create tables");
}

#[test]
fn inner_join_over_wire() {
    let (_db, mut c) = setup();
    create_tables(&mut c);
    c.batch_execute(
        "INSERT INTO customers (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'carol');
         INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100), (11, 1, 50), (12, 2, 200);",
    )
    .expect("insert");
    let rows = c
        .query("SELECT customers.name, orders.total FROM customers INNER JOIN orders ON customers.id = orders.customer_id", &[])
        .expect("join");
    let mut got: Vec<(String, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    got.sort();
    assert_eq!(got, vec![("alice".to_owned(), 50), ("alice".to_owned(), 100), ("bob".to_owned(), 200)]);
}

#[test]
fn inner_join_with_where_and_order() {
    let (_db, mut c) = setup();
    create_tables(&mut c);
    c.batch_execute(
        "INSERT INTO customers (id, name) VALUES (1, 'alice'), (2, 'bob');
         INSERT INTO orders (id, customer_id, total) VALUES (10, 1, 100), (11, 1, 50), (12, 2, 200), (13, 1, 75);",
    )
    .expect("insert");
    let rows = c
        .query(
            "SELECT orders.total FROM customers JOIN orders ON customers.id = orders.customer_id \
             WHERE customers.name = $1 ORDER BY orders.total DESC LIMIT 2",
            &[&"alice"],
        )
        .expect("join");
    let got: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![100, 75]);
}
