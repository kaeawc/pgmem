//! Declarative partitioning: routing, partition constraints, row movement, sub-partitions,
//! ON CONFLICT, foreign keys to partitioned tables, COPY, ATTACH / DETACH and the catalogs.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn schema(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE ev (id int NOT NULL, region text NOT NULL, at date NOT NULL, v int DEFAULT 7, PRIMARY KEY (id, region, at))
           PARTITION BY LIST (region);
         CREATE TABLE ev_eu PARTITION OF ev FOR VALUES IN ('eu') PARTITION BY RANGE (at);
         CREATE TABLE ev_eu_24 PARTITION OF ev_eu FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
         CREATE TABLE ev_eu_25 PARTITION OF ev_eu FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');
         CREATE TABLE ev_us PARTITION OF ev FOR VALUES IN ('us')",
    )
    .expect("schema");
}

#[test]
fn routing_and_constraints() {
    let (_db, mut c) = setup();
    schema(&mut c);
    assert_eq!(
        rows(
            &mut c,
            "INSERT INTO ev (id, region, at) VALUES (1, 'eu', '2024-03-01'), (2, 'us', '2024-03-01'), (3, 'eu', '2025-03-01') RETURNING tableoid::regclass::text, id::text, v::text"
        ),
        [["ev_eu_24", "1", "7"], ["ev_us", "2", "7"], ["ev_eu_25", "3", "7"]]
    );
    let e = expect_code(c.batch_execute("INSERT INTO ev VALUES (4, 'eu', '2027-03-01')"), "23514");
    assert_eq!(message(&e), "no partition of relation \"ev_eu\" found for row");
    let e = expect_code(c.batch_execute("INSERT INTO ev VALUES (4, 'mars', '2024-03-01')"), "23514");
    assert_eq!(message(&e), "no partition of relation \"ev\" found for row");
    let e = expect_code(c.batch_execute("INSERT INTO ev_us VALUES (4, 'eu', '2024-03-01')"), "23514");
    assert_eq!(message(&e), "new row for relation \"ev_us\" violates partition constraint");
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM ev"), [["3"]]);
    assert_eq!(rows(&mut c, "SELECT string_agg(id::text, ',' ORDER BY id) FROM ev_eu"), [["1,3"]]);
    // Moving rows between partitions, through the partitioned table.
    assert_eq!(rows(&mut c, "UPDATE ev SET region = 'us' WHERE id = 3 RETURNING tableoid::regclass::text, id::text"), [["ev_us", "3"]]);
    assert_eq!(rows(&mut c, "UPDATE ev SET at = '2025-06-01' WHERE id = 1 RETURNING tableoid::regclass::text"), [["ev_eu_25"]]);
    let e = expect_code(c.batch_execute("UPDATE ev_us SET region = 'eu'"), "23514");
    assert_eq!(message(&e), "new row for relation \"ev_us\" violates partition constraint");
    // Unique keys are per partition and named after it.
    let e = expect_code(c.batch_execute("INSERT INTO ev VALUES (2, 'us', '2024-03-01')"), "23505");
    assert_eq!(message(&e), "duplicate key value violates unique constraint \"ev_us_pkey\"");
    assert_eq!(
        rows(
            &mut c,
            "INSERT INTO ev VALUES (2, 'us', '2024-03-01', 99) ON CONFLICT (id, region, at) DO UPDATE SET v = EXCLUDED.v RETURNING v::text"
        ),
        [["99"]]
    );
    let mut deleted = rows(&mut c, "DELETE FROM ev WHERE region = 'us' RETURNING tableoid::regclass::text, id::text");
    deleted.sort();
    assert_eq!(deleted, [["ev_us", "2"], ["ev_us", "3"]]);
    let e = expect_code(c.batch_execute("ALTER TABLE ev ADD UNIQUE (v)"), "0A000");
    assert_eq!(message(&e), "unique constraint on partitioned table must include all partitioning columns");
}

