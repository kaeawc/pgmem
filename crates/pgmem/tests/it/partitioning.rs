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
    assert_eq!(
        rows(
            &mut c,
            "UPDATE ev SET at = '2025-06-01' WHERE id = 1
             RETURNING tableoid::regclass::text, old.tableoid::regclass::text, new.tableoid::regclass::text"
        ),
        [["ev_eu_25", "ev_eu_24", "ev_eu_25"]]
    );
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
fn returning_of_moved_rows_sees_from_tables() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE pt (id int, v text) PARTITION BY RANGE (id);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);
         CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (10) TO (20);
         INSERT INTO pt VALUES (1, 'a'), (2, 'b'), (3, 'c');
         CREATE TABLE src (id int, w text);
         INSERT INTO src VALUES (1, 'x'), (2, 'y'), (3, 'z')",
    )
    .expect("schema");
    // The issue's repro: the row moves from pt1 to pt2.
    assert_eq!(rows(&mut c, "UPDATE pt SET id = 15 FROM src WHERE pt.id = src.id RETURNING pt.id, src.w").len(), 3);
    c.batch_execute("DELETE FROM pt; INSERT INTO pt VALUES (1, 'a'), (2, 'b')").expect("reset");
    assert_eq!(
        rows(&mut c, "UPDATE pt SET id = 15 FROM src WHERE pt.id = src.id AND src.id = 1 RETURNING pt.id::text, src.w"),
        vec![vec!["15", "x"]]
    );
    // Target columns, FROM columns, expressions mixing both, old / new, tableoid and src.*.
    assert_eq!(
        rows(
            &mut c,
            "UPDATE pt SET id = pt.id + 10, v = src.w FROM src WHERE pt.id = src.id
             RETURNING pt.tableoid::regclass::text, pt.id::text, pt.v || src.w, (src.id * 2)::text,
                       old.id::text, old.v, new.id::text, new.v, src.*"
        ),
        vec![vec!["pt2", "12", "yy", "4", "2", "b", "12", "y", "2", "y"]]
    );
    assert_eq!(rows(&mut c, "UPDATE pt p SET id = 3 FROM src s WHERE p.id = s.id + 13 RETURNING *"), vec![vec!["3", "a", "2", "y"]]);
    // Pin: a row that stays in its partition.
    assert_eq!(
        rows(&mut c, "UPDATE pt SET v = 'q' FROM src WHERE pt.id = src.id + 9 RETURNING pt.tableoid::regclass::text, pt.*, src.w, old.v"),
        vec![vec!["pt2", "12", "q", "z", "y"]]
    );
}

#[test]
fn returning_of_rows_moved_into_sub_partitions_sees_from_tables() {
    let (_db, mut c) = setup();
    schema(&mut c);
    c.batch_execute(
        "INSERT INTO ev (id, region, at, v) VALUES (1, 'us', '2024-03-01', 10), (2, 'eu', '2024-03-01', 20);
         CREATE TABLE moves (id int, region text, at date, note text);
         INSERT INTO moves VALUES (1, 'eu', '2025-06-01', 'to eu 25'), (2, 'us', '2024-03-01', 'to us')",
    )
    .expect("schema");
    assert_eq!(
        rows(
            &mut c,
            "WITH u AS (UPDATE ev SET region = m.region, at = m.at, v = ev.v + m.id FROM moves m WHERE ev.id = m.id
                        RETURNING ev.tableoid::regclass::text AS part, ev.id, ev.v, m.note, old.region AS was, new.at::text AS at)
             SELECT part, id::text, v::text, note, was, at FROM u ORDER BY id"
        ),
        vec![vec!["ev_eu_25", "1", "11", "to eu 25", "us", "2025-06-01"], vec!["ev_us", "2", "22", "to us", "eu", "2024-03-01"]]
    );
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

