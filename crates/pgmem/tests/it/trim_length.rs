use crate::support::setup;

fn one_string(c: &mut postgres::Client, sql: &str) -> String {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn ltrim_default() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT ltrim('   hello')"), "hello");
}

#[test]
fn rtrim_default() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT rtrim('hello   ')"), "hello");
}

#[test]
fn btrim_cutset() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT btrim('xxhelloxxx', 'x')"), "hello");
}

#[test]
fn ltrim_cutset() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT ltrim('//path/x', '/')"), "path/x");
}

#[test]
fn char_length() {
    let (_db, mut c) = setup();
    let got: i32 = c.query_one("SELECT char_length('héllo')", &[]).expect("char_length").get(0);
    assert_eq!(got, 5);
}

#[test]
fn octet_length_text() {
    let (_db, mut c) = setup();
    let got: i32 = c.query_one("SELECT octet_length('héllo')", &[]).expect("octet_length").get(0);
    assert_eq!(got, 6); // h(1) + é(2) + l(1) + l(1) + o(1)
}

#[test]
fn octet_length_bytea() {
    let (_db, mut c) = setup();
    let got: i32 = c.query_one("SELECT octet_length($1::bytea)", &[&vec![0u8, 1, 2, 3]]).expect("octet_length").get(0);
    assert_eq!(got, 4);
}

#[test]
fn trim_null_propagates() {
    let (_db, mut c) = setup();
    let got: Option<String> = c.query_one("SELECT ltrim(NULL::text)", &[]).expect("ltrim").get(0);
    assert_eq!(got, None);
}
