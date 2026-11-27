use crate::support::{expect_code, setup, texts};

fn subquery_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE customers (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, customer_id int NOT NULL);
         INSERT INTO customers (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'carol');
         INSERT INTO orders (id, customer_id) VALUES (10, 1), (11, 2);",
    )
    .expect("subquery setup");
}

#[test]
fn scalar_subquery_over_wire() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let n: i32 = c.query_one("SELECT (SELECT id FROM customers WHERE name = 'alice')", &[]).expect("scalar").get(0);
    assert_eq!(n, 1);
}

#[test]
fn scalar_subquery_empty_returns_null() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let n: Option<i32> = c.query_one("SELECT (SELECT id FROM customers WHERE name = 'no-one')", &[]).expect("scalar").get(0);
    assert_eq!(n, None);
}

#[test]
fn scalar_subquery_multiple_rows_errors() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    expect_code(c.execute("SELECT (SELECT id FROM customers)", &[]), "21000");
}

#[test]
fn in_list_over_wire() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let rows = c.query("SELECT id FROM customers WHERE id IN (1, 3)", &[]).expect("select");
    let mut got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    got.sort();
    assert_eq!(got, [1, 3]);
}

#[test]
fn in_subquery_over_wire() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let rows = c.query("SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders) ORDER BY name", &[]).expect("select");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["alice", "bob"]);
}

#[test]
fn not_in_subquery_over_wire() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let rows = c.query("SELECT name FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)", &[]).expect("select");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["carol"]);
}

#[test]
fn in_subquery_with_parameter() {
    let (_db, mut c) = setup();
    subquery_setup(&mut c);
    let rows = c.query("SELECT id FROM customers WHERE name IN ($1, $2) ORDER BY id", &[&"alice", &"carol"]).expect("select");
    let got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, [1, 3]);
}

/// Rows of a simple query as `a|b` lines (NULL as empty).
fn lines(c: &mut postgres::Client, sql: &str) -> Vec<String> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(Option::unwrap_or_default).collect::<Vec<_>>().join("|")).collect()
}

fn correlation_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE t (v int);
         INSERT INTO t VALUES (1), (2), (3);
         CREATE TABLE u (w int);
         INSERT INTO u VALUES (1), (2), (3);",
    )
    .expect("correlation setup");
}

