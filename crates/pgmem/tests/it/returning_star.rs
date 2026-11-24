use crate::support::{TestDb, setup};
use postgres::Client;

fn returning_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL, n int)").expect("create");
    (db, c)
}

fn one_row(c: &mut Client, sql: &str) -> (i32, String, Option<i32>) {
    let r = c.query_one(sql, &[]).expect(sql);
    assert_eq!(r.len(), 3, "RETURNING * columns");
    (r.get(0), r.get(1), r.get(2))
}

#[test]
fn returning_star_insert() {
    let (_db, mut c) = returning_setup();
    let got = one_row(&mut c, "INSERT INTO t (id, name, n) VALUES (1, 'alice', 10) RETURNING *");
    assert_eq!(got, (1, "alice".to_owned(), Some(10)));
}

#[test]
fn returning_star_update() {
    let (_db, mut c) = returning_setup();
    c.batch_execute("INSERT INTO t (id, name, n) VALUES (2, 'bob', 5)").expect("insert");
    let got = one_row(&mut c, "UPDATE t SET n = n + 1 WHERE id = 2 RETURNING *");
    assert_eq!(got, (2, "bob".to_owned(), Some(6)));
}

#[test]
fn returning_star_delete() {
    let (_db, mut c) = returning_setup();
    c.batch_execute("INSERT INTO t (id, name, n) VALUES (3, 'carol', NULL)").expect("insert");
    let got = one_row(&mut c, "DELETE FROM t WHERE id = 3 RETURNING *");
    assert_eq!(got, (3, "carol".to_owned(), None));
}

#[test]
fn returning_star_mixed_with_expr() {
    let (_db, mut c) = returning_setup();
    let rows = c.query("INSERT INTO t (id, name, n) VALUES (4, 'dan', 100) RETURNING *, n * 2", &[]).expect("insert");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 4, "want 4 cols (id, name, n, n*2)");
}
