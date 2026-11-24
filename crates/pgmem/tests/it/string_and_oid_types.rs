//! varchar, bpchar, name, "char", json, oid, the reg* aliases and void: distinct types with
//! their own OIDs, coercions and output, as drivers see them.

use crate::support::{expect_code, setup, text_value, texts};
use postgres::types::Type as PgType;

fn column_types(c: &mut postgres::Client, sql: &str) -> Vec<PgType> {
    c.prepare(sql).expect("prepare").columns().iter().map(|col| col.type_().clone()).collect()
}

#[test]
fn string_types_have_their_own_oids() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE s (v varchar(5), b char(4), t text)").expect("create");
    assert_eq!(column_types(&mut c, "SELECT v, b, t FROM s"), vec![PgType::VARCHAR, PgType::BPCHAR, PgType::TEXT]);
    // Functions resolve through text and return text.
    assert_eq!(column_types(&mut c, "SELECT upper(v), b || 'x', current_schema() FROM s"), vec![PgType::TEXT, PgType::TEXT, PgType::NAME]);
}

#[test]
fn bpchar_pads_and_ignores_trailing_spaces() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE p (b char(4)); INSERT INTO p VALUES ('ab')").expect("setup");
    assert_eq!(text_value(&mut c, "SELECT b FROM p").as_deref(), Some("ab  "));
    assert_eq!(text_value(&mut c, "SELECT b = 'ab' FROM p").as_deref(), Some("t"));
    assert_eq!(text_value(&mut c, "SELECT count(*) FROM p WHERE b IN ('ab', 'cd')").as_deref(), Some("1"));
    // Casting to text drops the padding; octet_length counts it.
    assert_eq!(
        texts(&mut c, "SELECT b::text, length(b), octet_length(b) FROM p"),
        vec![vec![Some("ab".to_owned()), Some("2".to_owned()), Some("4".to_owned())]]
    );
    assert_eq!(text_value(&mut c, "SELECT 'abc'::char").as_deref(), Some("a"));
    let e = expect_code(c.execute("INSERT INTO p VALUES ('abcde')", &[]), "22001");
    assert_eq!(e.as_db_error().map(|d| d.message().to_owned()).as_deref(), Some("value too long for type character(4)"));
}

#[test]
fn name_and_internal_char() {
    let (_db, mut c) = setup();
    let long = "x".repeat(70);
    assert_eq!(text_value(&mut c, &format!("SELECT length('{long}'::name)")).as_deref(), Some("63"));
    assert_eq!(
        texts(&mut c, "SELECT 'relkind'::\"char\", pg_typeof('r'::\"char\")"),
        vec![vec![Some("r".to_owned()), Some("\"char\"".to_owned())]]
    );
}

#[test]
fn json_keeps_its_input_text() {
    let (_db, mut c) = setup();
    assert_eq!(text_value(&mut c, r#"SELECT '{"b":1,  "a":2}'::json"#).as_deref(), Some(r#"{"b":1,  "a":2}"#));
    assert_eq!(text_value(&mut c, r#"SELECT '{"b":1,  "a":2}'::json::jsonb"#).as_deref(), Some(r#"{"a": 2, "b": 1}"#));
    expect_code(c.simple_query("SELECT '{\"a\":1}'::json = '{\"a\":1}'::json"), "42883");
    expect_code(c.simple_query("SELECT '{bad'::json"), "22P02");
}

#[test]
fn regclass_and_regtype() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE \"Mixed\" (id int); CREATE TABLE plain (id int)").expect("create");
    assert_eq!(text_value(&mut c, "SELECT 'plain'::regclass").as_deref(), Some("plain"));
    assert_eq!(text_value(&mut c, "SELECT '\"Mixed\"'::regclass").as_deref(), Some("\"Mixed\""));
    assert_eq!(text_value(&mut c, "SELECT 'plain'::regclass = 'public.plain'::regclass").as_deref(), Some("t"));
    // A relation's regclass and its OID round-trip.
    assert_eq!(text_value(&mut c, "SELECT ('plain'::regclass)::oid::regclass").as_deref(), Some("plain"));
    expect_code(c.simple_query("SELECT 'nope'::regclass"), "42P01");
    assert_eq!(
        texts(&mut c, "SELECT 'int4'::regtype, 'character varying(10)'::regtype, 'text[]'::regtype"),
        vec![vec![Some("integer".to_owned()), Some("character varying".to_owned()), Some("text[]".to_owned())]]
    );
    expect_code(c.simple_query("SELECT 'nope'::regtype"), "42704");
    assert_eq!(column_types(&mut c, "SELECT pg_typeof(1), 1::oid"), vec![PgType::REGTYPE, PgType::OID]);
}

#[test]
fn pg_typeof_keeps_aggregates() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE n (i int); INSERT INTO n VALUES (1), (2), (3)").expect("setup");
    assert_eq!(texts(&mut c, "SELECT pg_typeof(sum(i)) FROM n"), vec![vec![Some("bigint".to_owned())]]);
}

#[test]
fn void_results_print_empty() {
    let (_db, mut c) = setup();
    assert_eq!(column_types(&mut c, "SELECT pg_sleep(0)"), vec![PgType::VOID]);
    assert_eq!(text_value(&mut c, "SELECT pg_sleep(0)").as_deref(), Some(""));
}

#[test]
fn cast_columns_are_named_like_postgres() {
    let (_db, mut c) = setup();
    let names = |c: &mut postgres::Client, sql: &str| -> Vec<String> {
        c.prepare(sql).expect("prepare").columns().iter().map(|col| col.name().to_owned()).collect()
    };
    assert_eq!(
        names(&mut c, "SELECT true, true::int, '{1}'::int[], (1 + 1)::text, ARRAY[1]::text, (SELECT 1)::text"),
        vec!["?column?", "int4", "int4", "text", "array", "?column?"]
    );
    assert_eq!(names(&mut c, "SELECT x::text, substr('abc', 2) FROM (SELECT 1 AS x) s"), vec!["x", "substr"]);
}

#[test]
fn non_decimal_integer_input() {
    let (_db, mut c) = setup();
    assert_eq!(
        texts(&mut c, "SELECT '0x1F'::int, '0o17'::int, '-0b101'::int8"),
        vec![vec![Some("31".to_owned()), Some("15".to_owned()), Some("-5".to_owned())]]
    );
}
