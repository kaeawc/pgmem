//! Port of exec/update_test.go as SQL-level tests.

use crate::support::{expect_code, setup};
use postgres::Client;

fn fixture(c: &mut Client) {
    c.batch_execute(
        "CREATE TABLE items (id int4 NOT NULL UNIQUE, label text, qty int4 NOT NULL, CONSTRAINT items_qty_check CHECK (qty > 0));
         INSERT INTO items VALUES (1, 'alpha', 10), (2, 'beta', 20), (3, 'gamma', 30);",
    )
    .expect("fixture");
}

fn snapshot(c: &mut Client) -> Vec<(i32, Option<String>, i32)> {
    c.query("SELECT id, label, qty FROM items ORDER BY id", &[]).expect("snapshot").iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect()
}

fn rows(v: &[(i32, &str, i32)]) -> Vec<(i32, Option<String>, i32)> {
    v.iter().map(|&(id, l, q)| (id, Some(l.to_owned()), q)).collect()
}

#[test]
fn update_where_matches_one() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    c.execute("UPDATE items SET label = 'renamed' WHERE id = 2", &[]).expect("update");
    assert_eq!(snapshot(&mut c), rows(&[(1, "alpha", 10), (2, "renamed", 20), (3, "gamma", 30)]));
}

#[test]
fn update_assignments_see_original() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    c.execute("UPDATE items SET id = qty, qty = id WHERE id = 1", &[]).expect("update");
    assert_eq!(snapshot(&mut c), rows(&[(2, "beta", 20), (3, "gamma", 30), (10, "alpha", 1)]));
}

#[test]
fn update_returning() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    let out = c.query("UPDATE items SET label = 'X' WHERE id > 1 RETURNING id, label", &[]).expect("update");
    let mut got: Vec<(i32, String)> = out.iter().map(|r| (r.get(0), r.get(1))).collect();
    got.sort();
    assert_eq!(got, vec![(2, "X".to_owned()), (3, "X".to_owned())]);
}

#[test]
fn update_not_null_violation() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    expect_code(c.execute("UPDATE items SET qty = NULL WHERE id = 2", &[]), "23502");
    assert_eq!(snapshot(&mut c), rows(&[(1, "alpha", 10), (2, "beta", 20), (3, "gamma", 30)]));
}

#[test]
fn update_unique_violation() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    expect_code(c.execute("UPDATE items SET id = 1 WHERE id = 2", &[]), "23505");
    assert_eq!(snapshot(&mut c), rows(&[(1, "alpha", 10), (2, "beta", 20), (3, "gamma", 30)]));
}

#[test]
fn update_check_violation() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    expect_code(c.execute("UPDATE items SET qty = 0 WHERE id = 2", &[]), "23514");
    assert_eq!(snapshot(&mut c), rows(&[(1, "alpha", 10), (2, "beta", 20), (3, "gamma", 30)]));
}

#[test]
fn update_no_match_leaves_intact() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    let n = c.execute("UPDATE items SET label = 'ignored' WHERE id = 999", &[]).expect("update");
    assert_eq!(n, 0);
    assert_eq!(snapshot(&mut c), rows(&[(1, "alpha", 10), (2, "beta", 20), (3, "gamma", 30)]));
}
