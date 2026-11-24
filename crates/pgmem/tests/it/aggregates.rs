use crate::support::{Text, expect_code, message, setup};

fn agg_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, n int, label text);
         INSERT INTO t (id, n, label) VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, NULL), (4, NULL, 'd');",
    )
    .expect("setup");
}

fn count(c: &mut postgres::Client, sql: &str) -> i64 {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn aggregate_count_star() {
    let (_db, mut c) = setup();
    agg_setup(&mut c);
    assert_eq!(count(&mut c, "SELECT count(*) FROM t"), 4);
}

/// count(col) skips NULLs.
#[test]
fn aggregate_count_column() {
    let (_db, mut c) = setup();
    agg_setup(&mut c);
    assert_eq!(count(&mut c, "SELECT count(label) FROM t"), 3);
}

#[test]
fn aggregate_sum_min_max_avg() {
    let (_db, mut c) = setup();
    agg_setup(&mut c);
    let row = c.query_one("SELECT sum(n), min(n), max(n), avg(n) FROM t", &[]).expect("select");
    // Non-null n's are 10, 20, 30. sum(int4) is bigint, min/max keep int4, avg(int4) is numeric.
    let (sum, min, max, avg): (i64, i32, i32, Text) = (row.get(0), row.get(1), row.get(2), row.get(3));
    assert_eq!((sum, min, max), (60, 10, 30));
    assert_eq!(avg, "20.0000000000000000");
}

/// On an empty table count is 0 and sum/min/max/avg are NULL.
#[test]
fn aggregate_on_empty_table() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE empty (n int)").expect("create");
    assert_eq!(count(&mut c, "SELECT count(*) FROM empty"), 0);
    let row = c.query_one("SELECT sum(n), min(n), max(n), avg(n) FROM empty", &[]).expect("select");
    let (sum, min, max, avg): (Option<i64>, Option<i32>, Option<i32>, Option<Text>) = (row.get(0), row.get(1), row.get(2), row.get(3));
    assert_eq!((sum, min, max, avg), (None, None, None, None));
}

#[test]
fn aggregate_with_where() {
    let (_db, mut c) = setup();
    agg_setup(&mut c);
    assert_eq!(count(&mut c, "SELECT count(*) FROM t WHERE n >= 20"), 2);
}

#[test]
fn aggregate_max_on_text() {
    let (_db, mut c) = setup();
    agg_setup(&mut c);
    let got: String = c.query_one("SELECT max(label) FROM t", &[]).expect("select").get(0);
    assert_eq!(got, "d");
}

#[test]
fn aggregate_mixed_requires_group_by() {
    let (_db, mut c) = setup();
    agg_setup(&mut c);
    let e = expect_code(c.execute("SELECT id, count(*) FROM t", &[]), "42803");
    assert_eq!(message(&e), r#"column "t.id" must appear in the GROUP BY clause or be used in an aggregate function"#);
}
