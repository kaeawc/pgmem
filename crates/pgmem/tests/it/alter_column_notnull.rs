use crate::support::{expect_code, setup};

#[test]
fn alter_column_drop_not_null() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL)").expect("CREATE");
    c.execute("INSERT INTO t (id, name) VALUES (1, 'a')", &[]).expect("INSERT");
    expect_code(c.execute("INSERT INTO t (id, name) VALUES (2, NULL)", &[]), "23502");
    c.batch_execute("ALTER TABLE t ALTER COLUMN name DROP NOT NULL").expect("ALTER COLUMN DROP NOT NULL");
    c.execute("INSERT INTO t (id, name) VALUES (2, NULL)", &[]).expect("INSERT after DROP NOT NULL");
}

#[test]
fn alter_column_set_not_null_no_nulls() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, label text)").expect("CREATE");
    c.execute("INSERT INTO t (id, label) VALUES (1, 'a'), (2, 'b')", &[]).expect("INSERT");
    c.batch_execute("ALTER TABLE t ALTER COLUMN label SET NOT NULL").expect("ALTER COLUMN SET NOT NULL");
    expect_code(c.execute("INSERT INTO t (id, label) VALUES (3, NULL)", &[]), "23502");
}

#[test]
fn alter_column_set_not_null_existing() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, label text)").expect("CREATE");
    c.execute("INSERT INTO t (id, label) VALUES (1, NULL)", &[]).expect("INSERT");
    expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN label SET NOT NULL"), "23502");
}

#[test]
fn alter_column_set_not_null_optional_column_keyword() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, label text)").expect("CREATE");
    c.batch_execute("ALTER TABLE t ALTER label SET NOT NULL").expect("ALTER without COLUMN keyword");
}

#[test]
fn alter_column_missing_column() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY)").expect("CREATE");
    expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN nope SET NOT NULL"), "42703");
}
