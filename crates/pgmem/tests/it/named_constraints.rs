use crate::support::{expect_code, message, setup};

#[test]
fn named_constraint_column_level_pk() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int CONSTRAINT t_pk PRIMARY KEY)").expect("create");
    c.execute("INSERT INTO t (id) VALUES (1)", &[]).expect("insert");
    let e = expect_code(c.execute("INSERT INTO t (id) VALUES (1)", &[]), "23505");
    assert_eq!(message(&e), r#"duplicate key value violates unique constraint "t_pk""#);
}

#[test]
fn named_constraint_column_level_check() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, age int CONSTRAINT age_nonneg CHECK (age >= 0))").expect("create");
    c.execute("INSERT INTO t (id, age) VALUES (1, 5)", &[]).expect("insert");
    let e = expect_code(c.execute("INSERT INTO t (id, age) VALUES (2, -1)", &[]), "23514");
    assert_eq!(message(&e), r#"new row for relation "t" violates check constraint "age_nonneg""#);
}

#[test]
fn named_constraint_table_level_check() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (
           id int PRIMARY KEY,
           lo int NOT NULL,
           hi int NOT NULL,
           CONSTRAINT lo_le_hi CHECK (lo <= hi)
         )",
    )
    .expect("create");
    c.execute("INSERT INTO t (id, lo, hi) VALUES (1, 0, 10)", &[]).expect("insert");
    let e = expect_code(c.execute("INSERT INTO t (id, lo, hi) VALUES (2, 10, 0)", &[]), "23514");
    assert_eq!(message(&e), r#"new row for relation "t" violates check constraint "lo_le_hi""#);
}

#[test]
fn named_constraint_table_level_check_no_name() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, qty int NOT NULL, CHECK (qty > 0))").expect("create");
    let e = expect_code(c.execute("INSERT INTO t (id, qty) VALUES (1, 0)", &[]), "23514");
    assert_eq!(message(&e), r#"new row for relation "t" violates check constraint "t_qty_check""#);
}

/// A named composite PRIMARY KEY (CONSTRAINT name + PRIMARY KEY (col1, col2)) is accepted and enforced.
#[test]
fn named_constraint_table_level_pk_keeps_unique() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int NOT NULL, b int NOT NULL, CONSTRAINT t_pk PRIMARY KEY (a, b))").expect("create");
    c.execute("INSERT INTO t (a, b) VALUES (1, 2), (1, 3)", &[]).expect("insert");
    let e = expect_code(c.execute("INSERT INTO t (a, b) VALUES (1, 2)", &[]), "23505");
    assert_eq!(message(&e), r#"duplicate key value violates unique constraint "t_pk""#);
}

/// The constraint name is exposed in the error message — sqlc tests often grep for it.
#[test]
fn named_constraint_name_round_trips_in_error() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, qty int NOT NULL, CONSTRAINT qty_positive CHECK (qty > 0))").expect("create");
    let e = expect_code(c.execute("INSERT INTO t (id, qty) VALUES (1, 0)", &[]), "23514");
    assert!(message(&e).contains("qty_positive"), "error {:?} does not mention constraint name", message(&e));
    assert_eq!(e.as_db_error().and_then(|d| d.constraint()), Some("qty_positive"));
}
