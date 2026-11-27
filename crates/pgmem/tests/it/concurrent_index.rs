//! `CONCURRENTLY` commands: refused inside a transaction block (`DROP INDEX`, `REINDEX`, as
//! `CREATE INDEX` already was), `DROP INDEX CONCURRENTLY`'s restrictions, and the invalid index
//! a failed `CREATE INDEX CONCURRENTLY` leaves behind (`indisvalid` / `indisready` false).

use crate::support::{RawConn, expect_code, message, setup, texts};
use postgres::Client;

fn rows(c: &mut Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

/// `indexname | indisvalid | indisready` of the indexes of `table`.
fn index_state(c: &mut Client, table: &str) -> Vec<Vec<String>> {
    rows(
        c,
        &format!(
            "SELECT indexrelid::regclass::text, indisvalid::text, indisready::text FROM pg_index \
             WHERE indrelid = '{table}'::regclass ORDER BY 1"
        ),
    )
}

fn index_names(c: &mut Client, table: &str) -> Vec<String> {
    rows(c, &format!("SELECT indexname FROM pg_indexes WHERE tablename = '{table}' ORDER BY 1")).into_iter().flatten().collect()
}

/// A table whose column `a` holds the duplicates a unique index cannot be built over.
fn duplicates() -> (crate::support::TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int); INSERT INTO t VALUES (1), (1)").expect("setup");
    (db, c)
}

#[test]
fn drop_index_concurrently_refused_in_a_transaction_block() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int, a int); CREATE INDEX i1 ON t (id)").expect("setup");
    c.batch_execute("BEGIN").expect("BEGIN");
    let e = expect_code(c.batch_execute("DROP INDEX CONCURRENTLY i1"), "25001");
    assert_eq!(message(&e), "DROP INDEX CONCURRENTLY cannot run inside a transaction block");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    // The same inside one simple Query, and in the implicit block of a multi-statement one.
    for sql in ["BEGIN; DROP INDEX CONCURRENTLY i1; COMMIT", "SELECT 1; DROP INDEX CONCURRENTLY i1", "DROP INDEX CONCURRENTLY i1; SELECT 1"]
    {
        let e = expect_code(c.batch_execute(sql), "25001");
        assert_eq!(message(&e), "DROP INDEX CONCURRENTLY cannot run inside a transaction block", "{sql}");
        c.batch_execute("ROLLBACK").expect("ROLLBACK");
        assert_eq!(index_names(&mut c, "t"), ["i1"], "{sql}");
    }
    // Outside a block it drops the index; a missing one is skipped with IF EXISTS.
    c.batch_execute("DROP INDEX CONCURRENTLY i1").expect("outside a block");
    c.batch_execute("DROP INDEX CONCURRENTLY IF EXISTS i1").expect("IF EXISTS");
    assert!(index_names(&mut c, "t").is_empty());
    let e = expect_code(c.batch_execute("DROP INDEX CONCURRENTLY i1"), "42704");
    assert_eq!(message(&e), "index \"i1\" does not exist");
}

#[test]
fn create_index_concurrently_refused_in_an_implicit_block() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int, a int)").expect("setup");
    for sql in ["SELECT 1; CREATE INDEX CONCURRENTLY i1 ON t (a)", "CREATE INDEX CONCURRENTLY i1 ON t (a); SELECT 1"] {
        let e = expect_code(c.batch_execute(sql), "25001");
        assert_eq!(message(&e), "CREATE INDEX CONCURRENTLY cannot run inside a transaction block", "{sql}");
        assert!(index_names(&mut c, "t").is_empty(), "{sql}");
    }
}

