use crate::support::{setup, text_value};
use pgmem::SqlValue;

/// M0 acceptance: a stock client connects, runs SELECT 1 over the extended protocol and gets 1 back.
#[test]
fn select1_pgx() {
    let (_db, mut c) = setup();
    let n: i32 = c.query_one("SELECT 1", &[]).expect("query").get(0);
    assert_eq!(n, 1);
}

/// The pool path the README example uses.
#[test]
fn select1_pgxpool() {
    let (db, _) = setup();
    let mut pool = db.connect();
    let n: i32 = pool.query_one("SELECT 1", &[]).expect("query").get(0);
    assert_eq!(n, 1);
}

/// M1 acceptance: a sqlc-generated GetUsers stand-in returns the seeded rows with the right types.
#[test]
fn get_users_sqlc_style() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE users (id int NOT NULL, name text NOT NULL)").expect("create users");
    let users = [(1, "alice"), (2, "bob"), (3, "carol")];
    match db.server() {
        Some(server) => server
            .seed("users", users.iter().map(|&(id, name)| vec![SqlValue::Int4(id), SqlValue::Text(name.into())]).collect())
            .expect("seed"),
        None => c.batch_execute("INSERT INTO users VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')").expect("insert users"),
    }
    let rows = c.query("SELECT id, name FROM users", &[]).expect("query");
    let got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    let want: Vec<(i32, String)> = users.iter().map(|&(id, name)| (id, name.to_owned())).collect();
    assert_eq!(got, want);
}

/// Simple-query mode: bypasses Parse/Bind/Describe/Execute and goes straight to Query.
#[test]
fn simple_query_protocol() {
    let (_db, mut c) = setup();
    assert_eq!(text_value(&mut c, "SELECT 1").as_deref(), Some("1"));
}
