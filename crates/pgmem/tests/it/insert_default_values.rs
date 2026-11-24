use crate::support::{expect_code, setup};

/// A table with only an auto-fill (BIGSERIAL) column accepts DEFAULT VALUES.
#[test]
fn insert_default_values_auto_filled() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE seq (id bigserial PRIMARY KEY)").expect("create");
    c.execute("INSERT INTO seq DEFAULT VALUES", &[]).expect("insert 1");
    c.execute("INSERT INTO seq DEFAULT VALUES", &[]).expect("insert 2");
    let n: i64 = c.query_one("SELECT count(*) FROM seq", &[]).expect("count").get(0);
    assert_eq!(n, 2);
}

/// RETURNING * works with DEFAULT VALUES too.
#[test]
fn insert_default_values_returning() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE seq (id bigserial PRIMARY KEY)").expect("create");
    let id: i64 = c.query_one("INSERT INTO seq DEFAULT VALUES RETURNING *", &[]).expect("insert").get(0);
    assert_eq!(id, 1);
}

/// A NOT NULL column without an auto-fill rejects DEFAULT VALUES with 23502.
#[test]
fn insert_default_values_not_null_still_enforced() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id bigserial PRIMARY KEY, name text NOT NULL)").expect("create");
    expect_code(c.execute("INSERT INTO t DEFAULT VALUES", &[]), "23502");
}
