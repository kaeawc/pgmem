use crate::support::setup;

fn one(c: &mut postgres::Client, sql: &str) -> Option<i32> {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn position_found() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT position('lo' IN 'hello world')"), Some(4));
}

#[test]
fn position_not_found() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT position('xyz' IN 'hello world')"), Some(0));
}

#[test]
fn position_null_propagates() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT position(NULL::text IN 'hello')"), None);
}

#[test]
fn position_unicode() {
    let (_db, mut c) = setup();
    // "é" precedes "llo"; 1-indexed character position.
    assert_eq!(one(&mut c, "SELECT position('llo' IN 'héllo')"), Some(3));
}
