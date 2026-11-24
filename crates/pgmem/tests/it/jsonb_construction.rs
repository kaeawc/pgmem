use crate::support::{Text, expect_code, message, setup};
use postgres::Client;

fn jsonb_text(c: &mut Client, sql: &str) -> Text {
    c.query_one(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).get(0)
}

#[test]
fn to_jsonb_number() {
    let (_db, mut c) = setup();
    assert_eq!(jsonb_text(&mut c, "SELECT to_jsonb(42)"), "42");
}

#[test]
fn to_jsonb_string() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: to_jsonb('hello') returned "hello" vs Postgres rejecting the unknown-typed literal.
    let e = expect_code(c.query_one("SELECT to_jsonb('hello')", &[]), "42804");
    assert_eq!(message(&e), "could not determine polymorphic type because input has type unknown");
    assert_eq!(jsonb_text(&mut c, "SELECT to_jsonb('hello'::text)"), r#""hello""#);
}

#[test]
fn jsonb_build_object_mixed() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: `{"name":"alice","age":30,"active":true}` vs Postgres' canonical jsonb
    // (keys sorted by length then bytes, ", " / ": " separators).
    let got = jsonb_text(&mut c, "SELECT jsonb_build_object('name', 'alice', 'age', 30, 'active', true)");
    assert_eq!(got, r#"{"age": 30, "name": "alice", "active": true}"#);
}

#[test]
fn jsonb_build_object_odd_args_fails() {
    let (_db, mut c) = setup();
    let e = expect_code(c.query_one("SELECT jsonb_build_object('k')", &[]), "22023");
    assert_eq!(message(&e), "argument list must have even number of elements");
}

#[test]
fn jsonb_build_object_null_key_fails() {
    let (_db, mut c) = setup();
    let e = expect_code(c.query_one("SELECT jsonb_build_object(NULL, 'v')", &[]), "22023");
    assert_eq!(message(&e), "argument 1: key must not be null");
}

#[test]
fn jsonb_build_array_mixed() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: `[1,"two",true,null]` vs Postgres' canonical `[1, "two", true, null]`.
    assert_eq!(jsonb_text(&mut c, "SELECT jsonb_build_array(1, 'two', true, NULL)"), r#"[1, "two", true, null]"#);
}

#[test]
fn jsonb_typeof() {
    let (_db, mut c) = setup();
    for (expr, want) in [
        (r#"jsonb_typeof('{"a":1}'::jsonb)"#, "object"),
        ("jsonb_typeof('[1,2]'::jsonb)", "array"),
        (r#"jsonb_typeof('"hi"'::jsonb)"#, "string"),
        ("jsonb_typeof('42'::jsonb)", "number"),
        ("jsonb_typeof('true'::jsonb)", "boolean"),
        ("jsonb_typeof('null'::jsonb)", "null"),
    ] {
        let got: String = c.query_one(&format!("SELECT {expr}"), &[]).unwrap_or_else(|e| panic!("{expr}: {e}")).get(0);
        assert_eq!(got, want, "{expr}");
    }
}

#[test]
fn jsonb_array_length() {
    let (_db, mut c) = setup();
    let n: i32 = c.query_one("SELECT jsonb_array_length('[1, 2, 3, 4]'::jsonb)", &[]).expect("query").get(0);
    assert_eq!(n, 4);
}

#[test]
fn jsonb_array_length_non_array_errors() {
    let (_db, mut c) = setup();
    let e = expect_code(c.query_one(r#"SELECT jsonb_array_length('{"a":1}'::jsonb)"#, &[]), "22023");
    assert_eq!(message(&e), "cannot get array length of a non-array");
}

#[test]
fn jsonb_build_object_nested_jsonb() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: `{"inner":{"x":1}}` vs Postgres' canonical `{"inner": {"x": 1}}`.
    let got = jsonb_text(&mut c, "SELECT jsonb_build_object('inner', jsonb_build_object('x', 1))");
    assert_eq!(got, r#"{"inner": {"x": 1}}"#);
}
