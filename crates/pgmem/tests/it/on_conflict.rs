use crate::support::{TestDb, expect_code, setup};
use postgres::Client;

fn conflict_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL); INSERT INTO t (id, name) VALUES (1, 'alice');")
        .expect("setup");
    (db, c)
}

fn name_of(c: &mut Client, id: i32) -> String {
    c.query_one("SELECT name FROM t WHERE id = $1", &[&id]).expect("readback").get(0)
}

fn count(c: &mut Client) -> i64 {
    c.query_one("SELECT count(*) FROM t", &[]).expect("count").get(0)
}

#[test]
fn on_conflict_do_nothing_skips() {
    let (_db, mut c) = conflict_setup();
    let n = c.execute("INSERT INTO t (id, name) VALUES (1, 'duplicate') ON CONFLICT (id) DO NOTHING", &[]).expect("upsert");
    assert_eq!(n, 0);
    assert_eq!(name_of(&mut c, 1), "alice");
}

#[test]
fn on_conflict_do_nothing_inserts_new() {
    let (_db, mut c) = conflict_setup();
    let n = c.execute("INSERT INTO t (id, name) VALUES (2, 'bob') ON CONFLICT (id) DO NOTHING", &[]).expect("insert");
    assert_eq!(n, 1);
}

#[test]
fn on_conflict_do_nothing_mixed_batch() {
    let (_db, mut c) = conflict_setup();
    let n = c.execute("INSERT INTO t (id, name) VALUES (1, 'dup'), (3, 'carol') ON CONFLICT (id) DO NOTHING", &[]).expect("upsert");
    assert_eq!(n, 1);
    assert_eq!(count(&mut c), 2, "alice, carol");
}

#[test]
fn on_conflict_do_update_excluded() {
    let (_db, mut c) = conflict_setup();
    let n = c
        .execute("INSERT INTO t (id, name) VALUES (1, 'updated') ON CONFLICT (id) DO UPDATE SET name = excluded.name", &[])
        .expect("upsert");
    assert_eq!(n, 1);
    assert_eq!(name_of(&mut c, 1), "updated");
}

#[test]
fn on_conflict_do_update_references_existing() {
    let (_db, mut c) = conflict_setup();
    // Deviation from pgmem-go: Go expected an unqualified `name` to resolve to the existing row;
    // Postgres rejects it as ambiguous (42702) because both `t` and `excluded` are in scope.
    let ambiguous = "INSERT INTO t (id, name) VALUES (1, 'bob') ON CONFLICT (id) DO UPDATE SET name = name || ' / ' || excluded.name";
    expect_code(c.execute(ambiguous, &[]), "42702");
    c.execute("INSERT INTO t (id, name) VALUES (1, 'bob') ON CONFLICT (id) DO UPDATE SET name = t.name || ' / ' || excluded.name", &[])
        .expect("upsert");
    assert_eq!(name_of(&mut c, 1), "alice / bob");
}

#[test]
fn on_conflict_do_update_returning() {
    let (_db, mut c) = conflict_setup();
    let row = c
        .query_one(
            "INSERT INTO t (id, name) VALUES (1, 'changed') ON CONFLICT (id) DO UPDATE SET name = excluded.name RETURNING id, name",
            &[],
        )
        .expect("query");
    let (id, name): (i32, String) = (row.get(0), row.get(1));
    assert_eq!((id, name.as_str()), (1, "changed"));
}

#[test]
fn on_conflict_do_update_new_row_still_inserts() {
    let (_db, mut c) = conflict_setup();
    let n =
        c.execute("INSERT INTO t (id, name) VALUES (5, 'new') ON CONFLICT (id) DO UPDATE SET name = excluded.name", &[]).expect("insert");
    assert_eq!(n, 1);
    assert_eq!(count(&mut c), 2, "alice + new");
}

#[test]
fn on_conflict_returning_only_for_actual_inserts() {
    let (_db, mut c) = conflict_setup();
    let rows = c.query("INSERT INTO t (id, name) VALUES (1, 'dup') ON CONFLICT (id) DO NOTHING RETURNING id", &[]).expect("query");
    assert!(rows.is_empty(), "got a RETURNING row for a skipped conflict");
}
