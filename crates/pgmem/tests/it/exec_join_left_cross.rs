//! Port of exec/join_left_cross_test.go as SQL-level tests.

use crate::exec_join::join_fixture;
use crate::support::setup;

#[test]
fn left_join_pads_unmatched() {
    let (_db, mut c) = setup();
    join_fixture(&mut c);
    let rows = c.query("SELECT users.name, orders.label FROM users LEFT JOIN orders ON users.id = orders.user_id", &[]).expect("join");
    let mut got: Vec<(String, Option<String>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    // Option sorts None first; Go sorted nil labels last — compare as a sorted set either way.
    got.sort();
    let want = vec![
        ("alice".to_owned(), Some("alice-order-A".to_owned())),
        ("alice".to_owned(), Some("alice-order-B".to_owned())),
        ("bob".to_owned(), Some("bob-order".to_owned())),
        ("carol".to_owned(), None),
    ];
    assert_eq!(got, want);
}

#[test]
fn cross_join_cartesian() {
    let (_db, mut c) = setup();
    join_fixture(&mut c);
    let rows = c.query("SELECT users.id FROM users CROSS JOIN orders", &[]).expect("cross join");
    assert_eq!(rows.len(), 9);
}
