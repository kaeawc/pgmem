use crate::support::{TestDb, expect_code, message, setup};
use postgres::Client;

fn insert_select_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE src (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE dst (id int PRIMARY KEY, name text NOT NULL);
         INSERT INTO src (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
    )
    .expect("setup");
    (db, c)
}

fn dst_count(c: &mut Client) -> i64 {
    c.query_one("SELECT count(*) FROM dst", &[]).expect("count").get(0)
}

#[test]
fn insert_select_basic() {
    let (_db, mut c) = insert_select_setup();
    let n = c.execute("INSERT INTO dst (id, name) SELECT id, name FROM src", &[]).expect("insert");
    assert_eq!(n, 3);
    assert_eq!(dst_count(&mut c), 3);
}

#[test]
fn insert_select_with_where() {
    let (_db, mut c) = insert_select_setup();
    c.execute("INSERT INTO dst (id, name) SELECT id, name FROM src WHERE id > 1", &[]).expect("insert");
    assert_eq!(dst_count(&mut c), 2);
}

#[test]
fn insert_select_returning() {
    let (_db, mut c) = insert_select_setup();
    let rows = c.query("INSERT INTO dst (id, name) SELECT id, name FROM src ORDER BY id RETURNING id", &[]).expect("insert");
    let mut ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3]);
}

#[test]
fn insert_select_column_mismatch_errors() {
    let (_db, mut c) = insert_select_setup();
    let e = expect_code(c.execute("INSERT INTO dst (id) SELECT id, name FROM src", &[]), "42601");
    assert_eq!(message(&e), "INSERT has more expressions than target columns");
}
