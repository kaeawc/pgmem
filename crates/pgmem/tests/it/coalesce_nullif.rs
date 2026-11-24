use crate::support::setup;

#[test]
fn coalesce_first_non_null() {
    let (_db, mut c) = setup();
    for (sql, want) in [
        ("SELECT COALESCE('first', 'second')", "first"),
        ("SELECT COALESCE(NULL, 'second')", "second"),
        ("SELECT COALESCE(NULL, NULL, 'third')", "third"),
    ] {
        let got: String = c.query_one(sql, &[]).unwrap_or_else(|e| panic!("{sql}: {e}")).get(0);
        assert_eq!(got, want, "{sql}");
    }
    let n: i32 = c.query_one("SELECT COALESCE(NULL, 42)", &[]).expect("int coalesce").get(0);
    assert_eq!(n, 42);
}

#[test]
fn coalesce_all_null() {
    let (_db, mut c) = setup();
    let s: Option<String> = c.query_one("SELECT COALESCE(NULL::text, NULL::text)", &[]).expect("select").get(0);
    assert_eq!(s, None);
    let s: Option<String> = c.query_one("SELECT COALESCE(NULL, NULL)", &[]).expect("select untyped").get(0);
    assert_eq!(s, None);
}

#[test]
fn nullif_equals_returns_null() {
    let (_db, mut c) = setup();
    let s: Option<String> = c.query_one("SELECT NULLIF('x', 'x')", &[]).expect("select").get(0);
    assert_eq!(s, None);
}

#[test]
fn nullif_different_returns_first() {
    let (_db, mut c) = setup();
    let s: String = c.query_one("SELECT NULLIF('x', 'y')", &[]).expect("select").get(0);
    assert_eq!(s, "x");
}

#[test]
fn coalesce_on_column() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE users (id int PRIMARY KEY, name text); INSERT INTO users (id, name) VALUES (1, 'alice'), (2, NULL)")
        .expect("setup");
    for (id, want) in [(1i32, "alice"), (2, "unknown")] {
        let got: String = c
            .query_one("SELECT COALESCE(name, 'unknown') FROM users WHERE id = $1", &[&id])
            .unwrap_or_else(|e| panic!("id={id}: {e}"))
            .get(0);
        assert_eq!(got, want, "id={id}");
    }
}