/// A subquery whose only outer reference is inside its FROM clause (a function's arguments,
/// a derived table, VALUES, a JOIN condition) or in LIMIT / OFFSET, ORDER BY, GROUP BY,
/// HAVING, a window, a CTE or a nested subquery is correlated: it runs once per outer row.
#[test]
fn correlation_in_every_clause_reruns_the_subquery() {
    let (_db, mut c) = setup();
    correlation_setup(&mut c);
    let cases: &[(&str, &[&str])] = &[
        ("SELECT v, (SELECT x FROM generate_series(v, v) x) FROM (VALUES (1), (2)) s(v)", &["1|1", "2|2"]),
        ("SELECT v, (SELECT x FROM generate_series(v, v) x) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT sum(x) FROM unnest(ARRAY[v, v * 2]) x) FROM t ORDER BY v", &["1|3", "2|6", "3|9"]),
        ("SELECT v, (SELECT x FROM abs(-v) x) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT y FROM u, LATERAL (SELECT w + v AS y) l WHERE w = 1) FROM t ORDER BY v", &["1|2", "2|3", "3|4"]),
        ("SELECT v, (SELECT sum(x) FROM u, LATERAL generate_series(w, v) x) FROM t ORDER BY v", &["1|1", "2|5", "3|14"]),
        (
            "SELECT v, (SELECT z FROM (SELECT 1) a(q), LATERAL (SELECT x AS z FROM generate_series(v, v) x) l) FROM t ORDER BY v",
            &["1|1", "2|2", "3|3"],
        ),
        ("SELECT v, (SELECT y FROM (SELECT v * 10 AS y) d) FROM t ORDER BY v", &["1|10", "2|20", "3|30"]),
        ("SELECT v, (SELECT y FROM (VALUES (v)) vv(y)) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT count(*) FROM u a JOIN u b ON a.w = b.w AND a.w <= v) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT count(*) FROM u JOIN generate_series(1, v) x ON x = w) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT sum(w) FROM (SELECT w FROM u ORDER BY w LIMIT v) s) FROM t ORDER BY v", &["1|1", "2|3", "3|6"]),
        ("SELECT v, (SELECT w FROM u ORDER BY w LIMIT 1 OFFSET v - 1) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT w FROM u ORDER BY abs(w - v) LIMIT 1) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT count(*) FROM (SELECT 1 FROM u GROUP BY w < v) g) FROM t ORDER BY v", &["1|1", "2|2", "3|2"]),
        ("SELECT v, (SELECT count(*) FROM u HAVING count(*) > v) FROM t ORDER BY v", &["1|3", "2|3", "3|"]),
        ("SELECT v, (SELECT max(r) FROM (SELECT sum(w) OVER (PARTITION BY w < v) r FROM u) s) FROM t ORDER BY v", &["1|6", "2|5", "3|3"]),
        (
            "SELECT v, (SELECT max(r) FROM (SELECT sum(w) OVER (ORDER BY w ROWS BETWEEN v PRECEDING AND CURRENT ROW) r FROM u) s) \
             FROM t ORDER BY v",
            &["1|5", "2|6", "3|6"],
        ),
        ("SELECT v, (WITH c AS (SELECT v AS y) SELECT y FROM c) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (WITH c AS (SELECT x FROM generate_series(v, v) x) SELECT x FROM c) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT x FROM generate_series(v, v) x UNION ALL SELECT 0 LIMIT 1) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT (SELECT x FROM generate_series(v, v) x)) AS s FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT (SELECT x FROM generate_series(v, v) x) FROM u WHERE w = 1) AS s FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT y FROM (SELECT x AS y FROM generate_series(v, v) x) d) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, (SELECT count(*) FROM u WHERE w IN (SELECT x FROM generate_series(1, v) x)) FROM t ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v, l.y FROM t, LATERAL (SELECT (SELECT x FROM generate_series(v, v) x) AS y) l ORDER BY v", &["1|1", "2|2", "3|3"]),
    ];
    for (sql, want) in cases {
        assert_eq!(lines(&mut c, sql), *want, "{sql}");
    }
}

/// EXISTS, IN, ANY, ALL, ARRAY() and row subqueries correlated only through FROM, in WHERE,
/// the select list and JOIN conditions.
#[test]
fn from_clause_correlation_in_every_subquery_kind() {
    let (_db, mut c) = setup();
    correlation_setup(&mut c);
    let cases: &[(&str, &[&str])] = &[
        ("SELECT v FROM t WHERE EXISTS (SELECT 1 FROM generate_series(v, 2) x) ORDER BY v", &["1", "2"]),
        ("SELECT v FROM t WHERE NOT EXISTS (SELECT 1 FROM generate_series(v, 2) x) ORDER BY v", &["3"]),
        ("SELECT v FROM t WHERE 2 IN (SELECT x FROM generate_series(v, 2) x) ORDER BY v", &["1", "2"]),
        ("SELECT v FROM t WHERE 2 = ANY (SELECT x FROM generate_series(v, v) x) ORDER BY v", &["2"]),
        ("SELECT v FROM t WHERE 2 <= ALL (SELECT x FROM generate_series(v, 3) x) ORDER BY v", &["2", "3"]),
        ("SELECT v FROM t WHERE (v, v) IN (SELECT x, x FROM generate_series(v, v) x) ORDER BY v", &["1", "2", "3"]),
        ("SELECT v FROM t WHERE v = (SELECT x FROM generate_series(v, v) x) ORDER BY v", &["1", "2", "3"]),
        ("SELECT v, ARRAY(SELECT x FROM generate_series(1, v) x) FROM t ORDER BY v", &["1|{1}", "2|{1,2}", "3|{1,2,3}"]),
        ("SELECT v, EXISTS (SELECT 1 FROM generate_series(v, 2) x) FROM t ORDER BY v", &["1|t", "2|t", "3|f"]),
        ("SELECT v, w FROM t JOIN u ON w = (SELECT x FROM generate_series(v, v) x) ORDER BY v", &["1|1", "2|2", "3|3"]),
    ];
    for (sql, want) in cases {
        assert_eq!(lines(&mut c, sql), *want, "{sql}");
    }
}

