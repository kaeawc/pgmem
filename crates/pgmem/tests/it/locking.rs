use crate::support::{TestDb, setup};
use postgres::Client;

fn lock_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE jobs (id int PRIMARY KEY, status text NOT NULL)").expect("CREATE");
    c.execute("INSERT INTO jobs (id, status) VALUES (1, 'queued'), (2, 'queued'), (3, 'done')", &[]).expect("INSERT");
    (db, c)
}

fn ids(c: &mut Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e:?}")).iter().map(|r| r.get(0)).collect()
}

#[test]
fn locking_for_update() {
    let (_db, mut c) = lock_setup();
    assert_eq!(ids(&mut c, "SELECT id FROM jobs WHERE status = 'queued' ORDER BY id FOR UPDATE"), vec![1, 2]);
}

#[test]
fn locking_for_update_skip_locked() {
    let (_db, mut c) = lock_setup();
    assert_eq!(ids(&mut c, "SELECT id FROM jobs WHERE status = 'queued' ORDER BY id LIMIT 1 FOR UPDATE SKIP LOCKED"), vec![1]);
}

#[test]
fn locking_for_share() {
    let (_db, mut c) = lock_setup();
    assert_eq!(ids(&mut c, "SELECT id FROM jobs ORDER BY id FOR SHARE").len(), 3);
}

#[test]
fn locking_for_no_key_update() {
    let (_db, mut c) = lock_setup();
    assert_eq!(ids(&mut c, "SELECT id FROM jobs WHERE id = 1 FOR NO KEY UPDATE"), vec![1]);
}

#[test]
fn locking_for_key_share_of() {
    let (_db, mut c) = lock_setup();
    assert_eq!(ids(&mut c, "SELECT id FROM jobs WHERE id = 2 FOR KEY SHARE OF jobs NOWAIT"), vec![2]);
}