#[test]
fn merge_into_partitioned_table() {
    let (_db, mut c) = setup();
    schema(&mut c);
    c.batch_execute("INSERT INTO ev VALUES (1, 'eu', '2024-03-01', 1), (2, 'us', '2024-03-01', 2), (3, 'eu', '2025-03-01', 3)")
        .expect("seed");
    // MATCHED UPDATE (in place and moving to another partition), DELETE, NOT MATCHED INSERT
    // routed to its leaf; RETURNING sees each row's leaf as tableoid.
    let mut out = rows(
        &mut c,
        "MERGE INTO ev t USING (VALUES (1, 'us'), (2, 'us'), (4, 'eu'), (3, 'eu')) s(id, r) ON t.id = s.id
           WHEN MATCHED AND s.id = 1 THEN UPDATE SET region = s.r, v = 10
           WHEN MATCHED AND s.id = 2 THEN UPDATE SET v = 20
           WHEN MATCHED AND s.id = 3 THEN DELETE
           WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.r, '2025-05-05')
           RETURNING merge_action(), t.tableoid::regclass::text, t.id::text, t.region, coalesce(old.v::text, '-'), coalesce(new.v::text, '-')",
    );
    out.sort();
    assert_eq!(
        out,
        [
            ["DELETE", "ev_eu_25", "3", "eu", "3", "-"],
            ["INSERT", "ev_eu_25", "4", "eu", "-", "7"],
            ["UPDATE", "ev_us", "1", "us", "1", "10"],
            ["UPDATE", "ev_us", "2", "us", "2", "20"],
        ]
    );
    assert_eq!(
        rows(&mut c, "SELECT tableoid::regclass::text, id::text, region, at::text, v::text FROM ev ORDER BY id"),
        [["ev_us", "1", "us", "2024-03-01", "10"], ["ev_us", "2", "us", "2024-03-01", "20"], ["ev_eu_25", "4", "eu", "2025-05-05", "7"]]
    );
    let e = expect_code(
        c.batch_execute("MERGE INTO ev t USING (VALUES (9, 'mars')) s(id, r) ON t.id = s.id WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.r, '2025-05-05')"),
        "23514",
    );
    assert_eq!(message(&e), "no partition of relation \"ev\" found for row");
    // NOT MATCHED BY SOURCE, DO NOTHING.
    let mut out = rows(
        &mut c,
        "MERGE INTO ev t USING (VALUES (1)) s(id) ON t.id = s.id WHEN NOT MATCHED BY SOURCE THEN DELETE RETURNING merge_action(), t.tableoid::regclass::text, t.id::text",
    );
    out.sort();
    assert_eq!(out, [["DELETE", "ev_eu_25", "4"], ["DELETE", "ev_us", "2"]]);
    let n = c.execute("MERGE INTO ev t USING (VALUES (1)) s(id) ON t.id = s.id WHEN MATCHED THEN DO NOTHING", &[]).expect("do nothing");
    assert_eq!(n, 0);
    // Unique keys are checked per partition (named after it).
    c.batch_execute("INSERT INTO ev VALUES (2, 'us', '2024-03-01')").expect("seed");
    let e =
        expect_code(c.batch_execute("MERGE INTO ev t USING (VALUES (1)) s(a) ON t.id = s.a WHEN MATCHED THEN UPDATE SET id = 2"), "23505");
    assert_eq!(message(&e), "duplicate key value violates unique constraint \"ev_us_pkey\"");
    // A target that is itself a partition: its constraint, then routing below it.
    c.batch_execute("INSERT INTO ev VALUES (5, 'eu', '2024-02-02')").expect("seed");
    let e = expect_code(
        c.batch_execute("MERGE INTO ev_eu t USING (VALUES (5)) s(id) ON t.id = s.id WHEN MATCHED THEN UPDATE SET region = 'us'"),
        "23514",
    );
    assert_eq!(message(&e), "new row for relation \"ev_eu\" violates partition constraint");
    let e = expect_code(
        c.batch_execute("MERGE INTO ev_eu t USING (VALUES (5)) s(id) ON t.id = s.id WHEN MATCHED THEN UPDATE SET at = '2030-01-01'"),
        "23514",
    );
    assert_eq!(message(&e), "no partition of relation \"ev_eu\" found for row");
    let e = expect_code(
        c.batch_execute("MERGE INTO ev_us t USING (VALUES (2)) s(id) ON t.id = s.id WHEN MATCHED THEN UPDATE SET region = 'eu'"),
        "23514",
    );
    assert_eq!(message(&e), "new row for relation \"ev_us\" violates partition constraint");
    assert_eq!(
        rows(
            &mut c,
            "MERGE INTO ev_eu t USING (VALUES (5)) s(id) ON t.id = s.id WHEN MATCHED THEN UPDATE SET at = '2025-02-02' RETURNING t.tableoid::regclass::text"
        ),
        [["ev_eu_25"]]
    );
}

#[test]
fn merge_returning_system_columns() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE mp (a int); INSERT INTO mp VALUES (1)").expect("setup");
    let mut out = rows(
        &mut c,
        "MERGE INTO mp p USING (VALUES (1), (2)) s(a) ON p.a = s.a WHEN MATCHED THEN UPDATE SET a = 3
           WHEN NOT MATCHED THEN INSERT VALUES (s.a) RETURNING p.tableoid::regclass::text, merge_action(), p.a::text",
    );
    out.sort();
    assert_eq!(out, [["mp", "INSERT", "2"], ["mp", "UPDATE", "3"]]);
}

#[test]
fn merge_into_partitioned_table_foreign_keys() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE ma (id int PRIMARY KEY) PARTITION BY HASH (id);
         CREATE TABLE ma0 PARTITION OF ma FOR VALUES WITH (MODULUS 2, REMAINDER 0);
         CREATE TABLE ma1 PARTITION OF ma FOR VALUES WITH (MODULUS 2, REMAINDER 1);
         INSERT INTO ma SELECT generate_series(1, 4);
         CREATE TABLE mt (acct int REFERENCES ma ON DELETE CASCADE);
         INSERT INTO mt VALUES (1), (2), (3)",
    )
    .expect("setup");
    assert_eq!(
        c.execute("MERGE INTO ma t USING (VALUES (1), (2), (9)) s(id) ON t.id = s.id WHEN MATCHED THEN DELETE WHEN NOT MATCHED THEN INSERT VALUES (s.id)", &[])
            .expect("merge"),
        3
    );
    assert_eq!(rows(&mut c, "SELECT string_agg(acct::text, ',' ORDER BY acct) FROM mt"), [["3"]]);
    assert_eq!(rows(&mut c, "SELECT string_agg(id::text, ',' ORDER BY id) FROM ma"), [["3,4,9"]]);
}
