use crate::support::{expect_code, message, setup};

#[test]
fn unary_minus_literal() {
    let (_db, mut c) = setup();
    let cases = [
        ("SELECT -1", -1),
        ("SELECT -42", -42),
        ("SELECT -(1 + 2)", -3),
        ("SELECT - -5", 5),   // double-negate
        ("SELECT -1 + 5", 4), // unary tighter than binary
        ("SELECT 5 - -3", 8), // binary minus then unary minus
    ];
    for (sql, want) in cases {
        let got: i32 = c.query_one(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).get(0);
        assert_eq!(got, want, "{sql}");
    }
}

#[test]
fn unary_minus_on_column() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE nums (n int NOT NULL); INSERT INTO nums (n) VALUES (10)").expect("setup");
    let got: i32 = c.query_one("SELECT -n FROM nums", &[]).expect("select").get(0);
    assert_eq!(got, -10);
}

#[test]
fn substring_negative_length_errors_literal() {
    let (_db, mut c) = setup();
    let e = expect_code(c.execute("SELECT substring('abc', 1, -1)", &[]), "22011");
    assert_eq!(message(&e), "negative substring length not allowed");
}
