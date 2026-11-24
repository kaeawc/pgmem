use crate::support::{Text, expect_code, setup};
use serde_json::{Value, json};

#[test]
fn jsonb_round_trip() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE docs (id int PRIMARY KEY, body jsonb NOT NULL)").expect("create");
    let want = json!({"title": "first", "tags": [1, 2, 3]});
    c.execute("INSERT INTO docs (id, body) VALUES ($1, $2)", &[&1i32, &want]).expect("insert");
    let got: Value = c.query_one("SELECT body FROM docs WHERE id = $1", &[&1i32]).expect("readback").get(0);
    assert_eq!(got["title"], "first");
    assert_eq!(got["tags"], json!([1, 2, 3]));
}

#[test]
fn jsonb_unique_constraint() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE keyed (k jsonb PRIMARY KEY)").expect("create");
    let first = json!({"a": 1});
    c.execute("INSERT INTO keyed (k) VALUES ($1)", &[&first]).expect("first insert");
    expect_code(c.execute("INSERT INTO keyed (k) VALUES ($1)", &[&first]), "23505");
    c.execute("INSERT INTO keyed (k) VALUES ($1)", &[&json!({"a": 2})]).expect("distinct insert");
}

#[test]
fn jsonb_accepts_scalar_and_array() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE blobs (id int PRIMARY KEY, body jsonb NOT NULL)").expect("create");
    let bodies = ["123", r#""a string""#, "[1, 2, 3]", "true", "null"];
    for (id, body) in (1i32..).zip(bodies) {
        let v: Value = serde_json::from_str(body).expect("valid json");
        c.execute("INSERT INTO blobs (id, body) VALUES ($1, $2)", &[&id, &v]).unwrap_or_else(|e| panic!("body {body}: {e}"));
    }
    let rows = c.query("SELECT body FROM blobs ORDER BY id", &[]).expect("readback");
    let got: Vec<Text> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, bodies.map(|b| Text(b.to_owned())));
}
