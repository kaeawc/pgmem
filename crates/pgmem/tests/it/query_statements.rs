//! MERGE, data-modifying WITH queries, RETURNING old/new, GROUPING SETS, window RANGE offsets
//! and EXCLUDE, SEARCH / CYCLE.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

#[test]
fn merge_runs_the_first_matching_clause() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE stock (sku text PRIMARY KEY, qty int NOT NULL); INSERT INTO stock VALUES ('a', 10), ('b', 5), ('c', 1)")
        .expect("setup");
    let merged = rows(
        &mut c,
        "MERGE INTO stock s USING (VALUES ('a', 1), ('c', -1), ('d', 4), ('e', 0)) AS v(sku, qty) ON s.sku = v.sku \
         WHEN MATCHED AND s.qty + v.qty <= 0 THEN DELETE \
         WHEN MATCHED THEN UPDATE SET qty = s.qty + v.qty \
         WHEN NOT MATCHED AND v.qty = 0 THEN DO NOTHING \
         WHEN NOT MATCHED THEN INSERT VALUES (v.sku, v.qty) \
         WHEN NOT MATCHED BY SOURCE THEN UPDATE SET qty = 0 \
         RETURNING merge_action(), s.sku, old.qty, new.qty",
    );
    let mut merged = merged;
    merged.sort();
    let row = |a: &str, s: &str, o: &str, n: &str| vec![a.to_owned(), s.to_owned(), o.to_owned(), n.to_owned()];
    assert_eq!(
        merged,
        [row("DELETE", "c", "1", "NULL"), row("INSERT", "d", "NULL", "4"), row("UPDATE", "a", "10", "11"), row("UPDATE", "b", "5", "0")]
    );
    assert_eq!(rows(&mut c, "SELECT sku, qty FROM stock ORDER BY sku"), [["a", "11"], ["b", "0"], ["d", "4"]]);
    let e = expect_code(
        c.batch_execute("MERGE INTO stock s USING (VALUES ('a'), ('a')) v(sku) ON s.sku = v.sku WHEN MATCHED THEN DELETE"),
        "21000",
    );
    assert_eq!(message(&e), "MERGE command cannot affect row a second time");
    expect_code(
        c.batch_execute(
            "MERGE INTO stock s USING (VALUES ('z')) v(sku) ON s.sku = v.sku WHEN NOT MATCHED THEN INSERT (sku) VALUES (v.sku)",
        ),
        "23502",
    );
    expect_code(c.batch_execute("MERGE INTO stock s USING stock t ON s.sku = t.sku WHEN NOT MATCHED THEN UPDATE SET qty = 1"), "42601");
}

#[test]
fn data_modifying_with_queries_share_one_snapshot() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE orders (id int PRIMARY KEY, status text); CREATE TABLE archive (id int, status text); INSERT INTO orders VALUES (1, 'done'), (2, 'open'), (3, 'done')")
        .expect("setup");
    let moved = rows(
        &mut c,
        "WITH moved AS (DELETE FROM orders WHERE status = 'done' RETURNING *) INSERT INTO archive SELECT * FROM moved RETURNING id",
    );
    assert_eq!(moved, [["1"], ["3"]]);
    // The main query reads the snapshot from before the WITH queries ran.
    assert_eq!(
        rows(
            &mut c,
            "WITH ins AS (INSERT INTO orders VALUES (4, 'new') RETURNING id) SELECT (SELECT count(*) FROM orders), (SELECT id FROM ins)"
        ),
        [["1", "4"]]
    );
    assert_eq!(rows(&mut c, "SELECT count(*) FROM orders"), [["2"]]);
    let e = expect_code(c.batch_execute("WITH d AS (DELETE FROM orders) SELECT * FROM d"), "0A000");
    assert_eq!(message(&e), "WITH query \"d\" does not have a RETURNING clause");
    let e = expect_code(c.batch_execute("SELECT * FROM (WITH d AS (DELETE FROM orders RETURNING id) SELECT id FROM d) s"), "0A000");
    assert_eq!(message(&e), "WITH clause containing a data-modifying statement must be at the top level");
    // Two inserts of the same key in one statement still collide.
    expect_code(c.batch_execute("WITH a AS (INSERT INTO orders VALUES (9, 'x') RETURNING id) INSERT INTO orders VALUES (9, 'y')"), "23505");
    // WITH on UPDATE and DELETE.
    assert_eq!(
        rows(&mut c, "WITH k AS (SELECT 2 AS id) UPDATE orders SET status = 'big' FROM k WHERE orders.id = k.id RETURNING orders.status"),
        [["big"]]
    );
    assert_eq!(rows(&mut c, "WITH k AS (SELECT 4 AS id) DELETE FROM orders USING k WHERE orders.id = k.id RETURNING orders.id"), [["4"]]);
}

