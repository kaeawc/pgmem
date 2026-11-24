use crate::support::{Text, setup};

fn derived_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE orders (id int PRIMARY KEY, user_id int NOT NULL, amount int NOT NULL);
         INSERT INTO orders (id, user_id, amount) VALUES (1, 1, 10), (2, 1, 20), (3, 2, 5), (4, 2, 50);",
    )
    .expect("derived setup");
}

#[test]
fn derived_table_basic() {
    let (_db, mut c) = setup();
    derived_setup(&mut c);
    let rows = c.query("SELECT sub.amount FROM (SELECT amount FROM orders WHERE user_id = 1) sub ORDER BY sub.amount", &[]).expect("query");
    let got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, [10, 20]);
}

#[test]
fn derived_table_as() {
    let (_db, mut c) = setup();
    derived_setup(&mut c);
    let n: i64 = c.query_one("SELECT count(*) FROM (SELECT amount FROM orders WHERE amount > 5) AS big", &[]).expect("query").get(0);
    assert_eq!(n, 3);
}

#[test]
fn derived_table_aggregate_over_subquery() {
    let (_db, mut c) = setup();
    derived_setup(&mut c);
    // Sum of per-user totals: sum(int) is bigint, and sum(bigint) is numeric.
    let got: Text = c
        .query_one("SELECT sum(per_user.total) FROM (SELECT user_id, sum(amount) AS total FROM orders GROUP BY user_id) per_user", &[])
        .expect("query")
        .get(0);
    assert_eq!(got, "85"); // 10+20+5+50
}

#[test]
fn derived_table_requires_alias() {
    let (_db, mut c) = setup();
    derived_setup(&mut c);
    // Deviation from pgmem-go: it rejected a derived table without an alias; since PostgreSQL 16 the
    // alias is optional, so the query succeeds.
    let rows = c.query("SELECT * FROM (SELECT * FROM orders)", &[]).expect("derived table without alias");
    assert_eq!(rows.len(), 4);
}
