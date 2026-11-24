use crate::support::setup;

fn nulls_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, n int);
         INSERT INTO t (id, n) VALUES (1, 10), (2, NULL), (3, 30), (4, NULL), (5, 20);",
    )
    .expect("setup");
}

fn ids(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect()
}

/// PG default: ASC means NULLS LAST.
#[test]
fn nulls_order_asc_default() {
    let (_db, mut c) = setup();
    nulls_setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM t ORDER BY n ASC, id"), [1, 5, 3, 2, 4]);
}

/// PG default: DESC means NULLS FIRST.
#[test]
fn nulls_order_desc_default() {
    let (_db, mut c) = setup();
    nulls_setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM t ORDER BY n DESC, id"), [2, 4, 3, 5, 1]);
}

#[test]
fn nulls_order_asc_nulls_first() {
    let (_db, mut c) = setup();
    nulls_setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM t ORDER BY n ASC NULLS FIRST, id"), [2, 4, 1, 5, 3]);
}

#[test]
fn nulls_order_desc_nulls_last() {
    let (_db, mut c) = setup();
    nulls_setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM t ORDER BY n DESC NULLS LAST, id"), [3, 5, 1, 2, 4]);
}