#[test]
fn drop_index_concurrently_takes_one_index_without_cascade() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int, a int); CREATE INDEX i1 ON t (id); CREATE INDEX i2 ON t (a)").expect("setup");
    let e = expect_code(c.batch_execute("DROP INDEX CONCURRENTLY i1, i2"), "0A000");
    assert_eq!(message(&e), "DROP INDEX CONCURRENTLY does not support dropping multiple objects");
    let e = expect_code(c.batch_execute("DROP INDEX CONCURRENTLY IF EXISTS nosuch, nosuch2"), "0A000");
    assert_eq!(message(&e), "DROP INDEX CONCURRENTLY does not support dropping multiple objects");
    let e = expect_code(c.batch_execute("DROP INDEX CONCURRENTLY i1 CASCADE"), "0A000");
    assert_eq!(message(&e), "DROP INDEX CONCURRENTLY does not support CASCADE");
    // Both restrictions are checked before the index is looked up; with both, "multiple" wins.
    expect_code(c.batch_execute("DROP INDEX CONCURRENTLY nosuch CASCADE"), "0A000");
    let e = expect_code(c.batch_execute("DROP INDEX CONCURRENTLY i1, i2 CASCADE"), "0A000");
    assert_eq!(message(&e), "DROP INDEX CONCURRENTLY does not support dropping multiple objects");
    assert_eq!(index_names(&mut c, "t"), ["i1", "i2"]);
    // RESTRICT is the default, and IF EXISTS of a missing index only notes it.
    c.batch_execute("DROP INDEX CONCURRENTLY IF EXISTS nosuch").expect("IF EXISTS");
    c.batch_execute("DROP INDEX CONCURRENTLY i1 RESTRICT").expect("one index");
    assert_eq!(index_names(&mut c, "t"), ["i2"]);
    // The transaction-block error comes first.
    c.batch_execute("BEGIN").expect("BEGIN");
    let e = expect_code(c.batch_execute("DROP INDEX CONCURRENTLY i2 CASCADE"), "25001");
    assert_eq!(message(&e), "DROP INDEX CONCURRENTLY cannot run inside a transaction block");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    // Without CONCURRENTLY several indexes and CASCADE are fine.
    c.batch_execute("CREATE INDEX i1 ON t (id); DROP INDEX i1, i2 CASCADE").expect("plain DROP INDEX");
}

#[test]
fn reindex_concurrently_and_friends_in_a_transaction_block() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int, a int); CREATE INDEX i1 ON t (id)").expect("setup");
    for sql in [
        "REINDEX INDEX CONCURRENTLY i1",
        "REINDEX TABLE CONCURRENTLY t",
        "REINDEX SCHEMA CONCURRENTLY public",
        "REINDEX (CONCURRENTLY) INDEX i1",
        "REINDEX (VERBOSE, CONCURRENTLY) TABLE t",
    ] {
        c.batch_execute("BEGIN").expect("BEGIN");
        let e = expect_code(c.batch_execute(sql), "25001");
        assert_eq!(message(&e), "REINDEX CONCURRENTLY cannot run inside a transaction block", "{sql}");
        c.batch_execute("ROLLBACK").expect("ROLLBACK");
        let e = expect_code(c.batch_execute(&format!("SELECT 1; {sql}")), "25001");
        assert_eq!(message(&e), "REINDEX CONCURRENTLY cannot run inside a transaction block", "{sql}");
    }
    for (sql, what) in [("REINDEX SCHEMA public", "SCHEMA"), ("REINDEX DATABASE postgres", "DATABASE")] {
        c.batch_execute("BEGIN").expect("BEGIN");
        let e = expect_code(c.batch_execute(sql), "25001");
        assert_eq!(message(&e), format!("REINDEX {what} cannot run inside a transaction block"));
        c.batch_execute("ROLLBACK").expect("ROLLBACK");
    }
    // Plain REINDEX of an index or a table, and `(CONCURRENTLY false)`, run in a block.
    c.batch_execute("BEGIN; REINDEX INDEX i1; REINDEX TABLE t; REINDEX (CONCURRENTLY false) INDEX i1; COMMIT").expect("plain REINDEX");
    expect_code(c.batch_execute("REINDEX INDEX CONCURRENTLY i1; REINDEX (CONCURRENTLY) TABLE t"), "25001");
    c.batch_execute("REINDEX INDEX CONCURRENTLY i1").expect("outside a block");
    c.batch_execute("REINDEX (CONCURRENTLY) TABLE t").expect("option form outside a block");
}

#[test]
fn refresh_materialized_view_concurrently_runs_in_a_transaction_block() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE MATERIALIZED VIEW mv AS SELECT 1 AS x; CREATE UNIQUE INDEX mv_x ON mv (x)").expect("setup");
    c.batch_execute("BEGIN; REFRESH MATERIALIZED VIEW CONCURRENTLY mv; COMMIT").expect("REFRESH ... CONCURRENTLY in a block");
    c.batch_execute("SELECT 1; REFRESH MATERIALIZED VIEW CONCURRENTLY mv").expect("REFRESH ... CONCURRENTLY in an implicit block");
}

