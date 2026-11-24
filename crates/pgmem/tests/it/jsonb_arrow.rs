use crate::support::{TestDb, Text, setup};
use postgres::Client;
use serde_json::{Value, json};

fn fixture() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE docs (id int PRIMARY KEY, body jsonb NOT NULL)").expect("create");
    c.execute(
        "INSERT INTO docs (id, body) VALUES (1, $1), (2, $2)",
        &[&json!({"name": "alice", "age": 30, "tags": ["a", "b", "c"]}), &json!({"name": "bob", "age": 25, "tags": []})],
    )
    .expect("insert");
    (db, c)
}

#[test]
fn json_arrow_text_key() {
    let (_db, mut c) = fixture();
    let got: String = c.query_one("SELECT body ->> 'name' FROM docs WHERE id = 1", &[]).expect("query").get(0);
    assert_eq!(got, "alice");
}

#[test]
fn json_arrow_json_key() {
    let (_db, mut c) = fixture();
    let row = c.query_one("SELECT body -> 'tags', body -> 'tags' FROM docs WHERE id = 1", &[]).expect("query");
    // Deviation from pgmem-go: raw `["a","b","c"]` vs Postgres' canonical jsonb text `["a", "b", "c"]`.
    let text: Text = row.get(0);
    assert_eq!(text, r#"["a", "b", "c"]"#);
    let value: Value = row.get(1);
    assert_eq!(value, json!(["a", "b", "c"]));
}

#[test]
fn json_arrow_filter_by_text() {
    let (_db, mut c) = fixture();
    let rows = c.query("SELECT id FROM docs WHERE body ->> 'name' = 'alice'", &[]).expect("query");
    let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![1]);
}

#[test]
fn json_arrow_array_index() {
    let (_db, mut c) = fixture();
    let got: String = c.query_one("SELECT body -> 'tags' ->> 1 FROM docs WHERE id = 1", &[]).expect("query").get(0);
    assert_eq!(got, "b");
}

#[test]
fn json_arrow_missing_key_is_null() {
    let (_db, mut c) = fixture();
    let got: Option<String> = c.query_one("SELECT body ->> 'missing' FROM docs WHERE id = 1", &[]).expect("query").get(0);
    assert_eq!(got, None);
}

#[test]
fn json_arrow_out_of_range_is_null() {
    let (_db, mut c) = fixture();
    let got: Option<String> = c.query_one("SELECT body -> 'tags' ->> 99 FROM docs WHERE id = 1", &[]).expect("query").get(0);
    assert_eq!(got, None);
}
