use crate::support::{TestDb, setup};
use postgres::Client;

fn alias_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE users (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, user_id int NOT NULL, amount int NOT NULL);
         INSERT INTO users (id, name) VALUES (1, 'alice'), (2, 'bob');
         INSERT INTO orders (id, user_id, amount) VALUES (10, 1, 100), (11, 2, 50)",
    )
    .expect("setup");
    (db, c)
}

#[test]
fn table_alias_implicit() {
    let (_db, mut c) = alias_setup();
    let name: String = c.query_one("SELECT u.name FROM users u WHERE u.id = 1", &[]).expect("query").get(0);
    assert_eq!(name, "alice");
}

#[test]
fn table_alias_as() {
    let (_db, mut c) = alias_setup();
    let name: String = c.query_one("SELECT u.name FROM users AS u WHERE u.id = 2", &[]).expect("query").get(0);
    assert_eq!(name, "bob");
}

#[test]
fn table_alias_join() {
    let (_db, mut c) = alias_setup();
    let rows = c.query("SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id ORDER BY u.id", &[]).expect("query");
    let got: Vec<(String, i32)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, vec![("alice".to_owned(), 100), ("bob".to_owned(), 50)]);
}
