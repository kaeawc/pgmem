//! Port of exec/insert_returning_test.go as SQL-level tests.

use crate::support::{expect_code, setup};
use postgres::Client;

fn create(c: &mut Client) {
    c.batch_execute("CREATE TABLE events (id int4 NOT NULL UNIQUE, kind text NOT NULL)").expect("create");
}

#[test]
fn insert_returning_emits_projected_rows() {
    let (_db, mut c) = setup();
    create(&mut c);
    let rows = c.query("INSERT INTO events (id, kind) VALUES (1, 'click'), (2, 'view') RETURNING id, kind", &[]).expect("insert");
    let got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(1, "click".to_owned()), (2, "view".to_owned())]);
}

#[test]
fn insert_returning_partial_columns() {
    let (_db, mut c) = setup();
    create(&mut c);
    let rows = c.query("INSERT INTO events (id, kind) VALUES (99, 'ping') RETURNING id", &[]).expect("insert");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);
    assert_eq!(rows[0].get::<_, i32>(0), 99);
}

#[test]
fn insert_returning_empty_on_failure() {
    let (_db, mut c) = setup();
    create(&mut c);
    c.execute("INSERT INTO events (id, kind) VALUES (1, 'ping')", &[]).expect("seed insert");
    expect_code(c.query("INSERT INTO events (id, kind) VALUES (1, 'again') RETURNING id", &[]), "23505");
    let n: i64 = c.query_one("SELECT count(*) FROM events", &[]).expect("count").get(0);
    assert_eq!(n, 1);
}
