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

#[test]
fn partition_of_after_dropped_column() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE dp_ref (id int PRIMARY KEY);
         INSERT INTO dp_ref VALUES (1), (2);
         CREATE TABLE dp (a int NOT NULL, b int, c text DEFAULT 'dflt', r int REFERENCES dp_ref, e int CHECK (e > 0),
           PRIMARY KEY (a, c)) PARTITION BY LIST (a);
         CREATE TABLE dp1 PARTITION OF dp FOR VALUES IN (1);
         ALTER TABLE dp DROP COLUMN b;
         CREATE TABLE dp2 PARTITION OF dp FOR VALUES IN (2, 3) PARTITION BY LIST (a);
         CREATE TABLE dp2a PARTITION OF dp2 FOR VALUES IN (2);
         CREATE TABLE dp2b PARTITION OF dp2 FOR VALUES IN (3);
         CREATE INDEX dp_e_idx ON dp (e)",
    )
    .expect("schema");
    // A partition created before the DROP keeps the dropped slot; one created after has only
    // the live columns (sub-partitions too).
    assert_eq!(
        rows(
            &mut c,
            "SELECT attrelid::regclass::text, attname::text, attnum::text, attisdropped::text FROM pg_attribute
              WHERE attrelid IN ('dp1'::regclass, 'dp2'::regclass, 'dp2b'::regclass) AND attnum > 0 ORDER BY 1, 3::int"
        ),
        [
            ["dp1", "a", "1", "false"],
            ["dp1", "........pg.dropped.2........", "2", "true"],
            ["dp1", "c", "3", "false"],
            ["dp1", "r", "4", "false"],
            ["dp1", "e", "5", "false"],
            ["dp2", "a", "1", "false"],
            ["dp2", "c", "2", "false"],
            ["dp2", "r", "3", "false"],
            ["dp2", "e", "4", "false"],
            ["dp2b", "a", "1", "false"],
            ["dp2b", "c", "2", "false"],
            ["dp2b", "r", "3", "false"],
            ["dp2b", "e", "4", "false"],
        ]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT column_name::text, ordinal_position::text, coalesce(column_default::text, '-') FROM information_schema.columns
              WHERE table_name = 'dp2b' ORDER BY ordinal_position"
        ),
        [["a", "1", "-"], ["c", "2", "'dflt'::text"], ["r", "3", "-"], ["e", "4", "-"]]
    );
    // Cloned indexes and key / CHECK / foreign key constraints use the partition's attnums.
    assert_eq!(
        rows(
            &mut c,
            "SELECT indexrelid::regclass::text, array_to_string(indkey::int2[], ' ') FROM pg_index
              WHERE indrelid IN ('dp1'::regclass, 'dp2b'::regclass) ORDER BY 1"
        ),
        [["dp1_e_idx", "5"], ["dp1_pkey", "1 3"], ["dp2b_e_idx", "4"], ["dp2b_pkey", "1 2"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT conrelid::regclass::text, conname::text, conkey::text FROM pg_constraint
              WHERE conrelid IN ('dp1'::regclass, 'dp2b'::regclass) AND contype IN ('p', 'c', 'f') ORDER BY 1, 2"
        ),
        [
            ["dp1", "dp1_pkey", "{1,3}"],
            ["dp1", "dp_e_check", "{5}"],
            ["dp1", "dp_r_fkey", "{4}"],
            ["dp2b", "dp2b_pkey", "{1,2}"],
            ["dp2b", "dp_e_check", "{4}"],
            ["dp2b", "dp_r_fkey", "{3}"],
        ]
    );
    // Routing, defaults, RETURNING and SELECT * through the parent.
    assert_eq!(
        rows(&mut c, "INSERT INTO dp (a, r, e) VALUES (1, 1, 5), (2, 2, 6), (3, 1, 7) RETURNING tableoid::regclass::text, *"),
        [["dp1", "1", "dflt", "1", "5"], ["dp2a", "2", "dflt", "2", "6"], ["dp2b", "3", "dflt", "1", "7"]]
    );
    // Row movement both ways between layouts.
    assert_eq!(
        rows(&mut c, "UPDATE dp SET a = 3, c = 'm' WHERE a = 2 RETURNING tableoid::regclass::text, *"),
        [["dp2b", "3", "m", "2", "6"]]
    );
    assert_eq!(
        rows(&mut c, "UPDATE dp SET a = 1, e = 9 WHERE c = 'm' RETURNING tableoid::regclass::text, *"),
        [["dp1", "1", "m", "2", "9"]]
    );
    assert_eq!(
        rows(&mut c, "UPDATE dp SET a = 2, c = 'n' WHERE a = 1 AND c = 'dflt' RETURNING tableoid::regclass::text, *"),
        [["dp2a", "2", "n", "1", "5"]]
    );
    // Constraints check the right columns on the new layout.
    let e = expect_code(c.batch_execute("INSERT INTO dp (a, c, r, e) VALUES (3, 'fk', 9, 1)"), "23503");
    assert_eq!(message(&e), "insert or update on table \"dp2b\" violates foreign key constraint \"dp_r_fkey\"");
    expect_code(c.batch_execute("INSERT INTO dp (a, c, r, e) VALUES (3, 'ck', 1, 0)"), "23514");
    let e = expect_code(c.batch_execute("INSERT INTO dp (a, r, e) VALUES (3, 1, 1)"), "23505");
    assert_eq!(message(&e), "duplicate key value violates unique constraint \"dp2b_pkey\"");
    expect_code(c.batch_execute("DELETE FROM dp_ref WHERE id = 2"), "23503");
    let mut w = c.copy_in("COPY dp (a, c, r, e) FROM STDIN").expect("copy in");
    std::io::Write::write_all(&mut w, b"2\tcp\t1\t3\n").expect("write");
    assert_eq!(w.finish().expect("finish"), 1);
    // DETACH and ATTACH the sub-partitioned partition back.
    c.batch_execute("ALTER TABLE dp DETACH PARTITION dp2").expect("detach");
    assert_eq!(
        rows(&mut c, "SELECT tableoid::regclass::text, * FROM dp2 ORDER BY a, c"),
        [["dp2a", "2", "cp", "1", "3"], ["dp2a", "2", "n", "1", "5"], ["dp2b", "3", "dflt", "1", "7"]]
    );
    c.batch_execute("ALTER TABLE dp ATTACH PARTITION dp2 FOR VALUES IN (2, 3)").expect("attach");
    assert_eq!(
        rows(&mut c, "SELECT tableoid::regclass::text, * FROM dp ORDER BY a, c"),
        [["dp1", "1", "m", "2", "9"], ["dp2a", "2", "cp", "1", "3"], ["dp2a", "2", "n", "1", "5"], ["dp2b", "3", "dflt", "1", "7"],]
    );
    assert_eq!(rows(&mut c, "DELETE FROM dp WHERE a = 3 RETURNING tableoid::regclass::text, *"), [["dp2b", "3", "dflt", "1", "7"]]);
}

