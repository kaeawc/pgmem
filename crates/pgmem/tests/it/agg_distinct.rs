use crate::support::setup;

fn agg_distinct_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, kind text NOT NULL, n int NOT NULL);
         INSERT INTO t (id, kind, n) VALUES (1, 'a', 10), (2, 'a', 10), (3, 'b', 20), (4, 'b', 30), (5, 'a', 40);",
    )
    .expect("setup");
}

#[test]
fn agg_distinct_count() {
    let (_db, mut c) = setup();
    agg_distinct_setup(&mut c);
    let got: i64 = c.query_one("SELECT count(DISTINCT kind) FROM t", &[]).expect("query").get(0);
    assert_eq!(got, 2);
}

#[test]
fn agg_distinct_count_vs_total() {
    let (_db, mut c) = setup();
    agg_distinct_setup(&mut c);
    let row = c.query_one("SELECT count(DISTINCT n), count(n) FROM t", &[]).expect("query");
    let (distinct, total): (i64, i64) = (row.get(0), row.get(1));
    assert_eq!(distinct, 4); // 10, 20, 30, 40
    assert_eq!(total, 5);
}

#[test]
fn agg_distinct_sum() {
    let (_db, mut c) = setup();
    agg_distinct_setup(&mut c);
    let got: i64 = c.query_one("SELECT sum(DISTINCT n) FROM t", &[]).expect("query").get(0);
    assert_eq!(got, 100); // 10 + 20 + 30 + 40 (the second 10 is dropped)
}

#[test]
fn agg_distinct_grouped() {
    let (_db, mut c) = setup();
    agg_distinct_setup(&mut c);
    let rows = c.query("SELECT kind, count(DISTINCT n) FROM t GROUP BY kind ORDER BY kind", &[]).expect("query");
    let got: Vec<(String, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [("a".to_owned(), 2), ("b".to_owned(), 2)]);
}
