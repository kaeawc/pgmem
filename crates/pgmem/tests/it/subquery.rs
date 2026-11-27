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

fn grouped_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE t (v int, w int);
         INSERT INTO t VALUES (1, 10), (2, 20), (2, 5), (3, 30);
         CREATE TABLE u (k int, z int);
         INSERT INTO u VALUES (1, 100), (2, 200), (2, 201), (25, 7), (30, 9);",
    )
    .expect("grouped setup");
}

/// A subquery of a grouped query reads the grouped row: its references to the outer query are
/// group keys (#294), in the select list, HAVING, ORDER BY, DISTINCT and window clauses, at
/// any nesting depth and under grouping sets.
#[test]
fn grouped_query_subqueries_read_group_keys() {
    let (_db, mut c) = setup();
    grouped_setup(&mut c);
    let cases: &[(&str, &[&str])] = &[
        ("SELECT v, (SELECT x FROM generate_series(v, v) x) FROM t GROUP BY v ORDER BY v", &["1|1", "2|2", "3|3"]),
        ("SELECT v FROM t GROUP BY v HAVING EXISTS (SELECT 1 FROM generate_series(v, 2)) ORDER BY v", &["1", "2"]),
        ("SELECT v, (SELECT count(*) FROM u WHERE u.k = t.v) FROM t GROUP BY v ORDER BY v", &["1|1", "2|2", "3|0"]),
        ("SELECT v FROM t GROUP BY v ORDER BY (SELECT max(z) FROM u WHERE u.k = t.v) NULLS FIRST, v", &["3", "1", "2"]),
        ("SELECT DISTINCT (SELECT count(*) FROM u WHERE u.k = t.v) FROM t GROUP BY v ORDER BY 1", &["0", "1", "2"]),
        ("SELECT v, rank() OVER (ORDER BY (SELECT count(*) FROM u WHERE u.k = t.v)) FROM t GROUP BY v ORDER BY v", &["1|2", "2|3", "3|1"]),
        (
            "SELECT v, (SELECT (SELECT t.v + count(*) FROM u u2 WHERE u2.k = u.k) FROM u WHERE u.k = t.v LIMIT 1) FROM t GROUP BY v ORDER BY v",
            &["1|2", "2|4", "3|"],
        ),
        (
            "SELECT v, (SELECT y FROM u, LATERAL (SELECT u.z + t.v AS y) l WHERE u.k = t.v ORDER BY y LIMIT 1) FROM t GROUP BY v ORDER BY v",
            &["1|101", "2|202", "3|"],
        ),
        (
            "SELECT v, (WITH c AS (SELECT z FROM u WHERE u.k = t.v) SELECT sum(z) FROM c) FROM t GROUP BY v ORDER BY v",
            &["1|100", "2|401", "3|"],
        ),
        (
            "SELECT v, w, (SELECT t.v + t.w) FROM t GROUP BY ROLLUP (v, w) ORDER BY 1, 2",
            &["1|10|11", "1||", "2|5|7", "2|20|22", "2||", "3|30|33", "3||", "||"],
        ),
        ("SELECT v, (SELECT GROUPING(t.v)) FROM t GROUP BY ROLLUP (v) ORDER BY 1", &["1|0", "2|0", "3|0", "|1"]),
    ];
    for (sql, want) in cases {
        assert_eq!(lines(&mut c, sql), *want, "{sql}");
    }
}

/// An aggregate whose arguments reference only an enclosing query is that query's aggregate:
/// `(SELECT sum(t.w))` groups the outer query, and the subquery reads its result.
#[test]
fn outer_aggregate_inside_subquery() {
    let (_db, mut c) = setup();
    grouped_setup(&mut c);
    c.batch_execute("CREATE TABLE p (id int PRIMARY KEY, name text, x int); INSERT INTO p VALUES (1, 'a', 5), (2, 'b', 7);").expect("p");
    let cases: &[(&str, &[&str])] = &[
        ("SELECT v, sum(w), (SELECT max(z) FROM u WHERE u.k = sum(t.w)) FROM t GROUP BY v ORDER BY v", &["1|10|", "2|25|7", "3|30|9"]),
        ("SELECT v, (SELECT sum(t.w)) FROM t GROUP BY v ORDER BY 1", &["1|10", "2|25", "3|30"]),
        ("SELECT (SELECT sum(w)) FROM t", &["65"]),
        ("SELECT (SELECT sum(w + z) FROM u) FROM t ORDER BY 1", &["542", "567", "617", "667"]),
        ("SELECT (SELECT count(*)) FROM t", &["1", "1", "1", "1"]),
        ("SELECT (SELECT (SELECT sum(t.w)) FROM u LIMIT 1) FROM t", &["65"]),
        ("SELECT (SELECT x FROM (SELECT sum(t.w) x) s) FROM t", &["65"]),
        (
            "SELECT v, (SELECT count(*) + sum(t.w) FROM u GROUP BY u.k ORDER BY 1 LIMIT 1) FROM t GROUP BY v ORDER BY 1",
            &["1|11", "2|26", "3|31"],
        ),
        ("SELECT v, (SELECT string_agg(t.w::text, ',' ORDER BY t.w)) FROM t GROUP BY v ORDER BY 1", &["1|10", "2|5,20", "3|30"]),
        ("SELECT v, (SELECT count(*) FILTER (WHERE t.w > 6)) FROM t GROUP BY v ORDER BY 1", &["1|1", "2|1", "3|1"]),
        ("SELECT v, sum(w), rank() OVER (ORDER BY (SELECT sum(t.w))) FROM t GROUP BY v ORDER BY 1", &["1|10|1", "2|25|2", "3|30|3"]),
        // A column functionally dependent on the grouped primary key, next to an aggregate.
        ("SELECT sum(x), p.name, (SELECT p.name || sum(p.x)) FROM p GROUP BY p.id ORDER BY 2", &["5|a|a5", "7|b|b7"]),
    ];
    for (sql, want) in cases {
        assert_eq!(lines(&mut c, sql), *want, "{sql}");
    }
}

