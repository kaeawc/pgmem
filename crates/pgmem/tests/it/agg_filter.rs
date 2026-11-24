use crate::support::setup;

fn filter_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE users (id int PRIMARY KEY, region text NOT NULL, active bool NOT NULL, score int NOT NULL);
         INSERT INTO users (id, region, active, score) VALUES
             (1, 'east', true,  100),
             (2, 'east', false, 50),
             (3, 'east', true,  90),
             (4, 'west', true,  200),
             (5, 'west', false, 30);",
    )
    .expect("setup");
}

#[test]
fn agg_filter_count_where_active() {
    let (_db, mut c) = setup();
    filter_setup(&mut c);
    let rows = c
        .query(
            "SELECT region, count(*) FILTER (WHERE active) AS active_count, count(*) AS total
             FROM users GROUP BY region ORDER BY region",
            &[],
        )
        .expect("query");
    let got: Vec<(String, i64, i64)> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!(got, [("east".to_owned(), 2, 3), ("west".to_owned(), 1, 2)]);
}

#[test]
fn agg_filter_sum_positive() {
    let (_db, mut c) = setup();
    filter_setup(&mut c);
    let got: i64 = c.query_one("SELECT sum(score) FILTER (WHERE active) FROM users", &[]).expect("query").get(0);
    assert_eq!(got, 390); // 100 + 90 + 200
}

#[test]
fn agg_filter_null_filter_skips() {
    let (_db, mut c) = setup();
    filter_setup(&mut c);
    // `score > NULL` is NULL, so every row is excluded: sum over an empty set is NULL.
    let got: Option<i64> = c.query_one("SELECT sum(score) FILTER (WHERE score > NULL) FROM users", &[]).expect("query").get(0);
    assert_eq!(got, None);
}

/// FILTER + DISTINCT compose.
#[test]
fn agg_filter_with_distinct() {
    let (_db, mut c) = setup();
    filter_setup(&mut c);
    let got: i64 = c.query_one("SELECT count(DISTINCT score) FILTER (WHERE active) FROM users", &[]).expect("query").get(0);
    assert_eq!(got, 3); // distinct active scores: 100, 90, 200
}
