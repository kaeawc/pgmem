use crate::support::{TestDb, setup};
use postgres::Client;

fn qstar_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE users (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, user_id int NOT NULL, amount int NOT NULL);
         INSERT INTO users (id, name) VALUES (1, 'alice');
         INSERT INTO orders (id, user_id, amount) VALUES (10, 1, 100)",
    )
    .expect("setup");
    (db, c)
}

fn column_count(c: &mut Client, sql: &str) -> usize {
    c.prepare(sql).expect("prepare").columns().len()
}

#[test]
fn qualified_star_all_from_one_side() {
    let (_db, mut c) = qstar_setup();
    let rows = c.query("SELECT u.* FROM users u JOIN orders o ON u.id = o.user_id", &[]).expect("query");
    assert_eq!(rows.len(), 1, "expected a row");
    assert_eq!(rows[0].len(), 2, "want 2 cols (id, name)");
    let (id, name): (i32, String) = (rows[0].get(0), rows[0].get(1));
    assert_eq!((id, name.as_str()), (1, "alice"));
}

#[test]
fn qualified_star_both_sides() {
    let (_db, mut c) = qstar_setup();
    // users(id, name) + orders(id, user_id, amount) = 5 columns.
    assert_eq!(column_count(&mut c, "SELECT u.*, o.* FROM users u JOIN orders o ON u.id = o.user_id"), 5);
}

#[test]
fn qualified_star_mixed_with_expr() {
    let (_db, mut c) = qstar_setup();
    assert_eq!(column_count(&mut c, "SELECT u.*, o.amount FROM users u JOIN orders o ON u.id = o.user_id"), 3);
}
