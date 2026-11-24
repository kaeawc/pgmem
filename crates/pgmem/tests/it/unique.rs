use crate::support::{expect_code, setup};

/// A duplicate-key insert fails with SQLSTATE 23505, which sqlc test suites pattern-match on.
#[test]
fn unique_violation_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE accounts (id int PRIMARY KEY, name text NOT NULL)").expect("create");
    c.execute("INSERT INTO accounts (id, name) VALUES ($1, $2)", &[&1i32, &"alice"]).expect("insert");
    expect_code(c.execute("INSERT INTO accounts (id, name) VALUES ($1, $2)", &[&1i32, &"alice-clone"]), "23505");
    let rows = c.query("SELECT id FROM accounts ORDER BY id LIMIT 100", &[]).expect("select");
    let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![1], "the original row is the only survivor");
}

/// PRIMARY KEY gives both 23502 (for NULL) and 23505 (for duplicates).
#[test]
fn unique_violation_primary_key_also_enforces_not_null() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE items (id int PRIMARY KEY)").expect("create");
    expect_code(c.execute("INSERT INTO items (id) VALUES ($1)", &[&None::<i32>]), "23502");
}
