use crate::support::setup;
use postgres::Client;

fn exec_all(c: &mut Client, stmts: &[&str]) {
    for sql in stmts {
        c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn migration_noop_create_extension() {
    let (_db, mut c) = setup();
    exec_all(&mut c, &[r#"CREATE EXTENSION IF NOT EXISTS "uuid-ossp""#, "CREATE EXTENSION pgcrypto WITH SCHEMA public VERSION '1.3'"]);
}

#[test]
fn migration_noop_drop_extension() {
    let (_db, mut c) = setup();
    exec_all(&mut c, &[r#"DROP EXTENSION IF EXISTS "uuid-ossp" CASCADE"#]);
}

#[test]
fn migration_noop_comment_on() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: Go commented on "uuid-ossp" without creating it; Postgres rejects
    // COMMENT ON EXTENSION for a missing extension (42704), so the extension is created first.
    exec_all(&mut c, &["CREATE TABLE t (id int PRIMARY KEY)", r#"CREATE EXTENSION IF NOT EXISTS "uuid-ossp""#]);
    exec_all(
        &mut c,
        &[
            "COMMENT ON TABLE t IS 'a table'",
            "COMMENT ON COLUMN t.id IS 'primary key'",
            "COMMENT ON SCHEMA public IS 'default'",
            r#"COMMENT ON EXTENSION "uuid-ossp" IS 'uuids'"#,
        ],
    );
}

#[test]
fn migration_noop_reset() {
    let (_db, mut c) = setup();
    exec_all(&mut c, &["RESET ALL", "RESET search_path"]);
}

#[test]
fn migration_noop_discard_all() {
    let (_db, mut c) = setup();
    exec_all(&mut c, &["DISCARD ALL"]);
}

#[test]
fn migration_noop_analyze_vacuum() {
    let (_db, mut c) = setup();
    exec_all(&mut c, &["CREATE TABLE t (id int PRIMARY KEY)", "ANALYZE t", "VACUUM ANALYZE t", "VACUUM"]);
}

#[test]
fn migration_noop_create_drop_schema() {
    let (_db, mut c) = setup();
    exec_all(&mut c, &["CREATE SCHEMA IF NOT EXISTS public", "DROP SCHEMA IF EXISTS old CASCADE"]);
}
