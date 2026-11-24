//! Port of exec/insert_serial_test.go as SQL-level tests.

use crate::support::setup;

const SERIAL_TABLE: &str = "CREATE TABLE t (id serial NOT NULL UNIQUE, label text NOT NULL)";

#[test]
fn serial_assigns_sequential_ids() {
    let (_db, mut c) = setup();
    c.batch_execute(SERIAL_TABLE).expect("create");
    for (i, label) in ["a", "b", "c"].into_iter().enumerate() {
        let rows = c.query("INSERT INTO t (label) VALUES ($1) RETURNING id", &[&label]).expect("insert");
        assert_eq!(rows.len(), 1);
        let id: i32 = rows[0].get(0);
        assert_eq!(id, i32::try_from(i + 1).expect("small"), "insert {i}");
    }
}

#[test]
fn big_serial_returns_int64() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id bigserial NOT NULL UNIQUE, label text NOT NULL)").expect("create");
    let row = c.query_one("INSERT INTO t (label) VALUES ('x') RETURNING id", &[]).expect("insert");
    assert_eq!(*row.columns()[0].type_(), postgres::types::Type::INT8);
    let id: i64 = row.get(0);
    assert_eq!(id, 1);
}

#[test]
fn serial_explicit_value_overrides_counter() {
    let (_db, mut c) = setup();
    c.batch_execute(SERIAL_TABLE).expect("create");
    c.execute("INSERT INTO t (id, label) VALUES (42, 'manual')", &[]).expect("manual");
    let id: i32 = c.query_one("INSERT INTO t (label) VALUES ('auto') RETURNING id", &[]).expect("auto").get(0);
    assert_eq!(id, 1, "counter independent of manual");
}

#[test]
fn serial_rollback_leaves_gap_in_counter() {
    let (_db, mut c) = setup();
    c.batch_execute(SERIAL_TABLE).expect("create");
    let mut tx = c.transaction().expect("begin");
    tx.execute("INSERT INTO t (label) VALUES ('doomed')", &[]).expect("doomed insert");
    tx.rollback().expect("rollback");
    let id: i32 = c.query_one("INSERT INTO t (label) VALUES ('kept') RETURNING id", &[]).expect("kept").get(0);
    assert_eq!(id, 2, "gap from rollback");
}
