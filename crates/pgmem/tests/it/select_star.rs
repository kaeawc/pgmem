use crate::support::{TestDb, setup};
use postgres::Client;

fn star_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL, n int);
         INSERT INTO t (id, name, n) VALUES (1, 'a', 10), (2, 'b', 20), (3, 'c', NULL)",
    )
    .expect("setup");
    (db, c)
}

fn column_names(c: &mut Client, sql: &str) -> Vec<String> {
    let stmt = c.prepare(sql).expect("prepare");
    stmt.columns().iter().map(|col| col.name().to_owned()).collect()
}

#[test]
fn select_star_all_columns() {
    let (_db, mut c) = star_setup();
    let rows = c.query("SELECT * FROM t ORDER BY id", &[]).expect("select");
    let got: Vec<(i32, String, Option<i32>)> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!(got.len(), 3);
    assert_eq!(got[0], (1, "a".to_owned(), Some(10)));
    assert_eq!((got[2].0, got[2].2), (3, None));
}

/// Expanded columns retain their source names — useful for sqlc-generated `RETURNING *` codegen.
#[test]
fn select_star_column_names() {
    let (_db, mut c) = star_setup();
    let rows = c.query("SELECT * FROM t WHERE id = 1", &[]).expect("select");
    assert_eq!(rows.len(), 1, "expected a row");
    let names: Vec<&str> = rows[0].columns().iter().map(|col| col.name()).collect();
    assert_eq!(names, ["id", "name", "n"]);
}

/// `SELECT * FROM a JOIN b ON ...` expands to every column from both sides in left-then-right order.
#[test]
fn select_star_join() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE a (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE b (id int PRIMARY KEY, a_id int NOT NULL, qty int);
         INSERT INTO a (id, name) VALUES (1, 'alice');
         INSERT INTO b (id, a_id, qty) VALUES (10, 1, 5)",
    )
    .expect("setup");
    assert_eq!(column_names(&mut c, "SELECT * FROM a JOIN b ON a.id = b.a_id"), ["id", "name", "id", "a_id", "qty"]);
}
