use crate::support::setup;

/// M2 acceptance: CREATE TABLE, multi-row INSERT, SELECT with WHERE / ORDER BY / LIMIT, all with `$N` parameters.
#[test]
fn m2_create_insert_select() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE accounts (id int NOT NULL, name text NOT NULL, balance bigint NOT NULL, active bool NOT NULL)")
        .expect("create");
    c.execute(
        "INSERT INTO accounts (id, name, balance, active) VALUES
            ($1, $2, $3, $4),
            ($5, $6, $7, $8),
            ($9, $10, $11, $12),
            ($13, $14, $15, $16)",
        &[
            &1i32, &"alice", &100i64, &true, //
            &2i32, &"bob", &50i64, &false, //
            &3i32, &"carol", &200i64, &true, //
            &4i32, &"dan", &75i64, &true,
        ],
    )
    .expect("insert");
    let rows = c
        .query(
            "SELECT id, name, balance FROM accounts WHERE active = $1 AND balance >= $2 ORDER BY balance DESC LIMIT $3",
            &[&true, &75i64, &2i64],
        )
        .expect("select");
    let got: Vec<(i32, String, i64)> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!(got, vec![(3, "carol".to_owned(), 200), (1, "alice".to_owned(), 100)]);
}

/// The OFFSET path and ASC order.
#[test]
fn m2_order_asc_with_offset() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE nums (n int NOT NULL); INSERT INTO nums (n) VALUES (5), (1), (4), (2), (3)").expect("setup");
    let rows = c.query("SELECT n FROM nums ORDER BY n ASC LIMIT 2 OFFSET 1", &[]).expect("select");
    let got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![2, 3]);
}