fn detail(e: &postgres::Error) -> String {
    e.as_db_error().and_then(|d| d.detail()).unwrap_or_default().to_owned()
}

#[test]
fn partition_column_options_and_inherited_names() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE pn (a int NOT NULL, b int, e int DEFAULT 7, CONSTRAINT pn_b_ck CHECK (b < 100)) PARTITION BY RANGE (a);
         CREATE TABLE pn1 PARTITION OF pn (e DEFAULT 42) FOR VALUES FROM (0) TO (10)",
    )
    .expect("partition with column options");
    // The partition's own default applies when inserting into it, the parent's through the parent.
    c.batch_execute("INSERT INTO pn1 (a, b) VALUES (1, 1); INSERT INTO pn (a, b) VALUES (2, 2)").expect("insert");
    assert_eq!(rows(&mut c, "SELECT a::text, e::text FROM pn ORDER BY a"), [["1", "42"], ["2", "7"]]);
    // A partition keeps the parent's NOT NULL constraint name.
    assert_eq!(
        rows(
            &mut c,
            "SELECT conrelid::regclass::text, conname FROM pg_constraint WHERE contype = 'n' AND conrelid IN ('pn'::regclass, 'pn1'::regclass) ORDER BY 1"
        ),
        [["pn", "pn_a_not_null"], ["pn1", "pn_a_not_null"]]
    );
    // A CHECK violated through the parent names the partition the row routed to.
    let e = expect_code(c.batch_execute("INSERT INTO pn VALUES (3, 200, 1)"), "23514");
    assert_eq!(message(&e), "new row for relation \"pn1\" violates check constraint \"pn_b_ck\"");
    assert_eq!(detail(&e), "Failing row contains (3, 200, 1).");
    // Neighbouring behaviour: no partition for the row is reported before CHECKs.
    let e = expect_code(c.batch_execute("INSERT INTO pn VALUES (30, 200, 1)"), "23514");
    assert_eq!(message(&e), "no partition of relation \"pn\" found for row");
}

