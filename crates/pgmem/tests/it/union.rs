use crate::support::{expect_code, message, setup, texts};

fn union_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE a (id int PRIMARY KEY, n int NOT NULL);
         CREATE TABLE b (id int PRIMARY KEY, n int NOT NULL);
         INSERT INTO a (id, n) VALUES (1, 10), (2, 20), (3, 30);
         INSERT INTO b (id, n) VALUES (4, 30), (5, 40);",
    )
    .expect("union setup");
}

/// The int4 column of every row, sorted (the queries have no ORDER BY).
fn sorted_ints(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    let mut got: Vec<i32> = c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect();
    got.sort();
    got
}

#[test]
fn union_all_keeps_duplicates() {
    let (_db, mut c) = setup();
    union_setup(&mut c);
    assert_eq!(sorted_ints(&mut c, "SELECT n FROM a UNION ALL SELECT n FROM b"), [10, 20, 30, 30, 40]);
}

#[test]
fn union_deduplicates() {
    let (_db, mut c) = setup();
    union_setup(&mut c);
    assert_eq!(sorted_ints(&mut c, "SELECT n FROM a UNION SELECT n FROM b"), [10, 20, 30, 40]);
}

#[test]
fn union_chain() {
    let (_db, mut c) = setup();
    union_setup(&mut c);
    assert_eq!(sorted_ints(&mut c, "SELECT n FROM a UNION ALL SELECT n FROM b UNION ALL SELECT 99"), [10, 20, 30, 30, 40, 99]);
}

#[test]
fn union_column_count_mismatch() {
    let (_db, mut c) = setup();
    union_setup(&mut c);
    let e = expect_code(c.query("SELECT id, n FROM a UNION SELECT n FROM b", &[]), "42601");
    assert_eq!(message(&e), "each UNION query must have the same number of columns");
}

/// Each row's columns joined by `,` (NULL as `NULL`), in output order.
fn ordered(c: &mut postgres::Client, sql: &str) -> Vec<String> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect::<Vec<_>>().join(",")).collect()
}

// Without ORDER BY the order is unspecified, but Postgres' is deterministic (sorted for a
// Sort + Unique plan, hash-table order for a hashed one) and tests snapshot it. The expected
// orders were recorded from PostgreSQL 18.

#[test]
fn union_output_order_matches_postgres() {
    let (_db, mut c) = setup();
    let cases: &[(&str, &[&str])] = &[
        // Two rows: Sort + Unique (NULLs sort last).
        ("SELECT NULL UNION SELECT 1", &["1", "NULL"]),
        ("SELECT 2 UNION SELECT 1", &["1", "2"]),
        // Three or more: HashAggregate, in hash-table order.
        ("SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 UNION SELECT 5", &["1", "3", "5", "4", "2"]),
        ("SELECT 3 UNION SELECT NULL UNION SELECT 1 UNION SELECT 3", &["NULL", "1", "3"]),
        (
            "VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10) UNION VALUES (11), (12)",
            &["12", "3", "11", "8", "10", "9", "7", "1", "5", "4", "2", "6"],
        ),
        ("SELECT 'apple' UNION SELECT 'banana' UNION SELECT 'cherry' UNION SELECT 'date'", &["cherry", "banana", "apple", "date"]),
        ("SELECT 1, 'a' UNION SELECT 2, 'b' UNION SELECT 1, 'a' UNION SELECT 3, NULL", &["3,NULL", "2,b", "1,a"]),
        ("VALUES (true), (false), (NULL) UNION VALUES (true)", &["NULL", "f", "t"]),
        ("SELECT 10000000000 UNION SELECT 1 UNION SELECT -5", &["-5", "1", "10000000000"]),
        ("SELECT 1.5 UNION SELECT 2.25 UNION SELECT 0.0001", &["1.5", "2.25", "0.0001"]),
        // A UNION ALL under a UNION is flattened into one hashed input.
        ("SELECT 1 UNION ALL SELECT 2 UNION SELECT 3", &["1", "2", "3"]),
    ];
    for (sql, want) in cases {
        assert_eq!(ordered(&mut c, sql), *want, "{sql}");
    }
}

#[test]
fn intersect_except_output_order_matches_postgres() {
    let (_db, mut c) = setup();
    let cases: &[(&str, &[&str])] = &[
        ("VALUES (1), (2), (3), (4), (5), (6) INTERSECT VALUES (6), (5), (4), (3)", &["6", "3", "5", "4"]),
        ("VALUES (1), (2), (3), (4), (5), (6) EXCEPT VALUES (2)", &["6", "1", "3", "5", "4"]),
        ("VALUES (1), (1), (2), (2), (2), (3) EXCEPT ALL VALUES (2)", &["1", "1", "3", "2", "2"]),
        ("VALUES (1), (1), (2), (2), (3), (4) INTERSECT ALL VALUES (1), (2), (1), (2), (2), (4), (4)", &["1", "1", "4", "2", "2"]),
        // INTERSECT hashes the input with fewer groups (here the constant one).
        ("SELECT 3 INTERSECT VALUES (1), (2), (3)", &["3"]),
        // Both inputs come sorted (Sort + Unique), so the SetOp merges them: sorted output.
        ("(SELECT 3 UNION SELECT 1 UNION SELECT 2) EXCEPT ALL (SELECT 1 UNION SELECT 4)", &["2", "3"]),
    ];
    for (sql, want) in cases {
        assert_eq!(ordered(&mut c, sql), *want, "{sql}");
    }
}

#[test]
fn select_distinct_output_order_matches_postgres() {
    let (_db, mut c) = setup();
    let cases: &[(&str, &[&str])] = &[
        ("SELECT DISTINCT x FROM (VALUES (1), (2), (3), (4), (5), (1), (2)) v(x)", &["1", "3", "5", "4", "2"]),
        ("SELECT DISTINCT x FROM (VALUES ('b'), ('a')) v(x)", &["a", "b"]),
        ("SELECT DISTINCT * FROM (VALUES (1, 'x'), (2, 'y'), (1, 'x'), (3, 'z')) v", &["3,z", "2,y", "1,x"]),
    ];
    for (sql, want) in cases {
        assert_eq!(ordered(&mut c, sql), *want, "{sql}");
    }
}
