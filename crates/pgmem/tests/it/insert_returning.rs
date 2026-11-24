use crate::support::setup;

/// The sqlc happy path: a single-row INSERT ... RETURNING id, as a generated `CreateUser` would do.
#[test]
fn insert_returning_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE accounts (id int PRIMARY KEY, name text NOT NULL)").expect("create");
    let id: i32 = c.query_one("INSERT INTO accounts (id, name) VALUES ($1, $2) RETURNING id", &[&42i32, &"alice"]).expect("insert").get(0);
    assert_eq!(id, 42);
}

/// The multi-row path yields one row per inserted tuple, in insertion order.
#[test]
fn insert_returning_multi_row() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE counters (n int NOT NULL)").expect("create");
    let rows = c.query("INSERT INTO counters (n) VALUES ($1), ($2), ($3) RETURNING n", &[&10i32, &20i32, &30i32]).expect("insert");
    let got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![10, 20, 30]);
}