#[test]
fn from_clause_correlation_in_update_and_delete() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE m (k int, a int, b int, d int[]);
         INSERT INTO m VALUES (1, 0, 0, NULL), (2, 0, 0, NULL), (3, 0, 0, NULL);
         UPDATE m SET a = (SELECT x FROM generate_series(k, k) x);
         UPDATE m SET (b) = (SELECT x * 10 FROM generate_series(k, k) x);
         UPDATE m SET d = ARRAY(SELECT x FROM generate_series(1, k) x);",
    )
    .expect("update");
    assert_eq!(lines(&mut c, "SELECT k, a, b, d FROM m ORDER BY k"), ["1|1|10|{1}", "2|2|20|{1,2}", "3|3|30|{1,2,3}"]);
    c.batch_execute("DELETE FROM m WHERE EXISTS (SELECT 1 FROM generate_series(k, k) x WHERE x = 2)").expect("delete");
    assert_eq!(lines(&mut c, "SELECT k FROM m ORDER BY k"), ["1", "3"]);
    c.batch_execute("DELETE FROM m WHERE k IN (SELECT y FROM (VALUES (k)) v(y) WHERE y > 2)").expect("delete");
    assert_eq!(lines(&mut c, "SELECT k FROM m ORDER BY k"), ["1"]);
}

/// An uncorrelated subquery runs once per statement, volatile functions included; one
/// correlated through its FROM clause runs per row.
#[test]
fn uncorrelated_subquery_runs_once() {
    let (_db, mut c) = setup();
    correlation_setup(&mut c);
    c.batch_execute("CREATE SEQUENCE sq").expect("sequence");
    let cases: &[(&str, &[&str])] = &[
        ("SELECT count(DISTINCT r) FROM (SELECT (SELECT random()) r FROM t) s", &["1"]),
        ("SELECT count(DISTINCT r) FROM (SELECT (SELECT random() FROM u LIMIT 1) r FROM t) s", &["1"]),
        ("SELECT count(DISTINCT r) FROM (SELECT (SELECT r FROM (SELECT random() AS r) q) r FROM t) s", &["1"]),
        ("SELECT count(DISTINCT r) FROM (SELECT (SELECT random() FROM generate_series(v, v)) r FROM t) s", &["3"]),
        ("SELECT v, (SELECT nextval('sq')) FROM t ORDER BY v", &["1|1", "2|1", "3|1"]),
        ("SELECT v, ARRAY(SELECT nextval('sq')) FROM t ORDER BY v", &["1|{2}", "2|{2}", "3|{2}"]),
        ("SELECT v, (SELECT nextval('sq') FROM generate_series(1, 1)) FROM t ORDER BY v", &["1|3", "2|3", "3|3"]),
        ("SELECT v, (SELECT nextval('sq') FROM generate_series(v, v)) FROM t ORDER BY v", &["1|4", "2|5", "3|6"]),
        ("SELECT v FROM t WHERE v IN (SELECT nextval('sq') - 5) ORDER BY v", &["2"]),
    ];
    for (sql, want) in cases {
        assert_eq!(lines(&mut c, sql), *want, "{sql}");
    }
}
