use crate::support::setup;

fn exists_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE users (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE orders (id int PRIMARY KEY, user_id int NOT NULL);
         INSERT INTO users (id, name) VALUES (1, 'alice'), (2, 'bob'), (3, 'carol');
         INSERT INTO orders (id, user_id) VALUES (10, 1), (11, 1), (12, 2);",
    )
    .expect("exists setup");
}

fn bool_value(c: &mut postgres::Client, sql: &str) -> bool {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn exists_true() {
    let (_db, mut c) = setup();
    exists_setup(&mut c);
    assert!(bool_value(&mut c, "SELECT EXISTS (SELECT 1 FROM users WHERE id = 1)"));
}

#[test]
fn exists_false() {
    let (_db, mut c) = setup();
    exists_setup(&mut c);
    assert!(!bool_value(&mut c, "SELECT EXISTS (SELECT 1 FROM users WHERE id = 999)"));
}

#[test]
fn not_exists() {
    let (_db, mut c) = setup();
    exists_setup(&mut c);
    assert!(bool_value(&mut c, "SELECT NOT EXISTS (SELECT 1 FROM users WHERE id = 999)"));
}

#[test]
fn exists_as_filter() {
    let (_db, mut c) = setup();
    exists_setup(&mut c);
    // Uncorrelated EXISTS acts as a constant predicate: every user passes when the inner plan finds rows.
    let rows = c.query("SELECT id FROM users WHERE EXISTS (SELECT 1 FROM orders WHERE user_id = 1) ORDER BY id", &[]).expect("query");
    let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, [1, 2, 3]);
}
