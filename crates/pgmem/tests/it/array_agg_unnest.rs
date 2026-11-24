use crate::support::setup;

fn aa_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, kind text NOT NULL, n int NOT NULL);
         INSERT INTO t (id, kind, n) VALUES (1, 'a', 10), (2, 'a', 20), (3, 'b', 30), (4, 'b', 40);",
    )
    .expect("setup");
}

#[test]
fn array_agg_int() {
    let (_db, mut c) = setup();
    aa_setup(&mut c);
    let rows = c.query("SELECT kind, array_agg(n) FROM t GROUP BY kind ORDER BY kind", &[]).expect("query");
    let mut got: Vec<(String, Vec<i32>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    // Element order inside array_agg is unspecified without ORDER BY.
    got.iter_mut().for_each(|(_, ns)| ns.sort());
    assert_eq!(got, [("a".to_owned(), vec![10, 20]), ("b".to_owned(), vec![30, 40])]);
}

#[test]
fn array_agg_text() {
    let (_db, mut c) = setup();
    aa_setup(&mut c);
    let got: Vec<String> = c.query_one("SELECT array_agg(kind) FROM t WHERE n < 30", &[]).expect("query").get(0);
    assert_eq!(got, ["a", "a"]);
}

#[test]
fn unnest_int() {
    let (_db, mut c) = setup();
    aa_setup(&mut c);
    let rows = c.query("SELECT id FROM t WHERE id IN (SELECT * FROM unnest($1::bigint[])) ORDER BY id", &[&vec![2i64, 4]]).expect("query");
    let got: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, [2, 4]);
}

#[test]
fn unnest_text() {
    let (_db, mut c) = setup();
    aa_setup(&mut c);
    let rows = c.query("SELECT u FROM unnest($1::text[]) AS u ORDER BY u", &[&vec!["banana", "apple", "cherry"]]).expect("query");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["apple", "banana", "cherry"]);
}

/// Round-trip: array_agg into unnest.
#[test]
fn array_agg_then_unnest() {
    let (_db, mut c) = setup();
    aa_setup(&mut c);
    let got: i64 = c.query_one("SELECT count(*) FROM unnest((SELECT array_agg(n) FROM t)::bigint[])", &[]).expect("query").get(0);
    assert_eq!(got, 4);
}
