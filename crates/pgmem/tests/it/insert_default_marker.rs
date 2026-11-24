use crate::support::setup;

#[test]
fn insert_default_picks_up_column_default() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, status text NOT NULL DEFAULT 'pending')").expect("create");
    c.execute("INSERT INTO t (id, status) VALUES (1, DEFAULT)", &[]).expect("insert");
    let status: String = c.query_one("SELECT status FROM t WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(status, "pending");
}

/// An explicit DEFAULT for a column without a default produces NULL.
#[test]
fn insert_default_no_column_default_leaves_null() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, note text)").expect("create");
    c.execute("INSERT INTO t (id, note) VALUES (1, DEFAULT)", &[]).expect("insert");
    let note: Option<String> = c.query_one("SELECT note FROM t WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(note, None);
}

/// An auto column given an explicit DEFAULT still gets the next sequence value.
#[test]
fn insert_default_auto_column() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id serial PRIMARY KEY, label text NOT NULL DEFAULT 'x')").expect("create");
    c.execute("INSERT INTO t (id, label) VALUES (DEFAULT, DEFAULT)", &[]).expect("insert");
    let row = c.query_one("SELECT id, label FROM t", &[]).expect("select");
    let (id, label): (i32, String) = (row.get(0), row.get(1));
    assert_eq!((id, label.as_str()), (1, "x"));
}

#[test]
fn insert_default_multi_row() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, status text DEFAULT 'pending')").expect("create");
    c.execute("INSERT INTO t (id, status) VALUES (1, 'live'), (2, DEFAULT), (3, 'archived')", &[]).expect("insert");
    let rows = c.query("SELECT id, status FROM t ORDER BY id", &[]).expect("select");
    let got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    let want = [(1, "live"), (2, "pending"), (3, "archived")].map(|(id, s)| (id, s.to_owned()));
    assert_eq!(got, want);
}