#[test]
fn catalogs_and_ddl() {
    let (_db, mut c) = setup();
    schema(&mut c);
    assert_eq!(
        rows(
            &mut c,
            "SELECT relname::text, relkind::text, relispartition::text, coalesce(pg_get_expr(relpartbound, oid), '-') FROM pg_class WHERE relname LIKE 'ev%' AND relkind IN ('r', 'p') ORDER BY 1"
        ),
        [
            ["ev", "p", "false", "-"],
            ["ev_eu", "p", "true", "FOR VALUES IN ('eu')"],
            ["ev_eu_24", "r", "true", "FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')"],
            ["ev_eu_25", "r", "true", "FOR VALUES FROM ('2025-01-01') TO ('2026-01-01')"],
            ["ev_us", "r", "true", "FOR VALUES IN ('us')"],
        ]
    );
    assert_eq!(
        rows(&mut c, "SELECT pg_get_partkeydef('ev'::regclass), pg_get_partkeydef('ev_eu'::regclass)"),
        [["LIST (region)", "RANGE (at)"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT relid::text, coalesce(parentrelid::text, '-'), isleaf::text, level::text FROM pg_partition_tree('ev') ORDER BY level, relid::text"
        ),
        [
            ["ev", "-", "false", "0"],
            ["ev_eu", "ev", "false", "1"],
            ["ev_us", "ev", "true", "1"],
            ["ev_eu_24", "ev_eu", "true", "2"],
            ["ev_eu_25", "ev_eu", "true", "2"]
        ]
    );
    assert_eq!(
        rows(&mut c, "SELECT pg_partition_root('ev_eu_24')::text, pg_partition_ancestors('ev_eu_24')::text"),
        [["ev", "ev_eu_24"], ["ev", "ev_eu"], ["ev", "ev"]]
    );
    let e = expect_code(c.batch_execute("CREATE TABLE ev_x PARTITION OF ev FOR VALUES IN ('us', 'ca')"), "42P17");
    assert_eq!(message(&e), "partition \"ev_x\" would overlap partition \"ev_us\"");
    let e = expect_code(c.batch_execute("CREATE TABLE ev_x PARTITION OF ev_eu FOR VALUES IN ('x')"), "42P16");
    assert_eq!(message(&e), "invalid bound specification for a range partition");
    // New columns and indexes reach the partitions.
    c.batch_execute("ALTER TABLE ev ADD COLUMN note text DEFAULT 'n'; CREATE INDEX ev_v_idx ON ev (v)").expect("alter");
    c.batch_execute("INSERT INTO ev (id, region, at) VALUES (5, 'eu', '2024-04-04')").expect("insert");
    assert_eq!(rows(&mut c, "SELECT note FROM ev_eu_24"), [["n"]]);
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM pg_indexes WHERE tablename = 'ev_us' AND indexdef LIKE '%(v)'"), [["1"]]);
    // DETACH makes a standalone table; ATTACH checks columns and existing rows.
    c.batch_execute("ALTER TABLE ev DETACH PARTITION ev_us").expect("detach");
    assert_eq!(rows(&mut c, "SELECT relispartition::text FROM pg_class WHERE relname = 'ev_us'"), [["false"]]);
    c.batch_execute("INSERT INTO ev_us VALUES (6, 'ca', '2024-01-01')").expect("standalone insert");
    let e = expect_code(c.batch_execute("ALTER TABLE ev ATTACH PARTITION ev_us FOR VALUES IN ('us')"), "23514");
    assert_eq!(message(&e), "partition constraint of relation \"ev_us\" is violated by some row");
    c.batch_execute("DELETE FROM ev_us; ALTER TABLE ev ATTACH PARTITION ev_us FOR VALUES IN ('us')").expect("attach");
    c.batch_execute("TRUNCATE ev").expect("truncate");
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM ev"), [["0"]]);
    c.batch_execute("DROP TABLE ev").expect("drop");
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM pg_class WHERE relname LIKE 'ev%'"), [["0"]]);
}

#[test]
fn hash_partitions_foreign_keys_and_copy() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE acct (id int PRIMARY KEY) PARTITION BY HASH (id);
         CREATE TABLE acct0 PARTITION OF acct FOR VALUES WITH (MODULUS 4, REMAINDER 0);
         CREATE TABLE acct1 PARTITION OF acct FOR VALUES WITH (MODULUS 4, REMAINDER 1);
         CREATE TABLE acct2 PARTITION OF acct FOR VALUES WITH (MODULUS 4, REMAINDER 2);
         CREATE TABLE acct3 PARTITION OF acct FOR VALUES WITH (MODULUS 4, REMAINDER 3);
         INSERT INTO acct SELECT generate_series(1, 12);
         CREATE TABLE txn (acct int REFERENCES acct, amount int)",
    )
    .expect("setup");
    assert_eq!(
        rows(&mut c, "SELECT tableoid::regclass::text, string_agg(id::text, ',' ORDER BY id) FROM acct GROUP BY 1 ORDER BY 1"),
        [["acct0", "1,12"], ["acct1", "3,5,8,9,11"], ["acct2", "2"], ["acct3", "4,6,7,10"]]
    );
    c.batch_execute("INSERT INTO txn VALUES (5, 10)").expect("fk to a partitioned table");
    let e = expect_code(c.batch_execute("INSERT INTO txn VALUES (50, 10)"), "23503");
    assert_eq!(message(&e), "insert or update on table \"txn\" violates foreign key constraint \"txn_acct_fkey\"");
    expect_code(c.batch_execute("DELETE FROM acct WHERE id = 5"), "23503");
    let mut w = c.copy_in("COPY acct FROM STDIN").expect("copy in");
    std::io::Write::write_all(&mut w, b"13\n14\n").expect("write");
    assert_eq!(w.finish().expect("finish"), 2);
    assert_eq!(rows(&mut c, "SELECT tableoid::regclass::text FROM acct WHERE id >= 13 ORDER BY id"), [["acct2"], ["acct0"]]);
    let e = expect_code(c.batch_execute("COPY acct TO STDOUT"), "42809");
    assert_eq!(message(&e), "cannot copy from partitioned table \"acct\"");
}

