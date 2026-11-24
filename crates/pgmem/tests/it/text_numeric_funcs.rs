use crate::support::{expect_code, setup};

fn one_string(c: &mut postgres::Client, sql: &str) -> String {
    c.query_one(sql, &[]).expect(sql).get(0)
}

fn one_i32(c: &mut postgres::Client, sql: &str) -> i32 {
    c.query_one(sql, &[]).expect(sql).get(0)
}

fn one_bytes(c: &mut postgres::Client, sql: &str) -> Vec<u8> {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn format_plain_string() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT format('hello %s', 'world')"), "hello world");
}

#[test]
fn format_percent_literal() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT format('100%%')"), "100%");
}

#[test]
fn format_identifier_and_literal() {
    let (_db, mut c) = setup();
    assert_eq!(
        one_string(&mut c, "SELECT format('SELECT * FROM %I WHERE x = %L', 'my table', 'a''b')"),
        r#"SELECT * FROM "my table" WHERE x = 'a''b'"#
    );
}

#[test]
fn format_null_sas_empty() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT format('[%s]', NULL)"), "[]");
}

#[test]
fn format_null_las_null() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT format('value: %L', NULL)"), "value: NULL");
}

#[test]
fn chr() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT chr(65)"), "A");
}

#[test]
fn chr_rejects_zero() {
    let (_db, mut c) = setup();
    // program_limit_exceeded: "null character not permitted"
    expect_code(c.query_one("SELECT chr(0)", &[]), "54000");
}

#[test]
fn ascii() {
    let (_db, mut c) = setup();
    assert_eq!(one_i32(&mut c, "SELECT ascii('Apple')"), 65);
}

#[test]
fn ascii_empty() {
    let (_db, mut c) = setup();
    assert_eq!(one_i32(&mut c, "SELECT ascii('')"), 0);
}

#[test]
fn to_hex() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT to_hex(255)"), "ff");
}

#[test]
fn md5() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT md5('hello')"), "5d41402abc4b2a76b9719d911017c592");
}

#[test]
fn sha256_bytea() {
    let (_db, mut c) = setup();
    assert_eq!(one_bytes(&mut c, "SELECT sha256('hello'::bytea)").len(), 32);
}

#[test]
fn sha512_bytea() {
    let (_db, mut c) = setup();
    assert_eq!(one_bytes(&mut c, "SELECT sha512('hello'::bytea)").len(), 64);
}
