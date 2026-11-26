//! ALTER TABLE column / table attributes: SET STATISTICS | STORAGE | COMPRESSION, REPLICA
//! IDENTITY, CLUSTER ON, SET [UN]LOGGED, reflected in pg_attribute / pg_class / pg_index.

use crate::support::{expect_code, message, setup, text_value, texts};
use postgres::Client;

fn base() -> (crate::support::TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (a int, b text, c int NOT NULL, d numeric, e uuid);
         CREATE INDEX t_b ON t (b);
         CREATE UNIQUE INDEX t_c ON t (c);
         CREATE UNIQUE INDEX t_a_part ON t (a) WHERE a > 0;
         CREATE UNIQUE INDEX t_expr ON t ((a + 1));
         CREATE UNIQUE INDEX t_a ON t (a);
         CREATE TABLE o (a int NOT NULL);
         CREATE UNIQUE INDEX o_a ON o (a);
         CREATE TABLE def (a int NOT NULL, UNIQUE (a) DEFERRABLE);",
    )
    .expect("setup");
    (db, c)
}

fn attr(c: &mut Client, table: &str, col: &str, what: &str) -> String {
    let sql = format!("SELECT {what}::text FROM pg_attribute WHERE attrelid = '{table}'::regclass AND attname = '{col}'");
    text_value(c, &sql).unwrap_or_else(|| "NULL".to_owned())
}

fn flags(c: &mut Client, table: &str, col: &str) -> Vec<(String, String)> {
    texts(c, &format!("SELECT indexrelid::regclass::text, {col}::text FROM pg_index WHERE indrelid = '{table}'::regclass ORDER BY 1"))
        .into_iter()
        .map(|r| (r[0].clone().expect("name"), r[1].clone().expect("flag")))
        .collect()
}

fn on(flags: &[(String, String)]) -> Vec<&str> {
    flags.iter().filter(|(_, v)| v == "true").map(|(n, _)| n.as_str()).collect()
}

#[test]
fn defaults_unchanged() {
    let (_db, mut c) = base();
    assert_eq!(attr(&mut c, "t", "a", "attstattarget"), "NULL");
    assert_eq!(attr(&mut c, "t", "a", "attstorage"), "p");
    assert_eq!(attr(&mut c, "t", "b", "attstorage"), "x");
    assert_eq!(attr(&mut c, "t", "d", "attstorage"), "m");
    assert_eq!(attr(&mut c, "t", "b", "attcompression"), "");
    assert_eq!(text_value(&mut c, "SELECT relreplident::text FROM pg_class WHERE oid = 't'::regclass").as_deref(), Some("d"));
    assert_eq!(text_value(&mut c, "SELECT relpersistence::text FROM pg_class WHERE oid = 't'::regclass").as_deref(), Some("p"));
    assert!(on(&flags(&mut c, "t", "indisclustered")).is_empty());
    assert!(on(&flags(&mut c, "t", "indisreplident")).is_empty());
}

#[test]
fn set_statistics() {
    let (_db, mut c) = base();
    c.batch_execute("ALTER TABLE t ALTER COLUMN a SET STATISTICS 500").expect("set");
    assert_eq!(attr(&mut c, "t", "a", "attstattarget"), "500");
    c.batch_execute("ALTER TABLE t ALTER a SET STATISTICS 0").expect("zero");
    assert_eq!(attr(&mut c, "t", "a", "attstattarget"), "0");
    c.batch_execute("ALTER TABLE t ALTER COLUMN a SET STATISTICS -1").expect("reset");
    assert_eq!(attr(&mut c, "t", "a", "attstattarget"), "NULL");
    // Above the maximum it is lowered (with a warning).
    c.batch_execute("ALTER TABLE t ALTER COLUMN a SET STATISTICS 10001").expect("lowered");
    assert_eq!(attr(&mut c, "t", "a", "attstattarget"), "10000");
    expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN a SET STATISTICS -2"), "22023");
    expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN nope SET STATISTICS 1"), "42703");
    expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN xmin SET STATISTICS 1"), "0A000");
    expect_code(c.batch_execute("ALTER TABLE nope ALTER COLUMN a SET STATISTICS 1"), "42P01");
    c.batch_execute("ALTER TABLE IF EXISTS nope ALTER COLUMN a SET STATISTICS 1").expect("if exists");
}

