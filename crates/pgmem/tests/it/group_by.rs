use crate::support::{expect_code, message, setup};

fn group_by_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE sales (id int PRIMARY KEY, region text NOT NULL, amount int NOT NULL);
         INSERT INTO sales (id, region, amount) VALUES
             (1, 'east', 10),
             (2, 'east', 20),
             (3, 'west', 5),
             (4, 'west', 30),
             (5, 'south', 100);",
    )
    .expect("setup");
}

fn region_sums(c: &mut postgres::Client, sql: &str) -> Vec<(String, i64)> {
    c.query(sql, &[]).expect(sql).iter().map(|r| (r.get(0), r.get(1))).collect()
}

#[test]
fn group_by_sum_per_region() {
    let (_db, mut c) = setup();
    group_by_setup(&mut c);
    let mut got = region_sums(&mut c, "SELECT region, sum(amount) FROM sales GROUP BY region");
    got.sort();
    assert_eq!(got, [("east".to_owned(), 30), ("south".to_owned(), 100), ("west".to_owned(), 35)]);
}

#[test]
fn group_by_count_and_order() {
    let (_db, mut c) = setup();
    group_by_setup(&mut c);
    let got = region_sums(&mut c, "SELECT region, count(*) FROM sales GROUP BY region ORDER BY region");
    assert_eq!(got, [("east".to_owned(), 2), ("south".to_owned(), 1), ("west".to_owned(), 2)]);
}

#[test]
fn group_by_having_filter() {
    let (_db, mut c) = setup();
    group_by_setup(&mut c);
    let got = region_sums(&mut c, "SELECT region, sum(amount) FROM sales GROUP BY region HAVING sum(amount) > 30 ORDER BY region");
    assert_eq!(got, [("south".to_owned(), 100), ("west".to_owned(), 35)]);
}

#[test]
fn group_by_rejects_bare_column_not_in_group_by() {
    let (_db, mut c) = setup();
    group_by_setup(&mut c);
    let e = expect_code(c.execute("SELECT id, sum(amount) FROM sales GROUP BY region", &[]), "42803");
    assert_eq!(message(&e), r#"column "sales.id" must appear in the GROUP BY clause or be used in an aggregate function"#);
}
