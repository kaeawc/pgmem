use crate::support::{TestDb, setup};
use postgres::Client;
use serde_json::{Value, json};

fn fixture() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE docs (id int PRIMARY KEY, body jsonb NOT NULL)").expect("create");
    c.execute(
        "INSERT INTO docs (id, body) VALUES (1, $1), (2, $2), (3, $3)",
        &[
            &json!({"name": "alice", "tags": ["a", "b"], "age": 30}),
            &json!({"name": "bob", "tags": ["b", "c"], "age": 25}),
            &json!({"name": "carol", "tags": []}),
        ],
    )
    .expect("insert");
    (db, c)
}

fn ids(c: &mut Client, sql: &str, arg: Value) -> Vec<i32> {
    c.query(sql, &[&arg]).unwrap_or_else(|e| panic!("{sql}: {e}")).iter().map(|r| r.get(0)).collect()
}

fn holds(c: &mut Client, sql: &str, a: Value, b: Value) -> bool {
    c.query_one(sql, &[&a, &b]).unwrap_or_else(|e| panic!("{sql}: {e}")).get(0)
}

#[test]
fn json_contains_object_key_match() {
    let (_db, mut c) = fixture();
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE body @> $1::jsonb ORDER BY id", json!({"name": "alice"})), [1]);
}

#[test]
fn json_contains_array_element_match() {
    let (_db, mut c) = fixture();
    // Rows whose `tags` array contains "b".
    assert_eq!(ids(&mut c, "SELECT id FROM docs WHERE body -> 'tags' @> $1::jsonb ORDER BY id", json!(["b"])), [1, 2]);
}

#[test]
fn json_contained_reverse() {
    let (_db, mut c) = fixture();
    assert!(holds(&mut c, "SELECT $1::jsonb <@ $2::jsonb", json!({"name": "alice"}), json!({"name": "alice", "tags": ["a", "b"]})));
}

#[test]
fn json_contains_negative() {
    let (_db, mut c) = fixture();
    assert!(!holds(&mut c, "SELECT $1::jsonb @> $2::jsonb", json!({"a": 1}), json!({"a": 2})));
}

#[test]
fn json_contains_nested() {
    let (_db, mut c) = fixture();
    assert!(holds(&mut c, "SELECT $1::jsonb @> $2::jsonb", json!({"a": {"b": 1, "c": 2}}), json!({"a": {"b": 1}})));
}
