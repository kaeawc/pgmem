use crate::support::setup;

fn ints(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect()
}

#[test]
fn recursive_cte_count_to_ten() {
    let (_db, mut c) = setup();
    let got = ints(&mut c, "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t ORDER BY n");
    assert_eq!(got, (1..=10).collect::<Vec<i32>>());
}

#[test]
fn recursive_cte_hierarchy_walk() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE org (id bigint PRIMARY KEY, parent_id bigint, name text NOT NULL);
         INSERT INTO org (id, parent_id, name) VALUES
             (1, NULL, 'root'), (2, 1, 'a'), (3, 1, 'b'), (4, 2, 'a1'), (5, 2, 'a2'), (6, 3, 'b1');",
    )
    .expect("setup");
    let rows = c
        .query(
            "WITH RECURSIVE descendants AS (
                 SELECT id, parent_id, name FROM org WHERE id = 1
                 UNION ALL
                 SELECT o.id, o.parent_id, o.name
                   FROM org o INNER JOIN descendants d ON o.parent_id = d.id
             )
             SELECT name FROM descendants ORDER BY id",
            &[],
        )
        .expect("query");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["root", "a", "b", "a1", "a2", "b1"]);
}

#[test]
fn recursive_cte_union_dedups() {
    let (_db, mut c) = setup();
    // UNION (without ALL) dedups, so the recursion stops once no new rows appear.
    let got = ints(&mut c, "WITH RECURSIVE t AS (SELECT 1 AS n UNION SELECT 1 FROM t) SELECT n FROM t");
    assert_eq!(got, [1]);
}

#[test]
fn recursive_cte_outer_references_are_fine() {
    let (_db, mut c) = setup();
    let got = ints(
        &mut c,
        "WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM t WHERE n < 5) SELECT n FROM t WHERE n > 2 ORDER BY n",
    );
    assert_eq!(got, [3, 4, 5]);
}
