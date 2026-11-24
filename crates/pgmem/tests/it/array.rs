use crate::support::{TestDb, setup};
use postgres::Client;

fn fixture() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE users (id bigint PRIMARY KEY, name text NOT NULL);
         INSERT INTO users (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'carol'), (4, 'dan')",
    )
    .expect("setup");
    (db, c)
}

#[test]
fn array_any_bigint() {
    let (_db, mut c) = fixture();
    let ids: Vec<i64> = vec![1, 3];
    let rows = c.query("SELECT id, name FROM users WHERE id = ANY($1::bigint[]) ORDER BY id", &[&ids]).expect("query");
    let got: Vec<(i64, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![(1, "alice".to_owned()), (3, "carol".to_owned())]);
}

#[test]
fn array_any_text() {
    let (_db, mut c) = fixture();
    let names = vec!["bob", "dan"];
    let rows = c.query("SELECT id FROM users WHERE name = ANY($1::text[]) ORDER BY id", &[&names]).expect("query");
    let ids: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![2, 4]);
}

#[test]
fn array_empty_array_matches_nothing() {
    let (_db, mut c) = fixture();
    let got: i64 = c.query_one("SELECT count(*) FROM users WHERE id = ANY($1::bigint[])", &[&Vec::<i64>::new()]).expect("query").get(0);
    assert_eq!(got, 0);
}

#[test]
fn array_bigint_literal() {
    let (_db, mut c) = fixture();
    let rows = c.query("SELECT id FROM users WHERE id = ANY($1::bigint[]) ORDER BY id", &[&vec![2i64, 4]]).expect("query");
    let ids: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![2, 4]);
}
