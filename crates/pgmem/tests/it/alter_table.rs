use crate::support::{TestDb, expect_code, setup};
use postgres::Client;

fn alter_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL); INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b');")
        .expect("setup");
    (db, c)
}

#[test]
fn alter_table_add_column_nullable() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t ADD COLUMN note text").expect("ALTER ADD");
    let rows = c.query("SELECT id, name, note FROM t ORDER BY id", &[]).expect("select");
    let got: Vec<(i32, String, Option<String>)> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!(got, vec![(1, "a".to_owned(), None), (2, "b".to_owned(), None)]);
    // The new column is writable.
    c.execute("UPDATE t SET note = 'hi' WHERE id = 1", &[]).expect("UPDATE");
    let note: Option<String> = c.query_one("SELECT note FROM t WHERE id = 1", &[]).expect("select after update").get(0);
    assert_eq!(note.as_deref(), Some("hi"));
}

#[test]
fn alter_table_add_column_not_null_on_empty() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE empty_t (id int PRIMARY KEY)").expect("CREATE");
    c.batch_execute("ALTER TABLE empty_t ADD COLUMN flag bool NOT NULL").expect("ALTER ADD NOT NULL");
    c.execute("INSERT INTO empty_t (id, flag) VALUES (1, true)", &[]).expect("INSERT");
}

#[test]
fn alter_table_add_column_not_null_on_non_empty() {
    let (_db, mut c) = alter_setup();
    expect_code(c.batch_execute("ALTER TABLE t ADD COLUMN flag bool NOT NULL"), "23502");
}

#[test]
fn alter_table_add_column_duplicate() {
    let (_db, mut c) = alter_setup();
    expect_code(c.batch_execute("ALTER TABLE t ADD COLUMN name text"), "42701");
}

#[test]
fn alter_table_add_column_if_not_exists() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t ADD COLUMN IF NOT EXISTS extra int").expect("ALTER ADD IF NOT EXISTS");
}

#[test]
fn alter_table_drop_column() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t DROP COLUMN name").expect("ALTER DROP");
    expect_code(c.query("SELECT name FROM t", &[]), "42703");
    let n: i64 = c.query_one("SELECT count(*) FROM t", &[]).expect("count").get(0);
    assert_eq!(n, 2);
    let id: i32 = c.query_one("SELECT id FROM t WHERE id = 1", &[]).expect("select id").get(0);
    assert_eq!(id, 1);
}

#[test]
fn alter_table_drop_column_if_exists_missing() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t DROP COLUMN IF EXISTS missing").expect("DROP IF EXISTS missing");
    expect_code(c.batch_execute("ALTER TABLE t DROP COLUMN missing"), "42703");
}

#[test]
fn alter_table_drop_column_cascade() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t DROP COLUMN name CASCADE").expect("DROP CASCADE");
}

#[test]
fn alter_table_rename_column() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t RENAME COLUMN name TO label").expect("ALTER RENAME");
    let label: String = c.query_one("SELECT label FROM t WHERE id = 1", &[]).expect("select label").get(0);
    assert_eq!(label, "a");
    expect_code(c.query("SELECT name FROM t", &[]), "42703");
}

#[test]
fn alter_table_rename_column_conflict() {
    let (_db, mut c) = alter_setup();
    expect_code(c.batch_execute("ALTER TABLE t RENAME COLUMN name TO id"), "42701");
}

#[test]
fn alter_table_missing_table() {
    let (_db, mut c) = alter_setup();
    expect_code(c.batch_execute("ALTER TABLE nope ADD COLUMN x int"), "42P01");
}
