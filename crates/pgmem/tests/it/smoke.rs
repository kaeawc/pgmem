use crate::support::setup;

#[test]
fn select_one() {
    let (_db, mut c) = setup();
    let row = c.query_one("SELECT 1", &[]).expect("query");
    let n: i32 = row.get(0);
    assert_eq!(n, 1);
    let rows = c.simple_query("SELECT 1 AS a, 'x' AS b").expect("simple");
    assert!(!rows.is_empty());
}

#[test]
fn create_insert_select() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE accounts (id int PRIMARY KEY, name text NOT NULL, balance bigint NOT NULL, active bool NOT NULL)")
        .expect("create");
    for (id, name, bal, active) in [(1, "alice", 100i64, true), (2, "bob", 50, false), (3, "carol", 200, true), (4, "dan", 75, true)] {
        c.execute("INSERT INTO accounts (id, name, balance, active) VALUES ($1, $2, $3, $4)", &[&id, &name, &bal, &active])
            .expect("insert");
    }
    let rows = c
        .query(
            "SELECT id, name, balance FROM accounts WHERE active = $1 AND balance >= $2 ORDER BY balance DESC LIMIT $3",
            &[&true, &75i64, &2i64],
        )
        .expect("select");
    let got: Vec<(i32, String, i64)> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!(got, vec![(3, "carol".to_owned(), 200), (1, "alice".to_owned(), 100)]);
}
