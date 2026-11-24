use crate::support::{TestDb, setup};
use postgres::Client;
use postgres::types::ToSql;

fn fixture() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, n int); INSERT INTO t (id, n) VALUES (1, 10), (2, NULL), (3, 30), (4, NULL), (5, 30)",
    )
    .expect("setup");
    (db, c)
}

fn ids(c: &mut Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Vec<i32> {
    c.query(sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")).iter().map(|r| r.get(0)).collect()
}

#[test]
fn is_distinct_truth() {
    let (_db, mut c) = fixture();
    for (sql, want) in [
        ("SELECT 1 IS DISTINCT FROM 1", false),
        ("SELECT 1 IS DISTINCT FROM 2", true),
        ("SELECT NULL IS DISTINCT FROM NULL", false),
        ("SELECT NULL IS DISTINCT FROM 1", true),
        ("SELECT 1 IS NOT DISTINCT FROM NULL", false),
        ("SELECT NULL IS NOT DISTINCT FROM NULL", true),
    ] {
        let got: bool = c.query_one(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).get(0);
        assert_eq!(got, want, "{sql}");
    }
}

#[test]
fn is_distinct_filter_includes_nulls() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n IS DISTINCT FROM 30 ORDER BY id", &[]), [1, 2, 4]);
}

#[test]
fn is_not_distinct_null_safe_equality() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n IS NOT DISTINCT FROM NULL ORDER BY id", &[]), [2, 4]);
}