#[test]
fn returning_old_and_new() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE e (id int PRIMARY KEY, s int)").expect("CREATE");
    assert_eq!(rows(&mut c, "INSERT INTO e VALUES (1, 10) RETURNING old.s, new.s"), [["NULL", "10"]]);
    assert_eq!(
        rows(&mut c, "INSERT INTO e VALUES (1, 20) ON CONFLICT (id) DO UPDATE SET s = 30 RETURNING old.s, new.s, s"),
        [["10", "30", "30"]]
    );
    assert_eq!(rows(&mut c, "UPDATE e SET s = 40 RETURNING WITH (OLD AS o, NEW AS n) o.s, n.s"), [["30", "40"]]);
    assert_eq!(rows(&mut c, "DELETE FROM e RETURNING old.s, new.s, s"), [["40", "NULL", "40"]]);
}

#[test]
fn update_set_columns_from_subquery_and_short_insert() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE emp (id int, salary int, bonus int DEFAULT 7); CREATE TABLE dept (budget int); INSERT INTO dept VALUES (1000)",
    )
    .expect("setup");
    // Without a column list, the values fill the leading columns.
    c.batch_execute("INSERT INTO emp VALUES (1)").expect("short INSERT");
    assert_eq!(
        rows(&mut c, "UPDATE emp SET (salary, bonus) = (SELECT budget / 10, budget / 100 FROM dept) RETURNING salary, bonus"),
        [["100", "10"]]
    );
    c.batch_execute("INSERT INTO dept VALUES (2000)").expect("second dept");
    expect_code(c.batch_execute("UPDATE emp SET (salary, bonus) = (SELECT budget, budget FROM dept)"), "21000");
    let e = expect_code(c.batch_execute("UPDATE emp e SET e.salary = 1"), "42703");
    assert_eq!(message(&e), "column \"e\" of relation \"emp\" does not exist");
    expect_code(c.batch_execute("UPDATE emp SET salary = 1 LIMIT 1"), "42601");
}

#[test]
fn grouping_sets_rollup_cube() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE sales (region text, product text, qty int); INSERT INTO sales VALUES ('n', 'x', 1), ('n', 'y', 2), ('s', 'x', 4)",
    )
    .expect("setup");
    let rollup = rows(
        &mut c,
        "SELECT region, product, sum(qty), grouping(region, product) FROM sales GROUP BY ROLLUP (region, product) ORDER BY 4, 1, 2",
    );
    assert_eq!(
        rollup,
        [
            ["n", "x", "1", "0"],
            ["n", "y", "2", "0"],
            ["s", "x", "4", "0"],
            ["n", "NULL", "3", "1"],
            ["s", "NULL", "4", "1"],
            ["NULL", "NULL", "7", "3"]
        ]
    );
    assert_eq!(rows(&mut c, "SELECT count(*) FROM sales GROUP BY CUBE (region, product)").len(), 8);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM sales GROUP BY GROUPING SETS ((), ())"), [["3"], ["3"]]);
    assert_eq!(
        rows(&mut c, "SELECT region, product, sum(qty) FROM sales GROUP BY region, ROLLUP (product) ORDER BY 1, 2 NULLS LAST").len(),
        5
    );
    expect_code(c.batch_execute("SELECT region, grouping(product) FROM sales GROUP BY region"), "42803");
}