/// Each index on the tables of `like`: name, relkind, relispartition, indisvalid, parent index.
fn index_state(c: &mut postgres::Client, like: &str) -> Vec<Vec<String>> {
    rows(
        c,
        &format!(
            "SELECT c.relname::text, c.relkind::text, c.relispartition::text, i.indisvalid::text,
                    coalesce((SELECT inhparent::regclass::text FROM pg_inherits WHERE inhrelid = c.oid), '-')
               FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
              WHERE c.relnamespace = 'public'::regnamespace AND c.relname LIKE '{like}' ORDER BY 1"
        ),
    )
}

fn detail(e: &postgres::Error) -> Option<String> {
    e.as_db_error().and_then(|d| d.detail()).map(str::to_owned)
}

#[test]
fn create_index_on_only_and_attach_partition() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE m (a int, b int) PARTITION BY RANGE (a);
         CREATE TABLE m1 PARTITION OF m FOR VALUES FROM (0) TO (10);
         CREATE TABLE m2 PARTITION OF m FOR VALUES FROM (10) TO (20);
         CREATE INDEX mi ON ONLY m (b)",
    )
    .expect("on only");
    assert_eq!(index_state(&mut c, "m%"), [["mi", "I", "false", "false", "-"]]);
    assert_eq!(rows(&mut c, "SELECT pg_get_indexdef('mi'::regclass)"), [["CREATE INDEX mi ON ONLY public.m USING btree (b)"]]);
    c.batch_execute("CREATE INDEX m1i ON m1 (b); ALTER INDEX mi ATTACH PARTITION m1i").expect("attach");
    assert_eq!(index_state(&mut c, "m%"), [["m1i", "i", "true", "true", "mi"], ["mi", "I", "false", "false", "-"]]);
    // Attaching again to the same parent is a no-op.
    c.batch_execute("ALTER INDEX mi ATTACH PARTITION m1i").expect("reattach");
    let e = {
        c.batch_execute("CREATE INDEX m2a ON m2 (a)").expect("m2a");
        expect_code(c.batch_execute("ALTER INDEX mi ATTACH PARTITION m2a"), "42P17")
    };
    assert_eq!(message(&e), "cannot attach index \"m2a\" as a partition of index \"mi\"");
    assert_eq!(detail(&e).as_deref(), Some("The index definitions do not match."));
    let e = {
        c.batch_execute("CREATE INDEX m1j ON m1 (b)").expect("m1j");
        expect_code(c.batch_execute("ALTER INDEX mi ATTACH PARTITION m1j"), "55000")
    };
    assert_eq!(message(&e), "cannot attach index \"m1j\" as a partition of index \"mi\"");
    assert_eq!(detail(&e).as_deref(), Some("Another index is already attached for partition \"m1\"."));
    let e = expect_code(c.batch_execute("ALTER INDEX mi ATTACH PARTITION mi"), "55000");
    assert_eq!(detail(&e).as_deref(), Some("Index \"mi\" is not an index on any partition of table \"m\"."));
    let e = expect_code(c.batch_execute("ALTER INDEX m ATTACH PARTITION m1i"), "42809");
    assert_eq!(message(&e), "\"m\" is not an index");
    let e = expect_code(c.batch_execute("ALTER INDEX m1i ATTACH PARTITION m1i"), "42809");
    assert_eq!(message(&e), "ALTER action ATTACH PARTITION cannot be performed on relation \"m1i\"");
    let e = expect_code(c.batch_execute("ALTER INDEX mi ATTACH PARTITION m1"), "42P17");
    assert_eq!(message(&e), "\"m1\" is not an index");
    expect_code(c.batch_execute("ALTER INDEX mi ATTACH PARTITION nope"), "42P01");
    let e = expect_code(c.batch_execute("DROP INDEX m1i"), "2BP01");
    assert_eq!(message(&e), "cannot drop index m1i because index mi requires it");
    // The last partition's index makes the parent valid.
    c.batch_execute("CREATE INDEX m2i ON m2 (b); ALTER INDEX mi ATTACH PARTITION m2i").expect("attach m2i");
    assert_eq!(rows(&mut c, "SELECT indisvalid::text FROM pg_index WHERE indexrelid = 'mi'::regclass"), [["true"]]);
    assert_eq!(
        rows(&mut c, "SELECT relid::text, coalesce(parentrelid::text, '-'), isleaf::text, level::text FROM pg_partition_tree('mi')"),
        [["mi", "-", "false", "0"], ["m1i", "mi", "true", "1"], ["m2i", "mi", "true", "1"]]
    );
    assert_eq!(rows(&mut c, "SELECT relid::text FROM pg_partition_ancestors('m1i')"), [["m1i"], ["mi"]]);
    assert_eq!(rows(&mut c, "SELECT pg_partition_root('m2i')::text"), [["mi"]]);
    // Dropping the parent drops the attached indexes, not the others.
    c.batch_execute("DROP INDEX mi").expect("drop parent");
    assert_eq!(
        rows(&mut c, "SELECT relname::text FROM pg_class WHERE relname LIKE 'm%' AND relkind IN ('i', 'I') ORDER BY 1"),
        [["m1j"], ["m2a"]]
    );
    // ON ONLY on a partitioned table without partitions is valid; on a plain table it is a plain index.
    c.batch_execute("CREATE TABLE e (a int) PARTITION BY LIST (a); CREATE INDEX ei ON ONLY e (a); CREATE TABLE t (a int); CREATE INDEX ti ON ONLY t (a)")
        .expect("empty");
    assert_eq!(index_state(&mut c, "%i"), [["ei", "I", "false", "true", "-"], ["ti", "i", "false", "true", "-"]]);
}

