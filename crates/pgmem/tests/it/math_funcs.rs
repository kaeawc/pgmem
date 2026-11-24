use crate::support::{expect_code, setup};

#[test]
fn abs() {
    let (_db, mut c) = setup();
    for (input, want) in [(-7i32, 7i32), (0, 0), (42, 42)] {
        let got: i32 = c.query_one("SELECT abs($1::int)", &[&input]).expect("abs").get(0);
        assert_eq!(got, want, "abs({input})");
    }
}

#[test]
fn mod_pg() {
    let (_db, mut c) = setup();
    let got: i32 = c.query_one("SELECT mod(10, 3)::int", &[]).expect("mod(10,3)").get(0);
    assert_eq!(got, 1);
    // Sign follows the dividend.
    let got: i32 = c.query_one("SELECT mod(-7, 3)::int", &[]).expect("mod(-7,3)").get(0);
    assert_eq!(got, -1);
}

#[test]
fn mod_div_by_zero() {
    let (_db, mut c) = setup();
    expect_code(c.query_one("SELECT mod(10, 0)", &[]), "22012");
}

#[test]
fn greatest_least() {
    let (_db, mut c) = setup();
    let row = c.query_one("SELECT greatest(3, 7, 1, 5)::int, least(3, 7, 1, 5)::int", &[]).expect("greatest/least");
    let (hi, lo): (i32, i32) = (row.get(0), row.get(1));
    assert_eq!((hi, lo), (7, 1));
}

#[test]
fn greatest_nulls_skipped() {
    let (_db, mut c) = setup();
    let got: i32 = c.query_one("SELECT greatest(NULL, 3, NULL, 9)::int", &[]).expect("greatest").get(0);
    assert_eq!(got, 9);
}

#[test]
fn greatest_all_nulls_is_null() {
    let (_db, mut c) = setup();
    let got: Option<i32> = c.query_one("SELECT greatest(NULL::int, NULL::int)", &[]).expect("greatest").get(0);
    assert_eq!(got, None);
}
