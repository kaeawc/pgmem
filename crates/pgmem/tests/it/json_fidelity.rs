//! json formatting, json vs jsonb operators, jsonpath, SQL/JSON and record conversion.

use crate::support::{expect_code, message, setup, text_value, texts};

fn one(c: &mut postgres::Client, sql: &str) -> String {
    text_value(c, sql).unwrap_or_else(|| "NULL".into())
}

#[test]
fn json_output_formats_like_postgres() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT json_build_object('a', 1, 'b', 'two', 'c', NULL)"), r#"{"a" : 1, "b" : "two", "c" : null}"#);
    assert_eq!(one(&mut c, "SELECT json_build_array(1, 'a', NULL, 2.5)"), r#"[1, "a", null, 2.5]"#);
    assert_eq!(
        one(&mut c, "SELECT row_to_json(r) FROM (SELECT 1 AS id, 'x' AS name, NULL::int AS z) r"),
        r#"{"id":1,"name":"x","z":null}"#
    );
    assert_eq!(one(&mut c, "SELECT row_to_json(ROW(1, 'foo'))"), r#"{"f1":1,"f2":"foo"}"#);
    assert_eq!(
        one(&mut c, "SELECT json_agg(r) FROM (VALUES (1, 'a'), (2, 'b')) r(id, name)"),
        "[{\"id\":1,\"name\":\"a\"}, \n {\"id\":2,\"name\":\"b\"}]"
    );
    assert_eq!(one(&mut c, "SELECT json_object_agg(k, v ORDER BY k) FROM (VALUES ('b', 2), ('a', 1)) t(k, v)"), r#"{ "a" : 1, "b" : 2 }"#);
    assert_eq!(one(&mut c, "SELECT to_jsonb(r) FROM (SELECT 1 AS id, ARRAY['a'] AS tags) r"), r#"{"id": 1, "tags": ["a"]}"#);
    assert_eq!(one(&mut c, "SELECT to_jsonb('Infinity'::numeric)"), r#""Infinity""#);
    assert_eq!(one(&mut c, "SELECT '[1, 2.50, 1e2, -0]'::jsonb"), "[1, 2.50, 100, 0]");
    let e = expect_code(c.simple_query(r#"SELECT '"\u0000"'::jsonb"#), "22P05");
    assert_eq!(message(&e), "unsupported Unicode escape sequence");
    // An ordered aggregate sorts the group's input for the other aggregates too (PG 16+).
    assert_eq!(
        texts(&mut c, "SELECT jsonb_agg(x ORDER BY x), json_agg(x) FROM (VALUES (2), (1)) v(x)"),
        vec![vec![Some("[1, 2]".into()), Some("[1, 2]".into())]]
    );
}

#[test]
fn json_operators_return_source_text() {
    let (_db, mut c) = setup();
    let row = c.query_one(r#"SELECT '{"a": [1,  2]}'::json -> 'a', pg_typeof('{"a": 1}'::json -> 'a')::text"#, &[]).expect("query");
    assert_eq!(row.get::<_, serde_json::Value>(0), serde_json::json!([1, 2]));
    assert_eq!(row.get::<_, String>(1), "json");
    assert_eq!(one(&mut c, r#"SELECT '{"a": [1,  2]}'::json -> 'a'"#), "[1,  2]");
    assert_eq!(
        texts(&mut c, "SELECT * FROM json_array_elements('[true, [1]]')"),
        vec![vec![Some("true".into())], vec![Some("[1]".into())]]
    );
    assert_eq!(one(&mut c, r#"SELECT ('{"a": {"b": 5}}'::jsonb)['a']['b']"#), "5");
    let e = expect_code(c.simple_query("SELECT * FROM jsonb_array_elements('{\"a\": 1}')"), "22023");
    assert_eq!(message(&e), "cannot extract elements from an object");
}

#[test]
fn jsonpath_queries_and_canonical_text() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT '$.a ? (@ > 0 && @ < 5 || @ == 10)'::jsonpath"), r#"$."a"?(@ > 0 && @ < 5 || @ == 10)"#);
    assert_eq!(one(&mut c, "SELECT '$.a + 2 * (3 - $.b)'::jsonpath"), r#"($."a" + 2 * (3 - $."b"))"#);
    assert_eq!(
        texts(
            &mut c,
            r#"SELECT '{"a": 1}'::jsonb @? '$.a ? (@ > 0)', '{"a": 1}'::jsonb @@ '$.a == 1', jsonb_path_query_array('{"a": [1, 2, 3]}', '$.a[*] ? (@ > 1)'), jsonb_path_query_first('{"a": [1, 2]}', '$.a[last]')"#
        ),
        vec![vec![Some("t".into()), Some("t".into()), Some("[2, 3]".into()), Some("2".into())]]
    );
    assert_eq!(
        texts(&mut c, r#"SELECT * FROM jsonb_path_query('{"x": [1, 5, 9]}', '$.x[*] ? (@ >= $min)', '{"min": 5}')"#),
        vec![vec![Some("5".into())], vec![Some("9".into())]]
    );
    let e = expect_code(c.simple_query(r#"SELECT jsonb_path_query_first('{"a": 1}', 'strict $.b')"#), "2203A");
    assert_eq!(message(&e), "JSON object does not contain key \"b\"");
    let e = expect_code(c.simple_query("SELECT '@.a'::jsonpath"), "42601");
    assert_eq!(message(&e), "@ is not allowed in root expressions");
}

#[test]
fn sql_json_functions() {
    let (_db, mut c) = setup();
    assert_eq!(
        texts(
            &mut c,
            r#"SELECT '{"a": 1}' IS JSON, '[1]' IS JSON ARRAY, '{"a": 1}' IS JSON SCALAR, 'abc' IS JSON, '{"a": 1, "a": 2}' IS JSON WITH UNIQUE KEYS"#
        ),
        vec![vec![Some("t".into()), Some("t".into()), Some("f".into()), Some("f".into()), Some("f".into())]]
    );
    assert_eq!(
        texts(&mut c, "SELECT JSON_OBJECT('a' VALUE 1, 'b' : 2), JSON_ARRAY(1, 'x', NULL), JSON_ARRAY(1, NULL NULL ON NULL)"),
        vec![vec![Some(r#"{"a" : 1, "b" : 2}"#.into()), Some(r#"[1, "x"]"#.into()), Some("[1, null]".into())]]
    );
    assert_eq!(
        texts(
            &mut c,
            r#"SELECT JSON_VALUE(jsonb '{"a": 42}', '$.a' RETURNING int), JSON_VALUE(jsonb '{"a": "x"}', '$.a' RETURNING int DEFAULT -1 ON ERROR), JSON_VALUE(jsonb '{"a": "x"}', '$.b' DEFAULT 'none' ON EMPTY), JSON_QUERY(jsonb '{"a": [1, 2]}', '$.a[*]' WITH WRAPPER), JSON_EXISTS(jsonb '{"a": [1, 2]}', '$.a[1]')"#
        ),
        vec![vec![Some("42".into()), Some("-1".into()), Some("none".into()), Some("[1, 2]".into()), Some("t".into())]]
    );
    let e = expect_code(c.simple_query(r#"SELECT JSON_VALUE(jsonb '{"a": [1]}', '$.a' ERROR ON ERROR)"#), "2203F");
    assert_eq!(message(&e), "JSON path expression in JSON_VALUE must return single scalar item");
    assert_eq!(
        texts(
            &mut c,
            r#"SELECT * FROM JSON_TABLE(jsonb '{"items": [{"x": 1, "t": [1, 2]}, {"x": 2}]}', '$.items[*]' COLUMNS (ord FOR ORDINALITY, x int PATH '$.x', NESTED PATH '$.t[*]' COLUMNS (tv int PATH '$'))) jt"#
        ),
        vec![
            vec![Some("1".into()), Some("1".into()), Some("1".into())],
            vec![Some("1".into()), Some("1".into()), Some("2".into())],
            vec![Some("2".into()), Some("2".into()), None],
        ]
    );
}

#[test]
fn json_records() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE person AS (name text, age int, tags text[])").expect("type");
    assert_eq!(
        texts(&mut c, r#"SELECT * FROM json_populate_record(NULL::person, '{"name": "Ann", "age": 30, "tags": ["x", "y"], "extra": 1}')"#),
        vec![vec![Some("Ann".into()), Some("30".into()), Some("{x,y}".into())]]
    );
    assert_eq!(
        texts(&mut c, r#"SELECT * FROM json_to_recordset('[{"a": 1}, {"a": 2, "b": true}]') AS t(a int, b bool)"#),
        vec![vec![Some("1".into()), None], vec![Some("2".into()), Some("t".into())]]
    );
    let e = expect_code(c.simple_query(r#"SELECT * FROM jsonb_populate_record(NULL::person, '{"age": "old"}')"#), "22P02");
    assert_eq!(message(&e), "invalid input syntax for type integer: \"old\"");
    assert_eq!(
        texts(
            &mut c,
            r#"SELECT jsonb_set_lax('{"a": 1, "b": 2}', '{a}', NULL, true, 'delete_key'), jsonb_insert('{"a": [0, 1, 2]}', '{a,1}', '"new"', true)"#
        ),
        vec![vec![Some(r#"{"b": 2}"#.into()), Some(r#"{"a": [0, 1, "new", 2]}"#.into())]]
    );
}
