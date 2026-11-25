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
