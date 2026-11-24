use crate::support::{TestDb, setup};
use postgres::Client;
use postgres::types::ToSql;

fn fixture() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, n int NOT NULL); INSERT INTO t (id, n) VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50)",
    )
    .expect("setup");
    (db, c)
}

fn ids(c: &mut Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Vec<i32> {
    c.query(sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")).iter().map(|r| r.get(0)).collect()
}

#[test]
fn between_inclusive() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n BETWEEN 20 AND 40 ORDER BY id", &[]), [2, 3, 4]);
}

#[test]
fn not_between() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n NOT BETWEEN 20 AND 40 ORDER BY id", &[]), [1, 5]);
}

#[test]
fn between_params() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n BETWEEN $1 AND $2 ORDER BY id", &[&15i32, &35i32]), [2, 3]);
}

#[test]
fn between_combined() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE n BETWEEN 10 AND 50 AND id > 2 ORDER BY id", &[]), [3, 4, 5]);
}
