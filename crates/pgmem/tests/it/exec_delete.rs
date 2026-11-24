//! Port of exec/delete_test.go as SQL-level tests.

use crate::support::setup;
use postgres::Client;

fn fixture(c: &mut Client) {
    c.batch_execute(
        "CREATE TABLE items (id int4 NOT NULL UNIQUE, label text);
         INSERT INTO items VALUES (1, 'alpha'), (2, 'beta'), (3, 'gamma');",
    )
    .expect("fixture");
}

fn remaining_ids(c: &mut Client) -> Vec<i32> {
    c.query("SELECT id FROM items ORDER BY id", &[]).expect("select ids").iter().map(|r| r.get(0)).collect()
}

#[test]
fn delete_where_match_one() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    let n = c.execute("DELETE FROM items WHERE id = 2", &[]).expect("delete");
    assert_eq!(n, 1);
    assert_eq!(remaining_ids(&mut c), vec![1, 3]);
}

#[test]
fn delete_no_where_deletes_all() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    c.execute("DELETE FROM items", &[]).expect("delete");
    assert!(remaining_ids(&mut c).is_empty());
}

#[test]
fn delete_returning() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    let rows = c.query("DELETE FROM items WHERE id > 1 RETURNING id, label", &[]).expect("delete returning");
    let mut got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    got.sort();
    assert_eq!(got, vec![(2, "beta".to_owned()), (3, "gamma".to_owned())]);
    assert_eq!(remaining_ids(&mut c), vec![1]);
}

#[test]
fn delete_no_match_leaves_table_intact() {
    let (_db, mut c) = setup();
    fixture(&mut c);
    let n = c.execute("DELETE FROM items WHERE id = 999", &[]).expect("delete");
    assert_eq!(n, 0);
    assert_eq!(remaining_ids(&mut c), vec![1, 2, 3]);
}