#[test]
fn referenced_partitioned_table_error_names_the_partition() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE rp (a int PRIMARY KEY) PARTITION BY RANGE (a);
         CREATE TABLE rp1 PARTITION OF rp FOR VALUES FROM (0) TO (10);
         CREATE TABLE rp2 PARTITION OF rp FOR VALUES FROM (10) TO (20) PARTITION BY RANGE (a);
         CREATE TABLE rp2a PARTITION OF rp2 FOR VALUES FROM (10) TO (15);
         CREATE TABLE rc (x int CONSTRAINT rc_fk REFERENCES rp (a));
         INSERT INTO rp VALUES (1), (12);
         INSERT INTO rc VALUES (1), (12)",
    )
    .expect("schema");
    let e = expect_code(c.batch_execute("DELETE FROM rp WHERE a = 1"), "23503");
    assert_eq!(message(&e), "update or delete on table \"rp1\" violates foreign key constraint \"rc_fk_1\" on table \"rc\"");
    assert_eq!(detail(&e), "Key (a)=(1) is still referenced from table \"rc\".");
    let e = expect_code(c.batch_execute("DELETE FROM rp WHERE a = 12"), "23503");
    assert_eq!(message(&e), "update or delete on table \"rp2a\" violates foreign key constraint \"rc_fk_3\" on table \"rc\"");
    let e = expect_code(c.batch_execute("DELETE FROM rp2a WHERE a = 12"), "23503");
    assert_eq!(message(&e), "update or delete on table \"rp2a\" violates foreign key constraint \"rc_fk_3\" on table \"rc\"");
    // A plain referenced table still names itself and the constraint as declared.
    c.batch_execute("CREATE TABLE pk (id int PRIMARY KEY); CREATE TABLE pkc (x int CONSTRAINT pkc_fk REFERENCES pk); INSERT INTO pk VALUES (1); INSERT INTO pkc VALUES (1)")
        .expect("plain");
    let e = expect_code(c.batch_execute("DELETE FROM pk"), "23503");
    assert_eq!(message(&e), "update or delete on table \"pk\" violates foreign key constraint \"pkc_fk\" on table \"pkc\"");
}

#[test]
fn new_partitions_reset_missing_defaults() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE mp (a int) PARTITION BY RANGE (a);
         CREATE TABLE mp1 PARTITION OF mp FOR VALUES FROM (0) TO (10);
         ALTER TABLE mp ADD COLUMN z int DEFAULT 5;
         CREATE TABLE mp2 PARTITION OF mp FOR VALUES FROM (10) TO (20)",
    )
    .expect("schema");
    assert_eq!(
        rows(
            &mut c,
            "SELECT attrelid::regclass::text, atthasmissing::text FROM pg_attribute WHERE attname = 'z' AND attrelid IN ('mp1'::regclass, 'mp2'::regclass) ORDER BY 1"
        ),
        [["mp1", "true"], ["mp2", "false"]]
    );
}

#[test]
fn drop_schema_cascade_notice() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA ds;
         CREATE TABLE ds.pt (a serial) PARTITION BY RANGE (a);
         CREATE TABLE ds.pt1 PARTITION OF ds.pt FOR VALUES FROM (0) TO (5);
         CREATE SEQUENCE ds.sq;
         CREATE TABLE ds.t2 (a int)",
    )
    .expect("schema");
    let mut raw = crate::support::RawConn::connect(&db, &mut c);
    // Dependents are listed in creation order, partitions are not listed (the table
    // drops them), and the list is the DETAIL.
    let r = raw.query("DROP SCHEMA ds");
    assert_eq!(r.errors, ["2BP01"]);
    let r = raw.query("DROP SCHEMA ds CASCADE");
    assert_eq!(
        r.notices,
        ["drop cascades to 3 other objects\ndrop cascades to table ds.pt\ndrop cascades to sequence ds.sq\ndrop cascades to table ds.t2"]
    );
    // One dependent: just the message.
    c.batch_execute("CREATE SCHEMA ds1; CREATE TABLE ds1.solo (a int)").expect("schema");
    let r = raw.query("DROP SCHEMA ds1 CASCADE");
    assert_eq!(r.notices, ["drop cascades to table ds1.solo"]);
}