#[test]
fn partitioned_index_hierarchies() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int, b int) PARTITION BY RANGE (a);
         CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10);
         CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (10) TO (20) PARTITION BY LIST (b);
         CREATE TABLE p21 PARTITION OF p2 FOR VALUES IN (1);
         CREATE TABLE p22 PARTITION OF p2 FOR VALUES IN (2);
         CREATE INDEX mine ON p1 (b);
         CREATE INDEX mine2 ON p1 (b);
         CREATE INDEX ON p (b)",
    )
    .expect("schema");
    // A regular CREATE INDEX adopts one matching index per partition and creates the rest.
    assert_eq!(
        index_state(&mut c, "%"),
        [
            ["mine", "i", "true", "true", "p_b_idx"],
            ["mine2", "i", "false", "true", "-"],
            ["p21_b_idx", "i", "true", "true", "p2_b_idx"],
            ["p22_b_idx", "i", "true", "true", "p2_b_idx"],
            ["p2_b_idx", "I", "true", "true", "p_b_idx"],
            ["p_b_idx", "I", "false", "true", "-"],
        ]
    );
    // Validity propagates up a multi-level hierarchy once every level is complete.
    c.batch_execute(
        "CREATE INDEX q ON ONLY p (a);
         CREATE INDEX q2 ON ONLY p2 (a);
         ALTER INDEX q ATTACH PARTITION q2;
         CREATE INDEX q1 ON p1 (a);
         ALTER INDEX q ATTACH PARTITION q1;
         CREATE INDEX q21 ON p21 (a);
         ALTER INDEX q2 ATTACH PARTITION q21",
    )
    .expect("levels");
    let e = {
        c.batch_execute("CREATE INDEX r0 ON ONLY p (a)").expect("r0");
        expect_code(c.batch_execute("ALTER INDEX r0 ATTACH PARTITION q2"), "55000")
    };
    assert_eq!(detail(&e).as_deref(), Some("Index \"q2\" is already attached to another index."));
    c.batch_execute("DROP INDEX r0").expect("drop r0");
    assert_eq!(
        index_state(&mut c, "q%"),
        [
            ["q", "I", "false", "false", "-"],
            ["q1", "i", "true", "true", "q"],
            ["q2", "I", "true", "false", "q"],
            ["q21", "i", "true", "true", "q2"],
        ]
    );
    c.batch_execute("CREATE INDEX q22 ON p22 (a); ALTER INDEX q2 ATTACH PARTITION q22").expect("q22");
    assert_eq!(
        rows(&mut c, "SELECT indisvalid::text FROM pg_index WHERE indexrelid IN ('q'::regclass, 'q2'::regclass)"),
        [["true"], ["true"]]
    );
    // ATTACH PARTITION adopts a matching index and creates the missing ones; DETACH unlinks them.
    c.batch_execute(
        "CREATE TABLE p3 (a int, b int); CREATE INDEX p3b ON p3 (b); ALTER TABLE p ATTACH PARTITION p3 FOR VALUES FROM (20) TO (30)",
    )
    .expect("attach");
    assert_eq!(index_state(&mut c, "p3%"), [["p3_a_idx", "i", "true", "true", "q"], ["p3b", "i", "true", "true", "p_b_idx"]]);
    c.batch_execute("ALTER TABLE p DETACH PARTITION p3").expect("detach");
    assert_eq!(index_state(&mut c, "p3%"), [["p3_a_idx", "i", "false", "true", "-"], ["p3b", "i", "false", "true", "-"]]);
    c.batch_execute("DROP INDEX p3b").expect("detached index drops");
    // A new partition gets an attached copy of an invalid parent index, which stays invalid.
    c.batch_execute("CREATE INDEX r ON ONLY p (a, b); CREATE TABLE p4 PARTITION OF p FOR VALUES FROM (40) TO (50)").expect("p4");
    assert_eq!(index_state(&mut c, "p4_a_b_idx"), [["p4_a_b_idx", "i", "true", "true", "r"]]);
    assert_eq!(rows(&mut c, "SELECT indisvalid::text FROM pg_index WHERE indexrelid = 'r'::regclass"), [["false"]]);
    // Two identical parent indexes get one partition index each.
    c.batch_execute("CREATE TABLE x (a int) PARTITION BY LIST (a); CREATE TABLE x1 PARTITION OF x FOR VALUES IN (1); CREATE INDEX ON x (a); CREATE INDEX ON x (a)")
        .expect("dupes");
    assert_eq!(
        index_state(&mut c, "x%"),
        [
            ["x1_a_idx", "i", "true", "true", "x_a_idx"],
            ["x1_a_idx1", "i", "true", "true", "x_a_idx1"],
            ["x_a_idx", "I", "false", "true", "-"],
            ["x_a_idx1", "I", "false", "true", "-"],
        ]
    );
}

