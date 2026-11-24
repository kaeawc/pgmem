use crate::support::{TestDb, expect_code, setup};
use postgres::Client;

fn view_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE orders (id int PRIMARY KEY, user_id int NOT NULL, amount int NOT NULL, status text NOT NULL);
         INSERT INTO orders (id, user_id, amount, status) VALUES
           (1, 1, 100, 'paid'),
           (2, 1, 50, 'pending'),
           (3, 2, 200, 'paid'),
           (4, 2, 75, 'paid');",
    )
    .expect("setup");
    (db, c)
}

#[test]
fn create_view_filtering_view() {
    let (_db, mut c) = view_setup();
    c.batch_execute("CREATE VIEW paid_orders AS SELECT * FROM orders WHERE status = 'paid'").expect("CREATE VIEW");
    let ids: Vec<i32> = c.query("SELECT id FROM paid_orders ORDER BY id", &[]).expect("query").iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![1, 3, 4]);
}

#[test]
fn create_view_aggregate_view() {
    let (_db, mut c) = view_setup();
    c.batch_execute(
        "CREATE VIEW user_totals AS
         SELECT user_id, sum(amount)::bigint AS total
         FROM orders WHERE status = 'paid' GROUP BY user_id",
    )
    .expect("CREATE VIEW");
    let rows = c.query("SELECT user_id, total FROM user_totals ORDER BY user_id", &[]).expect("query");
    let got: Vec<(i32, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(1, 100), (2, 275)]);
}

#[test]
fn create_view_qualified_reference() {
    let (_db, mut c) = view_setup();
    c.batch_execute("CREATE VIEW paid_orders AS SELECT id, user_id, amount FROM orders WHERE status = 'paid'").expect("CREATE VIEW");
    let n: i64 = c.query_one("SELECT count(*) FROM paid_orders po WHERE po.user_id = 2", &[]).expect("query").get(0);
    assert_eq!(n, 2);
}

#[test]
fn drop_view() {
    let (_db, mut c) = view_setup();
    c.batch_execute("CREATE VIEW v AS SELECT * FROM orders").expect("CREATE VIEW");
    c.batch_execute("DROP VIEW v").expect("DROP VIEW");
    expect_code(c.query("SELECT * FROM v", &[]), "42P01");
    c.batch_execute("DROP VIEW IF EXISTS v").expect("DROP VIEW IF EXISTS");
}

#[test]
fn create_view_conflict_with_table_name() {
    let (_db, mut c) = view_setup();
    expect_code(c.batch_execute("CREATE VIEW orders AS SELECT * FROM orders"), "42P07");
}
