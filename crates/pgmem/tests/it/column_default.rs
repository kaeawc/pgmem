use crate::support::setup;
use jiff::{SignedDuration, Timestamp};

#[test]
fn column_default_literal_fills_omitted_column() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, status text NOT NULL DEFAULT 'pending')").expect("create");
    c.execute("INSERT INTO t (id) VALUES (1)", &[]).expect("insert");
    let status: String = c.query_one("SELECT status FROM t WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(status, "pending");
}

#[test]
fn column_default_explicit_value_wins() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, status text NOT NULL DEFAULT 'pending')").expect("create");
    c.execute("INSERT INTO t (id, status) VALUES (1, 'active')", &[]).expect("insert");
    let status: String = c.query_one("SELECT status FROM t WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(status, "active");
}

/// An explicit NULL does not trigger the default: DEFAULT only applies when the column is omitted.
#[test]
fn column_default_explicit_null_keeps_null() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, note text DEFAULT 'hi')").expect("create");
    c.execute("INSERT INTO t (id, note) VALUES (1, NULL)", &[]).expect("insert");
    let note: Option<String> = c.query_one("SELECT note FROM t WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(note, None);
}

#[test]
fn column_default_now_fills_timestamp() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now())").expect("create");
    let before = Timestamp::now() - SignedDuration::from_secs(1);
    c.execute("INSERT INTO t (id) VALUES (1)", &[]).expect("insert");
    let after = Timestamp::now() + SignedDuration::from_secs(1);
    let got: Timestamp = c.query_one("SELECT created_at FROM t WHERE id = 1", &[]).expect("select").get(0);
    assert!(before <= got && got <= after, "created_at = {got}, want between {before} and {after}");
}

#[test]
fn column_default_default_values_uses_defaults() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id serial PRIMARY KEY, status text NOT NULL DEFAULT 'queued')").expect("create");
    c.execute("INSERT INTO t DEFAULT VALUES", &[]).expect("insert");
    let row = c.query_one("SELECT id, status FROM t", &[]).expect("select");
    let (id, status): (i32, String) = (row.get(0), row.get(1));
    assert_eq!((id, status.as_str()), (1, "queued"));
}

#[test]
fn column_default_alter_add_column_with_default() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY)").expect("create");
    c.execute("INSERT INTO t (id) VALUES (1)", &[]).expect("insert 1");
    c.execute("ALTER TABLE t ADD COLUMN status text DEFAULT 'new'", &[]).expect("alter");
    c.execute("INSERT INTO t (id) VALUES (2)", &[]).expect("insert 2");
    let rows = c.query("SELECT id, status FROM t ORDER BY id", &[]).expect("select");
    let got: Vec<(i32, Option<String>)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    // Deviation from pgmem-go: pgmem-go left pre-existing rows NULL (only row 2 was checked) vs Postgres
    // backfilling existing rows with the new column's default.
    assert_eq!(got, vec![(1, Some("new".to_owned())), (2, Some("new".to_owned()))]);
}