#[test]
fn set_storage() {
    let (_db, mut c) = base();
    c.batch_execute("ALTER TABLE t ALTER COLUMN b SET STORAGE EXTERNAL").expect("external");
    assert_eq!(attr(&mut c, "t", "b", "attstorage"), "e");
    c.batch_execute("ALTER TABLE t ALTER COLUMN b SET STORAGE main").expect("main");
    assert_eq!(attr(&mut c, "t", "b", "attstorage"), "m");
    c.batch_execute("ALTER TABLE t ALTER COLUMN b SET STORAGE PLAIN").expect("plain");
    assert_eq!(attr(&mut c, "t", "b", "attstorage"), "p");
    c.batch_execute("ALTER TABLE t ALTER COLUMN b SET STORAGE DEFAULT").expect("default");
    assert_eq!(attr(&mut c, "t", "b", "attstorage"), "x");
    // Fixed-length types only have plain storage.
    c.batch_execute("ALTER TABLE t ALTER COLUMN a SET STORAGE PLAIN").expect("int plain");
    let e = expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN a SET STORAGE EXTERNAL"), "0A000");
    assert_eq!(message(&e), "column data type integer can only have storage PLAIN");
    let e = expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN b SET STORAGE foo"), "22023");
    assert_eq!(message(&e), "invalid storage type \"foo\"");
    expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN nope SET STORAGE MAIN"), "42703");
    // A type change resets the column to the new type's default.
    c.batch_execute("ALTER TABLE t ALTER COLUMN b SET STORAGE EXTERNAL; ALTER TABLE t ALTER COLUMN b TYPE varchar").expect("retype");
    assert_eq!(attr(&mut c, "t", "b", "attstorage"), "x");
}

#[test]
fn set_compression() {
    let (_db, mut c) = base();
    c.batch_execute("ALTER TABLE t ALTER COLUMN b SET COMPRESSION pglz").expect("pglz");
    assert_eq!(attr(&mut c, "t", "b", "attcompression"), "p");
    c.batch_execute("ALTER TABLE t ALTER COLUMN b SET COMPRESSION lz4").expect("lz4");
    assert_eq!(attr(&mut c, "t", "b", "attcompression"), "l");
    c.batch_execute("ALTER TABLE t ALTER COLUMN b SET COMPRESSION DEFAULT").expect("default");
    assert_eq!(attr(&mut c, "t", "b", "attcompression"), "");
    let e = expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN b SET COMPRESSION foo"), "22023");
    assert_eq!(message(&e), "invalid compression method \"foo\"");
    let e = expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN a SET COMPRESSION lz4"), "0A000");
    assert_eq!(message(&e), "column data type integer does not support compression");
}

#[test]
fn replica_identity() {
    let (_db, mut c) = base();
    let rel = |c: &mut Client| text_value(c, "SELECT relreplident::text FROM pg_class WHERE oid = 't'::regclass").expect("row");
    c.batch_execute("ALTER TABLE t REPLICA IDENTITY FULL").expect("full");
    assert_eq!(rel(&mut c), "f");
    c.batch_execute("ALTER TABLE t REPLICA IDENTITY NOTHING").expect("nothing");
    assert_eq!(rel(&mut c), "n");
    c.batch_execute("ALTER TABLE t REPLICA IDENTITY USING INDEX t_c").expect("using index");
    assert_eq!(rel(&mut c), "i");
    assert_eq!(on(&flags(&mut c, "t", "indisreplident")), vec!["t_c"]);
    c.batch_execute("ALTER TABLE t REPLICA IDENTITY FULL").expect("full again");
    assert!(on(&flags(&mut c, "t", "indisreplident")).is_empty());
    c.batch_execute("ALTER TABLE t REPLICA IDENTITY USING INDEX t_c").expect("using index");
    c.batch_execute("ALTER TABLE t REPLICA IDENTITY DEFAULT").expect("default");
    assert_eq!(rel(&mut c), "d");
    assert!(on(&flags(&mut c, "t", "indisreplident")).is_empty());
    // Primary key indexes work too.
    c.batch_execute("CREATE TABLE pk (a int PRIMARY KEY); ALTER TABLE pk REPLICA IDENTITY USING INDEX pk_pkey").expect("pk");
    assert_eq!(on(&flags(&mut c, "pk", "indisreplident")), vec!["pk_pkey"]);
}

#[test]
fn replica_identity_errors() {
    let (_db, mut c) = base();
    let e = expect_code(c.batch_execute("ALTER TABLE t REPLICA IDENTITY USING INDEX t_b"), "42809");
    assert_eq!(message(&e), "cannot use non-unique index \"t_b\" as replica identity");
    let e = expect_code(c.batch_execute("ALTER TABLE t REPLICA IDENTITY USING INDEX t_a_part"), "0A000");
    assert_eq!(message(&e), "cannot use partial index \"t_a_part\" as replica identity");
    let e = expect_code(c.batch_execute("ALTER TABLE t REPLICA IDENTITY USING INDEX t_expr"), "0A000");
    assert_eq!(message(&e), "cannot use expression index \"t_expr\" as replica identity");
    let e = expect_code(c.batch_execute("ALTER TABLE t REPLICA IDENTITY USING INDEX t_a"), "42809");
    assert_eq!(message(&e), "index \"t_a\" cannot be used as replica identity because column \"a\" is nullable");
    let e = expect_code(c.batch_execute("ALTER TABLE t REPLICA IDENTITY USING INDEX nope"), "42704");
    assert_eq!(message(&e), "index \"nope\" for table \"t\" does not exist");
    let e = expect_code(c.batch_execute("ALTER TABLE t REPLICA IDENTITY USING INDEX o_a"), "42809");
    assert_eq!(message(&e), "\"o_a\" is not an index for table \"t\"");
    let e = expect_code(c.batch_execute("ALTER TABLE def REPLICA IDENTITY USING INDEX def_a_key"), "0A000");
    assert_eq!(message(&e), "cannot use non-immediate index \"def_a_key\" as replica identity");
}

