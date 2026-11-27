use crate::support::{TestDb, expect_code, message, setup, texts};
use postgres::Client;

/// customers + orders (FK) with one customer row.
fn fk_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id));
         INSERT INTO customers (id, name) VALUES (1, 'alice');",
    )
    .expect("setup");
    (db, c)
}

#[test]
fn fk_insert_rejects_missing_parent() {
    let (_db, mut c) = fk_setup();
    expect_code(c.execute("INSERT INTO orders (id, customer_id) VALUES (10, 999)", &[]), "23503");
}

#[test]
fn fk_insert_accepts_existing_parent() {
    let (_db, mut c) = fk_setup();
    c.execute("INSERT INTO orders (id, customer_id) VALUES (10, 1)", &[]).expect("insert");
}

#[test]
fn fk_update_rejects_missing_parent() {
    let (_db, mut c) = fk_setup();
    c.execute("INSERT INTO orders (id, customer_id) VALUES (10, 1)", &[]).expect("seed");
    expect_code(c.execute("UPDATE orders SET customer_id = 42 WHERE id = 10", &[]), "23503");
}

#[test]
fn fk_delete_parent_with_children_restricts() {
    let (_db, mut c) = fk_setup();
    c.execute("INSERT INTO orders (id, customer_id) VALUES (10, 1)", &[]).expect("seed");
    expect_code(c.execute("DELETE FROM customers WHERE id = 1", &[]), "23503");
    let id: i32 = c.query_one("SELECT id FROM customers WHERE id = 1", &[]).expect("readback").get(0);
    assert_eq!(id, 1);
}

#[test]
fn fk_delete_parent_without_children_ok() {
    let (_db, mut c) = fk_setup();
    c.execute("DELETE FROM customers WHERE id = 1", &[]).expect("delete");
}

#[test]
fn fk_null_value_allowed() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY);
         CREATE TABLE notes (id int PRIMARY KEY, customer_id int REFERENCES customers(id));",
    )
    .expect("setup");
    // MATCH SIMPLE: a NULL in a nullable FK column skips the existence check.
    c.execute("INSERT INTO notes (id, customer_id) VALUES ($1, $2)", &[&1i32, &None::<i32>]).expect("insert");
}

#[test]
fn fk_visible_within_transaction() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY);
         CREATE TABLE notes (id int PRIMARY KEY, customer_id int NOT NULL REFERENCES customers(id));",
    )
    .expect("setup");
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO customers (id) VALUES (1)", &[]).expect("INSERT customer");
    c.execute("INSERT INTO notes (id, customer_id) VALUES (10, 1)", &[]).expect("child INSERT in same tx");
    c.batch_execute("COMMIT").expect("COMMIT");
}

#[test]
fn replica_role_skips_foreign_key_enforcement_and_actions() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE rp (id int PRIMARY KEY);
         CREATE TABLE rc (id int, pid int REFERENCES rp ON DELETE CASCADE ON UPDATE CASCADE);
         CREATE TABLE rn (id int, pid int REFERENCES rp ON DELETE SET NULL);
         CREATE TABLE rd (id int, pid int REFERENCES rp DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO rp VALUES (1), (2); INSERT INTO rc VALUES (1, 1); INSERT INTO rn VALUES (1, 2);
         SET session_replication_role = replica",
    )
    .expect("setup");
    c.batch_execute("INSERT INTO rc VALUES (2, 99); UPDATE rc SET pid = 98 WHERE id = 1; INSERT INTO rd VALUES (1, 77)")
        .expect("no fk check");
    // Referential actions do not fire either: the children keep their keys.
    c.batch_execute("DELETE FROM rp WHERE id = 1; DELETE FROM rp WHERE id = 2; UPDATE rp SET id = 5 WHERE id = 3").expect("no actions");
    assert_eq!(
        texts(&mut c, "SELECT id, pid FROM rc ORDER BY id"),
        [[Some("1".into()), Some("98".into())], [Some("2".into()), Some("99".into())]]
    );
    assert_eq!(texts(&mut c, "SELECT id, pid FROM rn"), [[Some("1".into()), Some("2".into())]]);
    // A deferred foreign key is not checked at commit.
    c.batch_execute("BEGIN; INSERT INTO rd VALUES (2, 66); COMMIT").expect("deferred");
    c.batch_execute("SET session_replication_role = origin").expect("origin");
    // Back in the origin role the constraints enforce again, and ADD CONSTRAINT validates.
    let e = expect_code(c.batch_execute("INSERT INTO rc VALUES (3, 99)"), "23503");
    assert_eq!(message(&e), "insert or update on table \"rc\" violates foreign key constraint \"rc_pid_fkey\"");
    c.batch_execute("SET session_replication_role = replica").expect("replica");
    expect_code(c.batch_execute("ALTER TABLE rn ADD CONSTRAINT rn_fk2 FOREIGN KEY (id) REFERENCES rp"), "23503");
}
