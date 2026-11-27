use crate::support::{TestDb, expect_code, setup, texts};
use postgres::Client;

fn alter_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL); INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b');")
        .expect("setup");
    (db, c)
}

#[test]
fn alter_table_add_column_nullable() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t ADD COLUMN note text").expect("ALTER ADD");
    let rows = c.query("SELECT id, name, note FROM t ORDER BY id", &[]).expect("select");
    let got: Vec<(i32, String, Option<String>)> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect();
    assert_eq!(got, vec![(1, "a".to_owned(), None), (2, "b".to_owned(), None)]);
    // The new column is writable.
    c.execute("UPDATE t SET note = 'hi' WHERE id = 1", &[]).expect("UPDATE");
    let note: Option<String> = c.query_one("SELECT note FROM t WHERE id = 1", &[]).expect("select after update").get(0);
    assert_eq!(note.as_deref(), Some("hi"));
}

#[test]
fn alter_table_add_column_not_null_on_empty() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE empty_t (id int PRIMARY KEY)").expect("CREATE");
    c.batch_execute("ALTER TABLE empty_t ADD COLUMN flag bool NOT NULL").expect("ALTER ADD NOT NULL");
    c.execute("INSERT INTO empty_t (id, flag) VALUES (1, true)", &[]).expect("INSERT");
}

#[test]
fn alter_table_add_column_not_null_on_non_empty() {
    let (_db, mut c) = alter_setup();
    expect_code(c.batch_execute("ALTER TABLE t ADD COLUMN flag bool NOT NULL"), "23502");
}

#[test]
fn alter_table_add_column_duplicate() {
    let (_db, mut c) = alter_setup();
    expect_code(c.batch_execute("ALTER TABLE t ADD COLUMN name text"), "42701");
}

#[test]
fn alter_table_add_column_if_not_exists() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t ADD COLUMN IF NOT EXISTS extra int").expect("ALTER ADD IF NOT EXISTS");
}

#[test]
fn alter_table_drop_column() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t DROP COLUMN name").expect("ALTER DROP");
    expect_code(c.query("SELECT name FROM t", &[]), "42703");
    let n: i64 = c.query_one("SELECT count(*) FROM t", &[]).expect("count").get(0);
    assert_eq!(n, 2);
    let id: i32 = c.query_one("SELECT id FROM t WHERE id = 1", &[]).expect("select id").get(0);
    assert_eq!(id, 1);
}

#[test]
fn alter_table_drop_column_if_exists_missing() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t DROP COLUMN IF EXISTS missing").expect("DROP IF EXISTS missing");
    expect_code(c.batch_execute("ALTER TABLE t DROP COLUMN missing"), "42703");
}

#[test]
fn alter_table_drop_column_cascade() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t DROP COLUMN name CASCADE").expect("DROP CASCADE");
}

fn index_names(c: &mut Client, table: &str) -> Vec<String> {
    let sql = format!("SELECT indexrelid::regclass::text FROM pg_index WHERE indrelid = '{table}'::regclass ORDER BY 1");
    texts(c, &sql).into_iter().map(|r| r[0].clone().expect("index name")).collect()
}

fn constraint_names(c: &mut Client, table: &str) -> Vec<String> {
    let sql = format!("SELECT conname::text FROM pg_constraint WHERE conrelid = '{table}'::regclass ORDER BY 1");
    texts(c, &sql).into_iter().map(|r| r[0].clone().expect("constraint name")).collect()
}

// Postgres drops every index that reads a dropped column: key column, key expression,
// partial-index predicate or INCLUDE column (#249).
#[test]
fn alter_table_drop_column_drops_indexes() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (a int, b int, c int, d text, e int);
         CREATE INDEX t_ab ON t (a, b);
         CREATE INDEX t_expr ON t ((a + c));
         CREATE INDEX t_lower ON t (lower(d));
         CREATE INDEX t_part ON t (a) WHERE e > 0;
         CREATE INDEX t_incl ON t (a) INCLUDE (e);
         CREATE INDEX t_a ON t (a);",
    )
    .expect("setup");
    c.batch_execute("ALTER TABLE t DROP COLUMN b").expect("drop b");
    assert_eq!(texts(&mut c, "SELECT to_regclass('t_ab')::text"), vec![vec![None]]);
    c.batch_execute("ALTER TABLE t DROP COLUMN c").expect("drop c");
    assert_eq!(texts(&mut c, "SELECT to_regclass('t_expr')::text"), vec![vec![None]]);
    c.batch_execute("ALTER TABLE t DROP COLUMN d").expect("drop d");
    assert_eq!(texts(&mut c, "SELECT to_regclass('t_lower')::text"), vec![vec![None]]);
    c.batch_execute("ALTER TABLE t DROP COLUMN e").expect("drop e");
    assert_eq!(index_names(&mut c, "t"), vec!["t_a"]);
    // The name is free again.
    c.batch_execute("CREATE INDEX t_ab ON t (a)").expect("reuse name");
}