#[test]
fn partitioned_key_indexes_are_attached() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE u (a int NOT NULL, b int NOT NULL, PRIMARY KEY (a)) PARTITION BY RANGE (a);
         CREATE TABLE u1 PARTITION OF u FOR VALUES FROM (0) TO (10);
         ALTER TABLE u ADD CONSTRAINT u_ab UNIQUE (a, b);
         CREATE TABLE u2 (a int NOT NULL, b int NOT NULL);
         ALTER TABLE u ATTACH PARTITION u2 FOR VALUES FROM (10) TO (20)",
    )
    .expect("schema");
    assert_eq!(
        index_state(&mut c, "u%"),
        [
            ["u1_a_b_key", "i", "true", "true", "u_ab"],
            ["u1_pkey", "i", "true", "true", "u_pkey"],
            ["u2_a_b_key", "i", "true", "true", "u_ab"],
            ["u2_pkey", "i", "true", "true", "u_pkey"],
            ["u_ab", "I", "false", "true", "-"],
            ["u_pkey", "I", "false", "true", "-"],
        ]
    );
    let e = expect_code(c.batch_execute("CREATE UNIQUE INDEX u_a ON ONLY u (a); ALTER INDEX u_a ATTACH PARTITION u1_pkey"), "55000");
    assert_eq!(detail(&e).as_deref(), Some("Index \"u1_pkey\" is already attached to another index."));
    let e = expect_code(c.batch_execute("DROP INDEX u1_pkey"), "2BP01");
    assert_eq!(message(&e), "cannot drop index u1_pkey because index u_pkey requires it");
}