#[test]
fn cluster_on() {
    let (_db, mut c) = base();
    c.batch_execute("ALTER TABLE t CLUSTER ON t_b").expect("cluster on");
    assert_eq!(on(&flags(&mut c, "t", "indisclustered")), vec!["t_b"]);
    // Only one index is clustered at a time.
    c.batch_execute("ALTER TABLE t CLUSTER ON t_c").expect("cluster on another");
    assert_eq!(on(&flags(&mut c, "t", "indisclustered")), vec!["t_c"]);
    c.batch_execute("ALTER TABLE t SET WITHOUT CLUSTER").expect("without");
    assert!(on(&flags(&mut c, "t", "indisclustered")).is_empty());
    c.batch_execute("ALTER TABLE t SET WITHOUT CLUSTER").expect("without again");
    let e = expect_code(c.batch_execute("ALTER TABLE t CLUSTER ON nope"), "42704");
    assert_eq!(message(&e), "index \"nope\" for table \"t\" does not exist");
    let e = expect_code(c.batch_execute("ALTER TABLE t CLUSTER ON t_a_part"), "0A000");
    assert_eq!(message(&e), "cannot cluster on partial index \"t_a_part\"");
    let e = expect_code(c.batch_execute("ALTER TABLE t CLUSTER ON o_a"), "42809");
    assert_eq!(message(&e), "\"o_a\" is not an index for table \"t\"");
}

#[test]
fn set_logged_unlogged() {
    let (_db, mut c) = base();
    let rel = |c: &mut Client, t: &str| {
        text_value(c, &format!("SELECT relpersistence::text FROM pg_class WHERE oid = '{t}'::regclass")).expect("row")
    };
    c.batch_execute("ALTER TABLE t SET UNLOGGED").expect("unlogged");
    assert_eq!(rel(&mut c, "t"), "u");
    c.batch_execute("ALTER TABLE t SET UNLOGGED").expect("unlogged again");
    c.batch_execute("ALTER TABLE t SET LOGGED").expect("logged");
    assert_eq!(rel(&mut c, "t"), "p");
    c.batch_execute("CREATE UNLOGGED TABLE u (a int)").expect("create unlogged");
    assert_eq!(rel(&mut c, "u"), "u");
    c.batch_execute("CREATE TEMP TABLE tt (a int)").expect("temp");
    let e = expect_code(c.batch_execute("ALTER TABLE tt SET UNLOGGED"), "42P16");
    assert_eq!(message(&e), "cannot change logged status of table \"tt\" because it is temporary");
    // Foreign keys join tables of the same persistence only.
    c.batch_execute("CREATE TABLE p (a int PRIMARY KEY); CREATE TABLE r (a int REFERENCES p)").expect("fk");
    let e = expect_code(c.batch_execute("ALTER TABLE p SET UNLOGGED"), "42P16");
    assert_eq!(message(&e), "could not change table \"p\" to unlogged because it references logged table \"r\"");
    c.batch_execute("ALTER TABLE r SET UNLOGGED; ALTER TABLE p SET UNLOGGED").expect("both");
    let e = expect_code(c.batch_execute("ALTER TABLE r SET LOGGED"), "42P16");
    assert_eq!(message(&e), "could not change table \"r\" to logged because it references unlogged table \"p\"");
}

#[test]
fn several_actions_and_partitions() {
    let (_db, mut c) = base();
    c.batch_execute("ALTER TABLE t ALTER COLUMN a SET STATISTICS 100, ALTER COLUMN b SET STORAGE MAIN, REPLICA IDENTITY FULL")
        .expect("combined");
    assert_eq!(attr(&mut c, "t", "a", "attstattarget"), "100");
    assert_eq!(attr(&mut c, "t", "b", "attstorage"), "m");
    c.batch_execute(
        "CREATE TABLE pt (a int, b text) PARTITION BY RANGE (a);
         CREATE TABLE pp PARTITION OF pt FOR VALUES FROM (0) TO (10);
         ALTER TABLE pt ALTER COLUMN b SET STORAGE EXTERNAL, ALTER COLUMN b SET STATISTICS 77",
    )
    .expect("partitioned");
    assert_eq!(attr(&mut c, "pp", "b", "attstorage"), "e");
    assert_eq!(attr(&mut c, "pp", "b", "attstattarget"), "77");
    let e = expect_code(c.batch_execute("ALTER TABLE pt SET UNLOGGED"), "42809");
    assert_eq!(message(&e), "ALTER action SET UNLOGGED cannot be performed on relation \"pt\"");
}