#[test]
fn window_range_offsets_and_exclude() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE p (id int, pts int); INSERT INTO p VALUES (1, 10), (2, 20), (3, 20), (4, 5)").expect("setup");
    assert_eq!(
        rows(&mut c, "SELECT id, sum(pts) OVER (ORDER BY pts RANGE BETWEEN 5 PRECEDING AND 5 FOLLOWING) FROM p ORDER BY id"),
        [["1", "15"], ["2", "40"], ["3", "40"], ["4", "15"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT id, sum(pts) OVER (ORDER BY pts DESC RANGE BETWEEN 10 PRECEDING AND CURRENT ROW) FROM p ORDER BY id"),
        [["1", "50"], ["2", "40"], ["3", "40"], ["4", "15"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT id, sum(pts) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE CURRENT ROW) FROM p ORDER BY id"
        ),
        [["1", "20"], ["2", "30"], ["3", "25"], ["4", "20"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT id, sum(pts) OVER (ORDER BY pts ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING EXCLUDE TIES) FROM p ORDER BY id"
        ),
        [["1", "55"], ["2", "35"], ["3", "35"], ["4", "55"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT d, count(*) OVER (ORDER BY d RANGE BETWEEN INTERVAL '1 day' PRECEDING AND CURRENT ROW) FROM (VALUES (DATE '2024-01-01'), (DATE '2024-01-02'), (DATE '2024-01-04')) v(d) ORDER BY d"
        ),
        [["2024-01-01", "1"], ["2024-01-02", "2"], ["2024-01-04", "1"]]
    );
    let e =
        expect_code(c.batch_execute("SELECT sum(pts) OVER (ORDER BY id::text RANGE BETWEEN 1 PRECEDING AND CURRENT ROW) FROM p"), "0A000");
    assert_eq!(message(&e), "RANGE with offset PRECEDING/FOLLOWING is not supported for column type text");
    // Aggregates of the query inside window calls.
    assert_eq!(rows(&mut c, "SELECT pts, sum(count(*)) OVER () FROM p GROUP BY pts ORDER BY pts"), [["5", "4"], ["10", "4"], ["20", "4"]]);
    let e = expect_code(c.batch_execute("SELECT sum(row_number() OVER ()) FROM p"), "42803");
    assert_eq!(message(&e), "aggregate function calls cannot contain window function calls");
}

#[test]
fn recursive_search_and_cycle() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE edges (src int, dst int); INSERT INTO edges VALUES (1, 2), (2, 3), (3, 1), (1, 4)").expect("setup");
    let depth = rows(
        &mut c,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT e.dst FROM edges e JOIN t ON e.src = t.n WHERE e.dst <> 1) \
         SEARCH DEPTH FIRST BY n SET ord SELECT n FROM t ORDER BY ord",
    );
    assert_eq!(depth, [["1"], ["2"], ["3"], ["4"]]);
    let breadth = rows(
        &mut c,
        "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT e.dst FROM edges e JOIN t ON e.src = t.n WHERE e.dst <> 1) \
         SEARCH BREADTH FIRST BY n SET ord SELECT n FROM t ORDER BY ord",
    );
    assert_eq!(breadth, [["1"], ["2"], ["4"], ["3"]]);
    let cycle = rows(
        &mut c,
        "WITH RECURSIVE g(src, dst) AS (SELECT src, dst FROM edges WHERE src = 1 UNION ALL SELECT e.src, e.dst FROM edges e JOIN g ON e.src = g.dst) \
         CYCLE src SET looped TO 'Y' DEFAULT 'N' USING p SELECT src, dst, looped, p::text FROM g ORDER BY p::text, dst",
    );
    assert_eq!(
        cycle,
        [
            ["1", "2", "Y", "{(1),(2),(3),(1)}"],
            ["1", "4", "Y", "{(1),(2),(3),(1)}"],
            ["3", "1", "N", "{(1),(2),(3)}"],
            ["2", "3", "N", "{(1),(2)}"],
            ["1", "2", "N", "{(1)}"],
            ["1", "4", "N", "{(1)}"]
        ]
    );
}
