use crate::support::{expect_code, setup};

fn one_string(c: &mut postgres::Client, sql: &str) -> String {
    c.query_one(sql, &[]).expect(sql).get(0)
}

fn one_array(c: &mut postgres::Client, sql: &str) -> Option<Vec<String>> {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn regexp_replace_first_match_by_default() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT regexp_replace('foo bar foo', 'foo', 'BAZ')"), "BAZ bar foo");
}

#[test]
fn regexp_replace_global_flag() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT regexp_replace('foo bar foo', 'foo', 'BAZ', 'g')"), "BAZ bar BAZ");
}

#[test]
fn regexp_replace_case_insensitive() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT regexp_replace('Hello WORLD', 'world', 'there', 'gi')"), "Hello there");
}

#[test]
fn regexp_replace_back_reference() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: '$2 $1' expanded to "Smith John" vs Postgres, where back references are
    // written \2 \1 and '$2 $1' is literal replacement text.
    assert_eq!(one_string(&mut c, r"SELECT regexp_replace('John Smith', '(\w+) (\w+)', '$2 $1')"), "$2 $1");
    assert_eq!(one_string(&mut c, r"SELECT regexp_replace('John Smith', '(\w+) (\w+)', '\2 \1')"), "Smith John");
}

#[test]
fn regexp_match_groups() {
    let (_db, mut c) = setup();
    assert_eq!(one_array(&mut c, r"SELECT regexp_match('foo123bar', '([a-z]+)(\d+)')"), Some(vec!["foo".to_owned(), "123".to_owned()]));
}

#[test]
fn regexp_match_no_group_returns_whole_match() {
    let (_db, mut c) = setup();
    assert_eq!(one_array(&mut c, "SELECT regexp_match('hello world', 'world')"), Some(vec!["world".to_owned()]));
}

#[test]
fn regexp_match_no_match_returns_null() {
    let (_db, mut c) = setup();
    assert_eq!(one_array(&mut c, "SELECT regexp_match('hello', 'xyz')"), None);
}

#[test]
fn regexp_split_to_array() {
    let (_db, mut c) = setup();
    assert_eq!(
        one_array(&mut c, "SELECT regexp_split_to_array('a,,b;c', '[,;]')"),
        Some(vec!["a".into(), "".into(), "b".into(), "c".into()])
    );
}

#[test]
fn regexp_replace_bad_pattern() {
    let (_db, mut c) = setup();
    // invalid_regular_expression
    expect_code(c.query_one("SELECT regexp_replace('x', '(', 'y')", &[]), "2201B");
}
