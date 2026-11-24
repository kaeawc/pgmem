use crate::support::setup;

fn correlated_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE parents (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE children (id int PRIMARY KEY, parent_id int NOT NULL REFERENCES parents(id));
         INSERT INTO parents (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c');
         INSERT INTO children (id, parent_id) VALUES (10, 1), (11, 2);",
    )
    .expect("correlated setup");
}

fn ids(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect()
}

#[test]
fn correlated_exists_filters_by_outer_row() {
    let (_db, mut c) = setup();
    correlated_setup(&mut c);
    let got = ids(&mut c, "SELECT p.id FROM parents p WHERE EXISTS (SELECT 1 FROM children c WHERE c.parent_id = p.id) ORDER BY p.id");
    assert_eq!(got, [1, 2]);
}

#[test]
fn correlated_exists_not_exists() {
    let (_db, mut c) = setup();
    correlated_setup(&mut c);
    let got = ids(&mut c, "SELECT p.id FROM parents p WHERE NOT EXISTS (SELECT 1 FROM children c WHERE c.parent_id = p.id) ORDER BY p.id");
    assert_eq!(got, [3]);
}

#[test]
fn correlated_scalar_subquery() {
    let (_db, mut c) = setup();
    correlated_setup(&mut c);
    let rows = c
        .query("SELECT p.id, (SELECT count(*) FROM children c WHERE c.parent_id = p.id) AS n FROM parents p ORDER BY p.id", &[])
        .expect("query");
    let got: Vec<(i32, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [(1, 1), (2, 1), (3, 0)]);
}

#[test]
fn correlated_scalar_subquery_null_on_empty() {
    let (_db, mut c) = setup();
    correlated_setup(&mut c);
    let rows = c
        .query(
            "SELECT p.id, (SELECT c.id FROM children c WHERE c.parent_id = p.id LIMIT 1) AS first_child FROM parents p ORDER BY p.id",
            &[],
        )
        .expect("query");
    let got: Vec<(i32, Option<i32>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [(1, Some(10)), (2, Some(11)), (3, None)]);
}

#[test]
fn uncorrelated_exists_still_fast_path() {
    let (_db, mut c) = setup();
    correlated_setup(&mut c);
    let got: bool = c.query_one("SELECT EXISTS (SELECT 1 FROM children WHERE id = 99)", &[]).expect("query").get(0);
    assert!(!got);
}
