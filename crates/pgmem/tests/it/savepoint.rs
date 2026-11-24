use crate::support::{TestDb, expect_code, setup};
use postgres::Client;

/// A fresh database with an `items` table and one pinned connection.
fn savepoint_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE items (id int PRIMARY KEY, label text NOT NULL)").expect("CREATE");
    (db, c)
}

fn item_count(c: &mut Client) -> usize {
    c.query("SELECT id FROM items", &[]).expect("SELECT").len()
}

#[test]
fn savepoint_rollback_to_recovers_from_poisoned_block() {
    let (_db, mut c) = savepoint_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO items (id, label) VALUES (1, 'a')", &[]).expect("INSERT 1");
    c.batch_execute("SAVEPOINT before_dup").expect("SAVEPOINT");
    expect_code(c.execute("INSERT INTO items (id, label) VALUES (1, 'dup')", &[]), "23505");
    // Without ROLLBACK TO this would be 25P02; with it, the block recovers.
    c.batch_execute("ROLLBACK TO SAVEPOINT before_dup").expect("ROLLBACK TO");
    c.execute("INSERT INTO items (id, label) VALUES (2, 'b')", &[]).expect("INSERT after recovery");
    c.batch_execute("COMMIT").expect("COMMIT");
    assert_eq!(item_count(&mut c), 2, "id=1 (pre-savepoint) + id=2 (post-recovery)");
}

#[test]
fn savepoint_release_does_not_undo_work() {
    let (_db, mut c) = savepoint_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    c.batch_execute("SAVEPOINT s1").expect("SAVEPOINT");
    c.execute("INSERT INTO items (id, label) VALUES (1, 'a')", &[]).expect("INSERT");
    c.batch_execute("RELEASE SAVEPOINT s1").expect("RELEASE");
    c.batch_execute("COMMIT").expect("COMMIT");
    assert_eq!(item_count(&mut c), 1);
}

#[test]
fn savepoint_outside_tx_is_rejected() {
    let (_db, mut c) = savepoint_setup();
    expect_code(c.batch_execute("SAVEPOINT s1"), "25P01");
}

#[test]
fn savepoint_unknown_name_rejects() {
    let (_db, mut c) = savepoint_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    expect_code(c.batch_execute("ROLLBACK TO SAVEPOINT does_not_exist"), "3B001");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
}
