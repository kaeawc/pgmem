//! Join, set-operation, FETCH and error-reporting details that clients observe.

use crate::support::{expect_code, message, setup, text_value, texts};

fn row(vals: &[Option<&str>]) -> Vec<Option<String>> {
    vals.iter().map(|v| v.map(str::to_owned)).collect()
}

#[test]
fn full_join_using_merges_the_key_with_coalesce() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE l (k int, v text); CREATE TABLE r (k int, w text);
         INSERT INTO l VALUES (1, 'l1'), (2, 'l2');
         INSERT INTO r VALUES (2, 'r2'), (3, 'r3')",
    )
    .expect("setup");
    let rows = texts(&mut c, "SELECT k, l.k, r.k, v, w FROM l FULL JOIN r USING (k) ORDER BY 1");
    assert_eq!(
        rows,
        vec![
            row(&[Some("1"), Some("1"), None, Some("l1"), None]),
            row(&[Some("2"), Some("2"), Some("2"), Some("l2"), Some("r2")]),
            row(&[Some("3"), None, Some("3"), None, Some("r3")]),
        ]
    );
    // The USING alias names the merged column; an aliased join hides the inner names.
    assert_eq!(text_value(&mut c, "SELECT j.k FROM l JOIN r USING (k) AS j"), Some("2".into()));
    assert_eq!(text_value(&mut c, "SELECT lr.w FROM (l JOIN r USING (k)) AS lr"), Some("r2".into()));
    let e = expect_code(c.simple_query("SELECT l.v FROM (l JOIN r USING (k)) AS lr"), "42P01");
    assert_eq!(message(&e), "invalid reference to FROM-clause entry for table \"l\"");
    let e = expect_code(c.simple_query("SELECT * FROM l JOIN r USING (nope)"), "42703");
    assert_eq!(message(&e), "column \"nope\" specified in USING clause does not exist in left table");
    let e = expect_code(c.simple_query("SELECT * FROM l JOIN r"), "42601");
    assert_eq!(message(&e), "syntax error at end of input");
}

#[test]
fn aliased_table_name_is_not_visible() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t1 (id int)").expect("setup");
    let e = expect_code(c.simple_query("SELECT t1.id FROM t1 AS x"), "42P01");
    assert_eq!(message(&e), "invalid reference to FROM-clause entry for table \"t1\"");
    let e = expect_code(c.simple_query("SELECT * FROM t1 a, (SELECT a.id) s"), "42P01");
    assert_eq!(message(&e), "invalid reference to FROM-clause entry for table \"a\"");
}

#[test]
fn set_operation_resolves_untyped_literals() {
    let (_db, mut c) = setup();
    let rows = c.query("SELECT 1 UNION SELECT '2' ORDER BY 1", &[]).expect("union");
    let got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, vec![1, 2]);
    let stmt = c.prepare("SELECT NULL UNION ALL SELECT 1").expect("null union");
    assert_eq!(stmt.columns()[0].type_(), &postgres::types::Type::INT4);
    let e = expect_code(c.simple_query("SELECT 1 AS x UNION SELECT 2 ORDER BY x + 1"), "0A000");
    assert_eq!(message(&e), "invalid UNION/INTERSECT/EXCEPT ORDER BY clause");
}

#[test]
fn fetch_first_with_ties() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE p (id int, val int); INSERT INTO p VALUES (1, 30), (2, 10), (3, 20), (4, 20), (5, 5)").expect("setup");
    let rows = texts(&mut c, "SELECT id FROM p ORDER BY val DESC FETCH FIRST 2 ROWS WITH TIES");
    assert_eq!(rows, vec![row(&[Some("1")]), row(&[Some("3")]), row(&[Some("4")])]);
    let e = expect_code(c.simple_query("SELECT id FROM p FETCH FIRST 2 ROWS WITH TIES"), "42601");
    assert_eq!(message(&e), "WITH TIES cannot be specified without ORDER BY clause");
}

#[test]
fn row_in_subquery_like_any_and_between_symmetric() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int, b text); INSERT INTO t VALUES (1, 'x'), (2, 'y')").expect("setup");
    assert_eq!(text_value(&mut c, "SELECT count(*) FROM t WHERE (a, b) IN (SELECT 2, 'y')"), Some("1".into()));
    let e = expect_code(c.simple_query("SELECT 1 FROM t WHERE a IN (SELECT 1, 2)"), "42601");
    assert_eq!(message(&e), "subquery has too many columns");
    assert_eq!(
        texts(&mut c, "SELECT 'b' LIKE ANY (ARRAY['a%', 'b%']), 'abc' ILIKE ALL (ARRAY['A%', '%C']), 'abc' NOT LIKE ALL (ARRAY['x%'])"),
        vec![row(&[Some("t"), Some("t"), Some("t")])]
    );
    assert_eq!(text_value(&mut c, "SELECT count(*) FROM t WHERE a BETWEEN SYMMETRIC 2 AND 1"), Some("2".into()));
    assert_eq!(text_value(&mut c, "TABLE t ORDER BY a LIMIT 1"), Some("1".into()));
}

#[test]
fn recursive_cte_errors() {
    let (_db, mut c) = setup();
    let e = expect_code(c.simple_query("WITH x AS (SELECT 1), x AS (SELECT 2) SELECT * FROM x"), "42712");
    assert_eq!(message(&e), "WITH query name \"x\" specified more than once");
    let e = expect_code(c.simple_query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT count(*) FROM n) SELECT * FROM n"), "42P19");
    assert_eq!(message(&e), "aggregate functions are not allowed in a recursive query's recursive term");
    let e = expect_code(c.simple_query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT 'x'::text FROM n) SELECT * FROM n"), "42804");
    assert_eq!(message(&e), "UNION types integer and text cannot be matched");
}

#[test]
fn window_frame_and_ntile_errors() {
    let (_db, mut c) = setup();
    let e = expect_code(c.simple_query("SELECT sum(1) OVER (ROWS BETWEEN UNBOUNDED FOLLOWING AND CURRENT ROW)"), "42P20");
    assert_eq!(message(&e), "frame start cannot be UNBOUNDED FOLLOWING");
    let e = expect_code(c.simple_query("SELECT ntile(0) OVER ()"), "22014");
    assert_eq!(message(&e), "argument of ntile must be greater than zero");
}

#[test]
fn scanner_errors_are_worded_like_postgres() {
    let (_db, mut c) = setup();
    let e = expect_code(c.simple_query("SELECT 'unterminated"), "42601");
    assert_eq!(message(&e), "unterminated quoted string at or near \"'unterminated\"");
    let e = expect_code(c.simple_query("SELECT 1 ORDER"), "42601");
    assert_eq!(message(&e), "syntax error at end of input");
    let e = expect_code(c.simple_query("SELECT ARRAY[1] || 'x'::text"), "42883");
    assert_eq!(message(&e), "operator does not exist: integer[] || text");
}