#[test]
fn reindex_names_the_right_kind_of_relation() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int); CREATE INDEX t_i ON t (a); CREATE VIEW v AS SELECT 1").expect("setup");
    let e = expect_code(c.batch_execute("REINDEX INDEX t"), "42809");
    assert_eq!(message(&e), "\"t\" is not an index");
    let e = expect_code(c.batch_execute("REINDEX TABLE t_i"), "42809");
    assert_eq!(message(&e), "\"t_i\" is not a table or materialized view");
    expect_code(c.batch_execute("REINDEX TABLE v"), "42809");
    let e = expect_code(c.batch_execute("REINDEX INDEX nosuch"), "42P01");
    assert_eq!(message(&e), "relation \"nosuch\" does not exist");
    expect_code(c.batch_execute("REINDEX TABLE nosuch"), "42P01");
    let e = expect_code(c.batch_execute("REINDEX SCHEMA nosuch"), "3F000");
    assert_eq!(message(&e), "schema \"nosuch\" does not exist");
}

#[test]
fn failed_unique_build_leaves_an_invalid_index() {
    let (_db, mut c) = duplicates();
    let e = expect_code(c.batch_execute("CREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON t (a)"), "23505");
    assert_eq!(message(&e), "could not create unique index \"t_a_idx\"");
    // Postgres' build had not finished: the index is neither valid nor ready.
    assert_eq!(index_state(&mut c, "t"), [["t_a_idx", "false", "false"]]);
    assert_eq!(rows(&mut c, "SELECT relkind::text FROM pg_class WHERE relname = 't_a_idx'"), [["i"]]);
    // Its definition is unaffected.
    let def = "CREATE UNIQUE INDEX t_a_idx ON public.t USING btree (a)";
    assert_eq!(rows(&mut c, "SELECT pg_get_indexdef('t_a_idx'::regclass)"), [[def]]);
    assert_eq!(rows(&mut c, "SELECT indexdef FROM pg_indexes WHERE indexname = 't_a_idx'"), [[def]]);
    // The name is taken.
    let e = expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_a_idx ON t (a)"), "42P07");
    assert_eq!(message(&e), "relation \"t_a_idx\" already exists");
    expect_code(c.batch_execute("CREATE INDEX t_a_idx ON t (a)"), "42P07");
    c.batch_execute("CREATE INDEX CONCURRENTLY IF NOT EXISTS t_a_idx ON t (a)").expect("IF NOT EXISTS");
    assert_eq!(index_state(&mut c, "t"), [["t_a_idx", "false", "false"]]);
    // The table is untouched.
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM t"), [["2"]]);
}

#[test]
fn invalid_index_does_not_enforce_or_serve() {
    let (_db, mut c) = duplicates();
    expect_code(c.batch_execute("CREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON t (a)"), "23505");
    // Not maintained, so not enforced.
    c.batch_execute("INSERT INTO t VALUES (1)").expect("duplicate accepted");
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM t"), [["3"]]);
    // Not an arbiter for ON CONFLICT, whose bare form has nothing to conflict with.
    let e = expect_code(c.batch_execute("INSERT INTO t VALUES (1) ON CONFLICT (a) DO NOTHING"), "42P10");
    assert_eq!(message(&e), "there is no unique or exclusion constraint matching the ON CONFLICT specification");
    c.batch_execute("INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING").expect("bare ON CONFLICT");
    // Not usable for a foreign key, a constraint or CLUSTER.
    c.batch_execute("CREATE TABLE r (x int)").expect("r");
    let e = expect_code(c.batch_execute("ALTER TABLE r ADD FOREIGN KEY (x) REFERENCES t (a)"), "42830");
    assert_eq!(message(&e), "there is no unique constraint matching given keys for referenced table \"t\"");
    let e = expect_code(c.batch_execute("ALTER TABLE t ADD CONSTRAINT t_u UNIQUE USING INDEX t_a_idx"), "55000");
    assert_eq!(message(&e), "index \"t_a_idx\" is not valid");
    expect_code(c.batch_execute("ALTER TABLE t ADD PRIMARY KEY USING INDEX t_a_idx"), "55000");
    let e = expect_code(c.batch_execute("ALTER TABLE t CLUSTER ON t_a_idx"), "0A000");
    assert_eq!(message(&e), "cannot cluster on invalid index \"t_a_idx\"");
    // It can still be renamed.
    c.batch_execute("ALTER INDEX t_a_idx RENAME TO t_a_old").expect("rename");
    assert_eq!(index_state(&mut c, "t"), [["t_a_old", "false", "false"]]);
}

