use crate::support::{expect_code, message, setup};

fn union_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE a (id int PRIMARY KEY, n int NOT NULL);
         CREATE TABLE b (id int PRIMARY KEY, n int NOT NULL);
         INSERT INTO a (id, n) VALUES (1, 10), (2, 20), (3, 30);
         INSERT INTO b (id, n) VALUES (4, 30), (5, 40);",
    )
    .expect("union setup");
}

/// The int4 column of every row, sorted (the queries have no ORDER BY).
fn sorted_ints(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    let mut got: Vec<i32> = c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect();
    got.sort();
    got
}

#[test]
fn union_all_keeps_duplicates() {
    let (_db, mut c) = setup();
    union_setup(&mut c);
    assert_eq!(sorted_ints(&mut c, "SELECT n FROM a UNION ALL SELECT n FROM b"), [10, 20, 30, 30, 40]);
}

#[test]
fn union_deduplicates() {
    let (_db, mut c) = setup();
    union_setup(&mut c);
    assert_eq!(sorted_ints(&mut c, "SELECT n FROM a UNION SELECT n FROM b"), [10, 20, 30, 40]);
}

#[test]
fn union_chain() {
    let (_db, mut c) = setup();
    union_setup(&mut c);
    assert_eq!(sorted_ints(&mut c, "SELECT n FROM a UNION ALL SELECT n FROM b UNION ALL SELECT 99"), [10, 20, 30, 30, 40, 99]);
}

#[test]
fn union_column_count_mismatch() {
    let (_db, mut c) = setup();
    union_setup(&mut c);
    let e = expect_code(c.query("SELECT id, n FROM a UNION SELECT n FROM b", &[]), "42601");
    assert_eq!(message(&e), "each UNION query must have the same number of columns");
}
