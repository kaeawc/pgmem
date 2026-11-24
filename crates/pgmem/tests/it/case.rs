use crate::support::{TestDb, setup};
use postgres::Client;

fn case_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, n int, label text);
         INSERT INTO t (id, n, label) VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c'), (4, NULL, 'd')",
    )
    .expect("setup");
    (db, c)
}

#[test]
fn case_searched() {
    let (_db, mut c) = case_setup();
    let rows = c
        .query(
            "SELECT id,
                CASE WHEN n < 15 THEN 'low'
                     WHEN n < 25 THEN 'mid'
                     ELSE 'high'
                END
             FROM t WHERE n IS NOT NULL ORDER BY id",
            &[],
        )
        .expect("query");
    let got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(1, "low".to_owned()), (2, "mid".to_owned()), (3, "high".to_owned())]);
}

#[test]
fn case_simple() {
    let (_db, mut c) = case_setup();
    let rows = c.query("SELECT CASE label WHEN 'a' THEN 1 WHEN 'b' THEN 2 ELSE 0 END FROM t ORDER BY id", &[]).expect("query");
    let got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![1, 2, 0, 0]);
}

#[test]
fn case_no_else_defaults_null() {
    let (_db, mut c) = case_setup();
    let got: Option<String> = c.query_one("SELECT CASE WHEN 1 = 2 THEN 'never' END", &[]).expect("query").get(0);
    assert_eq!(got, None);
}

#[test]
fn case_null_operand() {
    let (_db, mut c) = case_setup();
    let got: String = c.query_one("SELECT CASE n WHEN 10 THEN 'ten' ELSE 'other' END FROM t WHERE id = 4", &[]).expect("query").get(0);
    assert_eq!(got, "other");
}
