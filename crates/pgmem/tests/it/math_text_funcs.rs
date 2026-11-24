use crate::support::{Text, setup};

fn one_text(c: &mut postgres::Client, sql: &str) -> Text {
    c.query_one(sql, &[]).expect(sql).get(0)
}

fn one_f64(c: &mut postgres::Client, sql: &str) -> f64 {
    c.query_one(sql, &[]).expect(sql).get(0)
}

fn one_string(c: &mut postgres::Client, sql: &str) -> String {
    c.query_one(sql, &[]).expect(sql).get(0)
}

// floor/ceil/round of a numeric literal return numeric (pgx scanned them into float64); read the text form.

#[test]
fn floor() {
    let (_db, mut c) = setup();
    assert_eq!(one_text(&mut c, "SELECT floor(3.7)"), "3");
}

#[test]
fn ceil() {
    let (_db, mut c) = setup();
    assert_eq!(one_text(&mut c, "SELECT ceil(3.2)"), "4");
}

#[test]
fn round_default() {
    let (_db, mut c) = setup();
    assert_eq!(one_text(&mut c, "SELECT round(3.5)"), "4");
}

#[test]
fn round_scale() {
    let (_db, mut c) = setup();
    assert_eq!(one_text(&mut c, "SELECT round(3.14159, 2)"), "3.14");
}

#[test]
fn power() {
    let (_db, mut c) = setup();
    assert_eq!(one_f64(&mut c, "SELECT power(2, 10)"), 1024.0);
}

#[test]
fn sqrt() {
    let (_db, mut c) = setup();
    assert_eq!(one_f64(&mut c, "SELECT sqrt(81)"), 9.0);
}

#[test]
fn sign() {
    let (_db, mut c) = setup();
    for (input, want) in [("5", 1.0), ("-3", -1.0), ("0", 0.0)] {
        assert_eq!(one_f64(&mut c, &format!("SELECT sign({input})")), want, "sign({input})");
    }
}

#[test]
fn random() {
    let (_db, mut c) = setup();
    let got = one_f64(&mut c, "SELECT random()");
    assert!((0.0..1.0).contains(&got), "got {got}, want in [0, 1)");
}

#[test]
fn pi() {
    let (_db, mut c) = setup();
    let got = one_f64(&mut c, "SELECT pi()");
    assert!((got - std::f64::consts::PI).abs() < 1e-9, "got {got}, want pi");
}

#[test]
fn lpad_default() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT lpad('42', 5)"), "   42");
}

#[test]
fn lpad_custom_fill() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT lpad('42', 6, 'ab')"), "abab42");
}

#[test]
fn rpad() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT rpad('42', 5, '0')"), "42000");
}

#[test]
fn lpad_truncates() {
    let (_db, mut c) = setup();
    assert_eq!(one_string(&mut c, "SELECT lpad('hello', 3)"), "hel");
}