#[test]
fn reindex_makes_an_invalid_index_valid_once_it_builds() {
    for sql in ["REINDEX INDEX t_a_idx", "REINDEX TABLE t", "REINDEX INDEX CONCURRENTLY t_a_idx", "REINDEX SCHEMA public"] {
        let (_db, mut c) = duplicates();
        expect_code(c.batch_execute("CREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON t (a)"), "23505");
        // The duplicates are still there: the rebuild fails the same way, the index stays invalid.
        let e = expect_code(c.batch_execute(sql), "23505");
        assert!(message(&e).starts_with("could not create unique index \"t_a_idx"), "{sql}: {}", message(&e));
        assert!(index_state(&mut c, "t").iter().all(|r| r[1] == "false" && r[2] == "false"), "{sql}");
        c.batch_execute("TRUNCATE t; INSERT INTO t VALUES (1), (2)").expect("fix the data");
        c.batch_execute("DROP INDEX IF EXISTS t_a_idx_ccnew").expect("clean up");
        c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(index_state(&mut c, "t"), [["t_a_idx", "true", "true"]], "{sql}");
        // Valid again: maintained and enforced.
        let e = expect_code(c.batch_execute("INSERT INTO t VALUES (1)"), "23505");
        assert_eq!(message(&e), "duplicate key value violates unique constraint \"t_a_idx\"");
        c.batch_execute("ALTER TABLE t ADD CONSTRAINT t_u UNIQUE USING INDEX t_a_idx").expect("usable again");
    }
}

#[test]
fn reindex_concurrently_of_a_table_skips_invalid_indexes() {
    let (db, mut c) = duplicates();
    c.batch_execute("CREATE INDEX t_ok ON t (a)").expect("valid index");
    expect_code(c.batch_execute("CREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON t (a)"), "23505");
    let mut raw = RawConn::connect(&db, &mut c);
    let r = raw.query("REINDEX TABLE CONCURRENTLY t");
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.notices, ["skipping reindex of invalid index \"public.t_a_idx\""]);
    assert_eq!(r.tags, ["REINDEX"]);
    assert_eq!(index_state(&mut c, "t"), [["t_a_idx", "false", "false"], ["t_ok", "true", "true"]]);
    // Nothing left to reindex concurrently once the valid one is dropped; plainly there is.
    c.batch_execute("DROP INDEX t_ok").expect("DROP INDEX");
    let r = raw.query("REINDEX TABLE CONCURRENTLY t");
    assert_eq!(
        r.notices,
        ["skipping reindex of invalid index \"public.t_a_idx\"", "table \"t\" has no indexes that can be reindexed concurrently"]
    );
    // A table without indexes says so too.
    c.batch_execute("CREATE TABLE e (a int)").expect("e");
    assert_eq!(raw.query("REINDEX TABLE e").notices, ["table \"e\" has no indexes to reindex"]);
    assert_eq!(raw.query("REINDEX TABLE CONCURRENTLY e").notices, ["table \"e\" has no indexes that can be reindexed concurrently"]);
}

#[test]
fn failed_concurrent_reindex_leaves_its_copy_invalid() {
    let (_db, mut c) = duplicates();
    expect_code(c.batch_execute("CREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON t (a)"), "23505");
    let e = expect_code(c.batch_execute("REINDEX INDEX CONCURRENTLY t_a_idx"), "23505");
    assert_eq!(message(&e), "could not create unique index \"t_a_idx_ccnew\"");
    assert_eq!(index_state(&mut c, "t"), [["t_a_idx", "false", "false"], ["t_a_idx_ccnew", "false", "false"]]);
    // A second attempt does not reuse the name.
    let e = expect_code(c.batch_execute("REINDEX INDEX CONCURRENTLY t_a_idx"), "23505");
    assert_eq!(message(&e), "could not create unique index \"t_a_idx_ccnew1\"");
    c.batch_execute("DROP INDEX CONCURRENTLY t_a_idx_ccnew").expect("drop a copy");
    c.batch_execute("DROP INDEX CONCURRENTLY t_a_idx_ccnew1").expect("drop the other copy");
    assert_eq!(index_state(&mut c, "t"), [["t_a_idx", "false", "false"]]);
}

