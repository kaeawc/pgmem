use crate::support::{TestDb, setup};
use postgres::Client;
use postgres::types::ToSql;

fn fixture() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, n int); INSERT INTO t (id, n) VALUES (1, 10), (2, NULL), (3, 30), (4, NULL)")
        .expect("setup");
    (db, c)
}

fn ids(c: &mut Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Vec<i32> {
    c.query(sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")).iter().map(|r| r.get(0)).collect()
}

#[test]
fn is_null_filters() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n IS NULL ORDER BY id", &[]), [2, 4]);
}

#[test]
fn is_not_null_filters() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n IS NOT NULL ORDER BY id", &[]), [1, 3]);
}

#[test]
fn is_null_distinct_from_equals() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n = NULL ORDER BY id", &[]), Vec::<i32>::new());
}
