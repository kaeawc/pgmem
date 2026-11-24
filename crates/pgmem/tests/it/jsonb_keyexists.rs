use crate::support::{TestDb, setup};
use postgres::Client;
use serde_json::{Value, json};

fn fixture() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE docs (id int PRIMARY KEY, body jsonb NOT NULL)").expect("create");
    c.execute(
        "INSERT INTO docs (id, body) VALUES (1, $1), (2, $2), (3, $3)",
        &[&json!({"name": "alice", "tags": ["a", "b"]}), &json!({"name": "bob", "age": 25}), &json!(["x", "y", "z"])],
    )
    .expect("insert");
    (db, c)
}

fn has_key(c: &mut Client, doc: Value, key: &str) -> bool {
    let sql = format!("SELECT $1::jsonb ? '{key}'");
    c.query_one(&sql, &[&doc]).unwrap_or_else(|e| panic!("{sql}: {e}")).get(0)
}

#[test]
fn json_key_exists_object() {
    let (_db, mut c) = fixture();
    let rows = c.query("SELECT id FROM docs WHERE body ? 'age' ORDER BY id", &[]).expect("query");
    let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![2]);
}

#[test]
fn json_key_exists_array_element() {
    let (_db, mut c) = fixture();
    assert!(has_key(&mut c, json!(["x", "y", "z"]), "y"));
}

#[test]
fn json_key_exists_not_present() {
    let (_db, mut c) = fixture();
    assert!(!has_key(&mut c, json!({"a": 1}), "missing"));
}

#[test]
fn json_key_exists_nested_key_not_checked() {
    let (_db, mut c) = fixture();
    // `?` is top-level only: `b` lives nested under `a`, so it's reported as absent.
    assert!(!has_key(&mut c, json!({"a": {"b": 1}}), "b"));
}
