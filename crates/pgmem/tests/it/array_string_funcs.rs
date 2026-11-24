use crate::support::setup;

fn one_string(c: &mut postgres::Client, sql: &str) -> String {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn array_length_non_empty() {
    let (_db, mut c) = setup();
    let n: Option<i32> = c.query_one("SELECT array_length($1::int[], 1)", &[&vec![1i32, 2, 3]]).expect("array_length").get(0);
    assert_eq!(n, Some(3));
}

#[test]
fn array_length_empty() {
    let (_db, mut c) = setup();
    let n: Option<i32> = c.query_one("SELECT array_length($1::int[], 1)", &[&Vec::<i32>::new()]).expect("array_length").get(0);
    assert_eq!(n, None, "empty array_length should be NULL");
}

#[test]
fn cardinality() {
    let (_db, mut c) = setup();
    let n: i32 = c.query_one("SELECT cardinality($1::text[])", &[&vec!["a", "b", "c"]]).expect("cardinality").get(0);
    assert_eq!(n, 3);
    // Cardinality of an empty array is 0 (unlike array_length).
    let n: i32 = c.query_one("SELECT cardinality($1::text[])", &[&Vec::<String>::new()]).expect("empty cardinality").get(0);
    assert_eq!(n, 0);
}

#[test]
fn array_to_string_plain_sep() {
    let (_db, mut c) = setup();
    let s: String = c.query_one("SELECT array_to_string($1::text[], ',')", &[&vec!["a", "b", "c"]]).expect("array_to_string").get(0);
    assert_eq!(s, "a,b,c");
}

#[test]
fn array_to_string_int_array() {
    let (_db, mut c) = setup();
    let s: String = c.query_one("SELECT array_to_string($1::int[], '-')", &[&vec![1i32, 2, 3]]).expect("array_to_string").get(0);
    assert_eq!(s, "1-2-3");
}

#[test]
fn string_to_array_basic() {
    let (_db, mut c) = setup();
    let a: Vec<String> = c.query_one("SELECT string_to_array('a,b,c', ',')", &[]).expect("string_to_array").get(0);
    assert_eq!(a, ["a", "b", "c"]);
}

#[test]
fn split_part_positive() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT split_part('a,b,c', ',', 2)"), "b");
}

#[test]
fn split_part_negative() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT split_part('a,b,c', ',', -1)"), "c");
}

#[test]
fn split_part_out_of_range() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT split_part('a,b', ',', 5)"), "");
}

#[test]
fn repeat() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT repeat('ab', 3)"), "ababab");
    assert_eq!(one_string(&mut c, "SELECT repeat('ab', 0)"), "");
}

#[test]
fn reverse() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT reverse('hello')"), "olleh");
}
