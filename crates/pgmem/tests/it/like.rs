use crate::support::{TestDb, setup};
use postgres::Client;
use postgres::types::ToSql;

fn fixture() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL); INSERT INTO t (id, name) VALUES (1, 'Alice'), (2, 'Bob'), (3, 'alfred'), (4, 'Carol')").expect("setup");
    (db, c)
}

fn ids(c: &mut Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Vec<i32> {
    c.query(sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")).iter().map(|r| r.get(0)).collect()
}

#[test]
fn like_prefix() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE name LIKE 'A%' ORDER BY id", &[]), [1]);
}

#[test]
fn like_underscore() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE name LIKE 'B_b' ORDER BY id", &[]), [2]);
}

#[test]
fn i_like_case_insensitive() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE name ILIKE 'a%' ORDER BY id", &[]), [1, 3]);
}

#[test]
fn like_not_like() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE name NOT LIKE 'A%' ORDER BY id", &[]), [2, 3, 4]);
}

#[test]
fn like_param_pattern() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM t WHERE name LIKE $1 ORDER BY id", &[&"%o%"]), [2, 4]);
}

#[test]
fn like_escaped_percent() {
    let (_db, mut c) = fixture();
    c.execute("INSERT INTO t (id, name) VALUES (5, '50%off')", &[]).expect("insert");
    assert_eq!(ids(&mut c, r"SELECT id FROM t WHERE name LIKE '50\%off' ORDER BY id", &[]), [5]);
}
