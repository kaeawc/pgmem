use crate::support::{expect_code, setup};

#[test]
fn arithmetic_basic_ops() {
    let (_db, mut c) = setup();
    let cases = [
        ("SELECT 2 + 3", 5),
        ("SELECT 10 - 4", 6),
        ("SELECT 6 * 7", 42),
        ("SELECT 20 / 4", 5),
        ("SELECT 17 % 5", 2),
        ("SELECT 1 + 2 * 3", 7), // precedence
        ("SELECT (1 + 2) * 3", 9),
        ("SELECT 10 - 4 - 1", 5), // left-assoc
    ];
    for (sql, want) in cases {
        let got: i32 = c.query_one(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).get(0);
        assert_eq!(got, want, "{sql}");
    }
}

#[test]
fn arithmetic_update_increment_counter() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE counters (id int PRIMARY KEY, n int NOT NULL); INSERT INTO counters (id, n) VALUES (1, 0)")
        .expect("setup");
    for i in 0..5 {
        c.execute("UPDATE counters SET n = n + 1 WHERE id = 1", &[]).unwrap_or_else(|e| panic!("UPDATE {i}: {e}"));
    }
    let n: i32 = c.query_one("SELECT n FROM counters WHERE id = 1", &[]).expect("readback").get(0);
    assert_eq!(n, 5);
}

#[test]
fn arithmetic_division_by_zero() {
    let (_db, mut c) = setup();
    expect_code(c.execute("SELECT 10 / 0", &[]), "22012");
}

#[test]
fn arithmetic_bigint_widens() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE big (a bigint NOT NULL, b int NOT NULL); INSERT INTO big (a, b) VALUES (1000000000000, 5)")
        .expect("setup");
    let sum: i64 = c.query_one("SELECT a + b FROM big", &[]).expect("select").get(0);
    assert_eq!(sum, 1_000_000_000_005);
}