#[test]
fn drop_removes_an_invalid_index() {
    for drop in ["DROP INDEX t_a_idx", "DROP INDEX CONCURRENTLY t_a_idx", "DROP TABLE t"] {
        let (_db, mut c) = duplicates();
        expect_code(c.batch_execute("CREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON t (a)"), "23505");
        c.batch_execute(drop).unwrap_or_else(|e| panic!("{drop}: {e}"));
        assert!(rows(&mut c, "SELECT 1 FROM pg_class WHERE relname = 't_a_idx'").is_empty(), "{drop}");
        c.batch_execute("CREATE TABLE IF NOT EXISTS t (a int); TRUNCATE t").expect("empty table");
        c.batch_execute("CREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON t (a)").unwrap_or_else(|e| panic!("{drop}: {e}"));
        assert_eq!(index_state(&mut c, "t"), [["t_a_idx", "true", "true"]]);
    }
}

#[test]
fn error_in_an_index_expression_leaves_an_invalid_index() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int); INSERT INTO t VALUES (0), (1)").expect("setup");
    let e = expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_expr ON t ((1 / a))"), "22012");
    assert_eq!(message(&e), "division by zero");
    expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_pred ON t (a) WHERE 1 / a > 0"), "22012");
    assert_eq!(index_state(&mut c, "t"), [["t_expr", "false", "false"], ["t_pred", "false", "false"]]);
    // Without CONCURRENTLY the failed build leaves nothing, and it fails the same way.
    expect_code(c.batch_execute("CREATE INDEX t_plain ON t ((1 / a))"), "22012");
    expect_code(c.batch_execute("CREATE INDEX t_plain_pred ON t (a) WHERE 1 / a > 0"), "22012");
    assert_eq!(index_state(&mut c, "t").len(), 2);
    // Errors found before the build leave nothing behind.
    expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_missing ON t (nope)"), "42703");
    expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_func ON t (nofunc(a))"), "42883");
    expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_am ON t USING nope (a)"), "42704");
    expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_where ON t (a) WHERE nope"), "42703");
    expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_syntax ON t (a"), "42601");
    assert_eq!(index_state(&mut c, "t").len(), 2);
    // An expression that evaluates is built as before.
    c.batch_execute("CREATE INDEX CONCURRENTLY t_ok ON t ((a + 1))").expect("an expression that evaluates");
    // TRUNCATE, not DELETE: Postgres' build also evaluates deleted rows some snapshot may still see.
    c.batch_execute("TRUNCATE t; INSERT INTO t VALUES (1)").expect("replace the bad row");
    c.batch_execute("REINDEX TABLE t").expect("repair");
    assert_eq!(index_state(&mut c, "t"), [["t_expr", "true", "true"], ["t_ok", "true", "true"], ["t_pred", "true", "true"]]);
}

#[test]
fn create_index_concurrently_on_a_partitioned_table_is_refused() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE p (a int) PARTITION BY RANGE (a); CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10)")
        .expect("setup");
    for sql in ["CREATE INDEX CONCURRENTLY pi ON p (a)", "CREATE INDEX CONCURRENTLY pi ON ONLY p (a)"] {
        let e = expect_code(c.batch_execute(sql), "0A000");
        assert_eq!(message(&e), "cannot create index on partitioned table \"p\" concurrently", "{sql}");
    }
    c.batch_execute("CREATE INDEX CONCURRENTLY p1i ON p1 (a)").expect("a partition on its own");
}

#[test]
fn failed_build_in_an_extended_protocol_pipeline_keeps_the_invalid_index() {
    let (db, mut c) = duplicates();
    let mut raw = RawConn::connect(&db, &mut c);
    let r = raw.pipeline(&["CREATE UNIQUE INDEX CONCURRENTLY t_a_idx ON t (a)"]);
    assert_eq!(r.errors, ["23505"]);
    assert_eq!(r.status, b'I');
    assert_eq!(index_state(&mut c, "t"), [["t_a_idx", "false", "false"]]);
    let r = raw.pipeline(&["DROP INDEX CONCURRENTLY t_a_idx"]);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(index_state(&mut c, "t").is_empty());
}
