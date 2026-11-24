use crate::support::{TestDb, expect_code, message, setup};
use postgres::Client;

fn truncate_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE a (id int PRIMARY KEY, n int NOT NULL);
         CREATE TABLE b (id int PRIMARY KEY, n int NOT NULL);
         INSERT INTO a (id, n) VALUES (1, 10), (2, 20);
         INSERT INTO b (id, n) VALUES (3, 30), (4, 40)",
    )
    .expect("setup");
    (db, c)
}

fn count(c: &mut Client, table: &str) -> i64 {
    c.query_one(&format!("SELECT count(*) FROM {table}"), &[]).expect("count").get(0)
}

#[test]
fn truncate_single_table() {
    let (_db, mut c) = truncate_setup();
    c.execute("TRUNCATE a", &[]).expect("truncate");
    assert_eq!(count(&mut c, "a"), 0);
    assert_eq!(count(&mut c, "b"), 2, "b untouched");
}

#[test]
fn truncate_table_keyword() {
    let (_db, mut c) = truncate_setup();
    c.execute("TRUNCATE TABLE a", &[]).expect("truncate");
    assert_eq!(count(&mut c, "a"), 0);
}

#[test]
fn truncate_multiple() {
    let (_db, mut c) = truncate_setup();
    c.execute("TRUNCATE a, b RESTART IDENTITY CASCADE", &[]).expect("truncate");
    assert_eq!(count(&mut c, "a"), 0);
    assert_eq!(count(&mut c, "b"), 0);
}

#[test]
fn truncate_unknown_table() {
    let (_db, mut c) = truncate_setup();
    let e = expect_code(c.execute("TRUNCATE nonexistent", &[]), "42P01");
    assert_eq!(message(&e), r#"relation "nonexistent" does not exist"#);
}
