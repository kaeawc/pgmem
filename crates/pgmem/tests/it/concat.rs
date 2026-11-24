use crate::support::setup;

fn one(c: &mut postgres::Client, sql: &str) -> Option<String> {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn concat_basic() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT concat('a', 'b', 'c')").as_deref(), Some("abc"));
}

/// concat treats NULL as empty (unlike `||` which propagates NULL).
#[test]
fn concat_nulls_skipped() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT concat('a', NULL, 'c')").as_deref(), Some("ac"));
}

#[test]
fn concat_mixed_types() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT concat('count: ', 42)").as_deref(), Some("count: 42"));
}

#[test]
fn concat_ws_basic() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT concat_ws(', ', 'a', 'b', 'c')").as_deref(), Some("a, b, c"));
}

#[test]
fn concat_ws_nulls_skipped_no_leading_sep() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT concat_ws('-', NULL, 'a', NULL, 'b')").as_deref(), Some("a-b"));
}

#[test]
fn concat_ws_null_sep_is_null() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT concat_ws(NULL::text, 'a', 'b')"), None);
}
