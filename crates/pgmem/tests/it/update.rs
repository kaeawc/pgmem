use crate::support::{expect_code, setup};

#[test]
fn update_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE accounts (id int PRIMARY KEY, name text NOT NULL);
         INSERT INTO accounts (id, name) VALUES (1, 'alice'), (2, 'bob')",
    )
    .expect("setup");
    let n = c.execute("UPDATE accounts SET name = $1 WHERE id = $2", &[&"ALICE", &1i32]).expect("update");
    assert_eq!(n, 1);
    let name: String = c.query_one("SELECT name FROM accounts WHERE id = $1", &[&1i32]).expect("select").get(0);
    assert_eq!(name, "ALICE");
}

/// RETURNING reflects the post-update row.
#[test]
fn update_returning_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE accounts (id int PRIMARY KEY, name text NOT NULL);
         INSERT INTO accounts (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
    )
    .expect("setup");
    let rows = c.query("UPDATE accounts SET name = $1 WHERE id >= $2 RETURNING id, name", &[&"X", &2i32]).expect("update");
    let mut got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    got.sort();
    assert_eq!(got, vec![(2, "X".to_owned()), (3, "X".to_owned())]);
}

/// Changing a UNIQUE column to a colliding value rejects with SQLSTATE 23505.
#[test]
fn update_unique_violation_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE accounts (id int PRIMARY KEY, name text NOT NULL);
         INSERT INTO accounts (id, name) VALUES (1, 'a'), (2, 'b')",
    )
    .expect("setup");
    expect_code(c.execute("UPDATE accounts SET id = $1 WHERE id = $2", &[&1i32, &2i32]), "23505");
    let id: i32 = c.query_one("SELECT id FROM accounts WHERE name = 'b'", &[]).expect("select").get(0);
    assert_eq!(id, 2, "original id unchanged");
}