// A primary key or unique constraint over a dropped column goes with its index, even when
// it spans other columns; constraints not using the column stay and are still enforced.
#[test]
fn alter_table_drop_column_drops_key_constraints() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE u (a int, b int, c int, PRIMARY KEY (a, b), UNIQUE (c, a), CONSTRAINT u_c_key2 UNIQUE (c));
         CREATE UNIQUE INDEX u_uidx ON u (b, c);",
    )
    .expect("setup");
    c.batch_execute("ALTER TABLE u DROP COLUMN b").expect("drop b");
    assert_eq!(constraint_names(&mut c, "u"), vec!["u_a_not_null", "u_c_a_key", "u_c_key2"]);
    assert_eq!(index_names(&mut c, "u"), vec!["u_c_a_key", "u_c_key2"]);
    // The primary key's NOT NULL on the surviving column stays.
    assert_eq!(
        texts(&mut c, "SELECT attnotnull::text FROM pg_attribute WHERE attrelid = 'u'::regclass AND attname = 'a'"),
        vec![vec![Some("true".to_owned())]]
    );
    expect_code(c.batch_execute("INSERT INTO u VALUES (1, 1), (1, 1)"), "23505");
    c.batch_execute("ALTER TABLE u DROP COLUMN c").expect("drop c");
    assert_eq!(constraint_names(&mut c, "u"), vec!["u_a_not_null"]);
    assert!(index_names(&mut c, "u").is_empty());
    c.batch_execute("INSERT INTO u VALUES (1), (1)").expect("no unique left");
}

// A unique index mixing a plain column and an expression goes when either is dropped, and
// stops being enforced.
#[test]
fn alter_table_drop_column_drops_unique_expression_index() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE m (a int, b text, c int);
         CREATE UNIQUE INDEX m_mixed ON m (a, lower(b));
         CREATE UNIQUE INDEX m_mixed2 ON m (lower(b), c);
         CREATE TABLE x (a int, b int, EXCLUDE USING btree (b WITH =));",
    )
    .expect("setup");
    c.batch_execute("ALTER TABLE m DROP COLUMN c").expect("drop c");
    assert_eq!(index_names(&mut c, "m"), vec!["m_mixed"]);
    c.batch_execute("ALTER TABLE m DROP COLUMN b").expect("drop b");
    assert!(index_names(&mut c, "m").is_empty());
    c.batch_execute("INSERT INTO m VALUES (1), (1)").expect("no unique left");
    c.batch_execute("ALTER TABLE x DROP COLUMN b").expect("drop b");
    assert!(constraint_names(&mut c, "x").is_empty());
    assert!(index_names(&mut c, "x").is_empty());
}

// On a partitioned table the partitions' indexes go too, with their pg_inherits links.
#[test]
fn alter_table_drop_column_drops_partitioned_indexes() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE pt (a int, b int, c int) PARTITION BY RANGE (a);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);
         CREATE INDEX pt_bc ON pt ((b + c));
         CREATE INDEX pt_a ON pt (a) WHERE c > 0;
         CREATE INDEX pt_ab ON pt (a, b);",
    )
    .expect("setup");
    c.batch_execute("ALTER TABLE pt DROP COLUMN c").expect("drop c");
    assert_eq!(index_names(&mut c, "pt"), vec!["pt_ab"]);
    assert_eq!(index_names(&mut c, "pt1"), vec!["pt1_a_b_idx"]);
    let links = texts(
        &mut c,
        "SELECT inhrelid::regclass::text, inhparent::regclass::text FROM pg_inherits \
         WHERE inhparent IN (SELECT indexrelid FROM pg_index WHERE indrelid = 'pt'::regclass) ORDER BY 1",
    );
    assert_eq!(links, vec![vec![Some("pt1_a_b_idx".to_owned()), Some("pt_ab".to_owned())]]);
}

#[test]
fn alter_table_rename_column() {
    let (_db, mut c) = alter_setup();
    c.batch_execute("ALTER TABLE t RENAME COLUMN name TO label").expect("ALTER RENAME");
    let label: String = c.query_one("SELECT label FROM t WHERE id = 1", &[]).expect("select label").get(0);
    assert_eq!(label, "a");
    expect_code(c.query("SELECT name FROM t", &[]), "42703");
}

#[test]
fn alter_table_rename_column_conflict() {
    let (_db, mut c) = alter_setup();
    expect_code(c.batch_execute("ALTER TABLE t RENAME COLUMN name TO id"), "42701");
}

#[test]
fn alter_table_missing_table() {
    let (_db, mut c) = alter_setup();
    expect_code(c.batch_execute("ALTER TABLE nope ADD COLUMN x int"), "42P01");
}
