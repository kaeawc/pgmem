use crate::support::setup;

fn cte_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE orders (id int PRIMARY KEY, user_id int NOT NULL, amount int NOT NULL);
         INSERT INTO orders (id, user_id, amount) VALUES
             (1, 1, 10),
             (2, 1, 20),
             (3, 2, 5),
             (4, 3, 100);",
    )
    .expect("cte setup");
}

fn ids(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect()
}

#[test]
fn cte_simple() {
    let (_db, mut c) = setup();
    cte_setup(&mut c);
    let got = ids(&mut c, "WITH big AS (SELECT id, amount FROM orders WHERE amount > 15) SELECT id FROM big ORDER BY id");
    assert_eq!(got, [2, 4]);
}

#[test]
fn cte_multiple() {
    let (_db, mut c) = setup();
    cte_setup(&mut c);
    // Second CTE references the first.
    let got = ids(
        &mut c,
        "WITH big AS (SELECT id, amount FROM orders WHERE amount > 5),
              huge AS (SELECT id FROM big WHERE amount > 50)
         SELECT id FROM huge ORDER BY id",
    );
    assert_eq!(got, [4]);
}

#[test]
fn cte_aggregate_from_cte() {
    let (_db, mut c) = setup();
    cte_setup(&mut c);
    let n: i64 = c
        .query_one(
            "WITH per_user AS (SELECT user_id, sum(amount) AS total FROM orders GROUP BY user_id)
             SELECT count(*) FROM per_user WHERE total > 25",
            &[],
        )
        .expect("query")
        .get(0);
    assert_eq!(n, 2);
}

#[test]
fn cte_referenced_twice() {
    let (_db, mut c) = setup();
    cte_setup(&mut c);
    let got = ids(&mut c, "WITH small AS (SELECT id FROM orders WHERE amount < 50) SELECT id FROM small UNION ALL SELECT id FROM small");
    assert_eq!(got.len(), 6); // 3 small rows x 2 references
}