/// Postgres' check_ungrouped_columns for sublinks, and aggregates placed in clauses of their
/// own level that do not allow them.
#[test]
fn grouped_subquery_errors() {
    let (_db, mut c) = setup();
    grouped_setup(&mut c);
    let cases: &[(&str, &str, &str)] = &[
        ("SELECT v, (SELECT w) FROM t GROUP BY v", "42803", "subquery uses ungrouped column \"t.w\" from outer query"),
        (
            "SELECT v, (SELECT (SELECT t.w) FROM u LIMIT 1) FROM t GROUP BY v",
            "42803",
            "subquery uses ungrouped column \"t.w\" from outer query",
        ),
        ("SELECT v FROM t GROUP BY v HAVING EXISTS (SELECT t.w)", "42803", "subquery uses ungrouped column \"t.w\" from outer query"),
        ("SELECT v + 1, (SELECT t.v + 1) FROM t GROUP BY v + 1", "42803", "subquery uses ungrouped column \"t.v\" from outer query"),
        ("SELECT v, (SELECT sum(t.w + u.z) FROM u) FROM t GROUP BY v", "42803", "subquery uses ungrouped column \"t.w\" from outer query"),
        (
            "SELECT (SELECT sum(t.w)), w FROM t",
            "42803",
            "column \"t.w\" must appear in the GROUP BY clause or be used in an aggregate function",
        ),
        ("SELECT count(*), (SELECT max(z) FROM u WHERE z > count(*)) FROM t", "42803", "aggregate functions are not allowed in WHERE"),
        ("SELECT v, (SELECT max(sum(t.w)) FROM u) FROM t GROUP BY v", "42803", "aggregate function calls cannot be nested"),
        (
            "SELECT * FROM t, LATERAL (SELECT sum(t.w)) s",
            "42803",
            "aggregate functions are not allowed in FROM clause of their own query level",
        ),
        ("SELECT * FROM t WHERE (SELECT sum(t.w)) > 0", "42803", "aggregate functions are not allowed in WHERE"),
        ("SELECT v FROM t GROUP BY (SELECT sum(t.w)), v", "42803", "aggregate functions are not allowed in GROUP BY"),
    ];
    for (sql, code, msg) in cases {
        let e = expect_code(c.execute(*sql, &[]), code);
        assert_eq!(crate::support::message(&e), *msg, "{sql}");
    }
}

/// EXISTS does not evaluate its subquery's select list, ORDER BY or DISTINCT (Postgres'
/// simplify_EXISTS_query), unless the subquery aggregates.
#[test]
fn exists_skips_its_select_list() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE SEQUENCE s3").expect("sequence");
    for (sql, want) in [
        ("SELECT 1 WHERE EXISTS (SELECT nextval('s3'))", "1"),
        ("SELECT EXISTS (SELECT nextval('s3') FROM generate_series(1, 3) ORDER BY 1)", "t"),
        ("SELECT EXISTS (SELECT DISTINCT nextval('s3') FROM generate_series(1, 3))", "t"),
        ("SELECT EXISTS (SELECT nextval('s3') FROM generate_series(1, 3) LIMIT 1)", "t"),
    ] {
        assert_eq!(lines(&mut c, sql), [want], "{sql}");
    }
    expect_code(c.execute("SELECT currval('s3')", &[]), "55000");
    assert_eq!(lines(&mut c, "SELECT EXISTS (SELECT max(nextval('s3')) FROM generate_series(1, 3))"), ["t"]);
    assert_eq!(lines(&mut c, "SELECT currval('s3')"), ["3"]);
}
