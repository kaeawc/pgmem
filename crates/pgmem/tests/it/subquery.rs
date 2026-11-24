use crate::support::{expect_code, setup};

fn subquery_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL);
         INSERT INTO customers (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'carol');
         INSERT INTO orders (id, customer_id) VALUES (10, 1), (11, 2);",
    )
    .expect("subquery setup");
}

#[test]
fn scalar_subquery_over_wire() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let n: i32 = c.query_one("SELECT (SELECT id FROM customers WHERE name = 'alice')", &[]).expect("scalar").get(0);
    assert_eq!(n, 1);
}

#[test]
fn scalar_subquery_empty_returns_null() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let n: Option<i32> = c.query_one("SELECT (SELECT id FROM customers WHERE name = 'no-one')", &[]).expect("scalar").get(0);
    assert_eq!(n, None);
}

#[test]
fn scalar_subquery_multiple_rows_errors() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    expect_code(c.execute("SELECT (SELECT id FROM customers)", &[]), "21000");
}

#[test]
fn in_list_over_wire() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let rows = c.query("SELECT id FROM customers WHERE id IN (1, 3)", &[]).expect("select");
    let mut got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    got.sort();
    assert_eq!(got, [1, 3]);
}

#[test]
fn in_subquery_over_wire() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let rows = c.query("SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders) ORDER BY name", &[]).expect("select");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["alice", "bob"]);
}

#[test]
fn not_in_subquery_over_wire() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let rows = c.query("SELECT name FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)", &[]).expect("select");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["carol"]);
}

#[test]
fn in_subquery_with_parameter() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let rows = c.query("SELECT id FROM customers WHERE name IN ($1, $2) ORDER BY id", &[&"alice", &"carol"]).expect("select");
    let got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, [1, 3]);
}
