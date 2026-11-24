use crate::support::{expect_code, message, setup};

/// After DROP, a subsequent SELECT errors because the table no longer exists.
#[test]
fn drop_table_removes_table() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY)").expect("create");
    c.execute("DROP TABLE t", &[]).expect("drop");
    expect_code(c.execute("SELECT id FROM t", &[]), "42P01");
}

/// DROP TABLE on a nonexistent table is SQLSTATE 42P01 (undefined_table).
#[test]
fn drop_table_missing_errors() {
    let (_db, mut c) = setup();
    let e = expect_code(c.execute("DROP TABLE nope", &[]), "42P01");
    assert_eq!(message(&e), r#"table "nope" does not exist"#);
}

/// DROP TABLE IF EXISTS is a no-op on a missing table.
#[test]
fn drop_table_if_exists() {
    let (_db, mut c) = setup();
    c.execute("DROP TABLE IF EXISTS nope", &[]).expect("drop if exists");
}

/// Dropping then recreating a table with the same name starts fresh (no leftover rows).
#[test]
fn drop_table_recreate_works() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY); INSERT INTO t (id) VALUES (1); DROP TABLE t; CREATE TABLE t (id int PRIMARY KEY)",
    )
    .expect("setup");
    let rows = c.query("SELECT id FROM t", &[]).expect("select");
    assert!(rows.is_empty(), "recreated table should be empty");
}
