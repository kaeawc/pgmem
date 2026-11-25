use crate::support::{RawConn, TestDb, expect_code, setup};
use postgres::Client;

/// A fresh database with an `accounts` table and one pinned connection.
fn txn_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE accounts (id int PRIMARY KEY, name text NOT NULL)").expect("CREATE");
    (db, c)
}

fn count_accounts(c: &mut Client) -> usize {
    c.query("SELECT id FROM accounts", &[]).expect("SELECT").len()
}

#[test]
fn txn_rollback_undoes_insert() {
    let (_db, mut c) = txn_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice')", &[]).expect("INSERT");
    assert_eq!(count_accounts(&mut c), 1, "in-tx count");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    assert_eq!(count_accounts(&mut c), 0, "post-rollback count");
}

#[test]
fn txn_commit_persists_insert() {
    let (_db, mut c) = txn_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice')", &[]).expect("INSERT");
    c.batch_execute("COMMIT").expect("COMMIT");
    assert_eq!(count_accounts(&mut c), 1, "post-commit count");
}

#[test]
fn txn_aborted_state_poisons_block() {
    let (_db, mut c) = txn_setup();
    c.batch_execute("BEGIN").expect("BEGIN");
    c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice')", &[]).expect("first INSERT");
    expect_code(c.execute("INSERT INTO accounts (id, name) VALUES (1, 'alice-clone')", &[]), "23505");
    expect_code(c.execute("INSERT INTO accounts (id, name) VALUES (2, 'bob')", &[]), "25P02");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    c.execute("INSERT INTO accounts (id, name) VALUES (2, 'bob')", &[]).expect("post-rollback INSERT");
    assert_eq!(count_accounts(&mut c), 1, "only bob: the first txn's alice was rolled back");
}

#[test]
fn txn_commit_after_error_acts_as_rollback() {
    let (db, mut c) = txn_setup();
    let mut raw = RawConn::connect(&db, &mut c);
    assert_eq!(raw.simple_query("BEGIN"), (vec!["BEGIN".to_owned()], vec![]));
    assert_eq!(raw.simple_query("INSERT INTO accounts (id, name) VALUES (1, 'alice')").1, Vec::<String>::new());
    // Poisons the transaction.
    assert_eq!(raw.simple_query("INSERT INTO accounts (id, name) VALUES (1, 'dup')").1, vec!["23505".to_owned()]);
    let (tags, errors) = raw.simple_query("COMMIT");
    assert!(errors.is_empty(), "COMMIT errored: {errors:?}");
    assert_eq!(tags, vec!["ROLLBACK".to_owned()], "CommandComplete tag");
    assert_eq!(count_accounts(&mut c), 0, "commit-after-error must roll back");
}

#[test]
fn txn_implicit_autocommit() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE bare (n int NOT NULL)").expect("CREATE");
    c.execute("INSERT INTO bare (n) VALUES (1), (2), (3)", &[]).expect("INSERT");
    // A different connection sees the rows, so they were committed.
    let mut other = db.connect();
    let rows = other.query("SELECT n FROM bare", &[]).expect("SELECT");
    let mut got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2, 3]);
}
