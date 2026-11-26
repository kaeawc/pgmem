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

fn hint(e: &postgres::Error) -> String {
    e.as_db_error().and_then(|d| d.hint()).unwrap_or_default().to_owned()
}

fn trigger_schema(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE log (n serial, name text, lvl text, tab text, op text, v text);
         CREATE FUNCTION lg() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN
             INSERT INTO log (name, lvl, tab, op, v) VALUES (TG_NAME, TG_LEVEL, TG_TABLE_NAME, TG_OP, CASE WHEN TG_LEVEL = 'ROW' AND TG_OP <> 'DELETE' THEN NEW.v END);
             IF TG_OP = 'DELETE' THEN RETURN OLD; END IF;
             RETURN NEW;
           END $$;
         CREATE FUNCTION bang() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.v := NEW.v || '!' || TG_TABLE_NAME; RETURN NEW; END $$;
         CREATE TABLE pt (id int, v text) PARTITION BY RANGE (id);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);
         CREATE TABLE pt2 PARTITION OF pt FOR VALUES FROM (10) TO (100) PARTITION BY RANGE (id);
         CREATE TABLE pt21 PARTITION OF pt2 FOR VALUES FROM (10) TO (50)",
    )
    .expect("trigger schema");
}

/// (table, trigger, parent trigger's table, tgenabled) of the triggers on pt*.
fn pt_triggers(c: &mut postgres::Client) -> Vec<Vec<String>> {
    rows(
        c,
        "SELECT t.tgrelid::regclass::text, t.tgname::text, coalesce((SELECT p.tgrelid::regclass::text FROM pg_trigger p WHERE p.oid = t.tgparentid), '-'), t.tgenabled::text
           FROM pg_trigger t WHERE t.tgrelid::regclass::text LIKE 'pt%' ORDER BY 1, 2",
    )
}

#[test]
fn row_triggers_clone_to_partitions_and_fire_on_the_leaf() {
    let (_db, mut c) = setup();
    trigger_schema(&mut c);
    c.batch_execute(
        "CREATE TRIGGER b BEFORE INSERT ON pt FOR EACH ROW EXECUTE FUNCTION bang();
         CREATE TRIGGER r AFTER INSERT OR UPDATE OR DELETE ON pt FOR EACH ROW EXECUTE FUNCTION lg();
         CREATE TRIGGER s AFTER INSERT OR UPDATE OR DELETE ON pt FOR EACH STATEMENT EXECUTE FUNCTION lg();
         CREATE TRIGGER ls AFTER INSERT OR UPDATE OR DELETE ON pt1 FOR EACH STATEMENT EXECUTE FUNCTION lg()",
    )
    .expect("triggers");
    // Row triggers are cloned to every partition, sub-partitions included; statement triggers stay.
    assert_eq!(
        pt_triggers(&mut c),
        [
            ["pt", "b", "-", "O"],
            ["pt", "r", "-", "O"],
            ["pt", "s", "-", "O"],
            ["pt1", "b", "pt", "O"],
            ["pt1", "ls", "-", "O"],
            ["pt1", "r", "pt", "O"],
            ["pt2", "b", "pt", "O"],
            ["pt2", "r", "pt", "O"],
            ["pt21", "b", "pt2", "O"],
            ["pt21", "r", "pt2", "O"],
        ]
    );
    // Row triggers fire once per row on the leaf; the parent's statement trigger fires once and
    // the leaf's statement triggers do not fire for statements on the parent.
    c.batch_execute(
        "INSERT INTO pt VALUES (1, 'a'), (11, 'b');
         UPDATE pt SET v = 'c' WHERE id = 11;
         DELETE FROM pt WHERE id = 1;
         INSERT INTO pt1 VALUES (2, 'x')",
    )
    .expect("dml");
    assert_eq!(
        rows(&mut c, "SELECT name, lvl, tab, op, coalesce(v, '-') FROM log ORDER BY n"),
        [
            ["r", "ROW", "pt1", "INSERT", "a!pt1"],
            ["r", "ROW", "pt21", "INSERT", "b!pt21"],
            ["s", "STATEMENT", "pt", "INSERT", "-"],
            ["r", "ROW", "pt21", "UPDATE", "c"],
            ["s", "STATEMENT", "pt", "UPDATE", "-"],
            ["r", "ROW", "pt1", "DELETE", "-"],
            ["s", "STATEMENT", "pt", "DELETE", "-"],
            ["r", "ROW", "pt1", "INSERT", "x!pt1"],
            ["ls", "STATEMENT", "pt1", "INSERT", "-"],
        ]
    );
    // Partitions created later get clones too, with the parent trigger's enabled state.
    c.batch_execute("ALTER TABLE pt DISABLE TRIGGER r; CREATE TABLE pt3 PARTITION OF pt FOR VALUES FROM (100) TO (200)").expect("pt3");
    assert_eq!(
        rows(&mut c, "SELECT tgname::text, tgenabled::text FROM pg_trigger WHERE tgrelid = 'pt3'::regclass ORDER BY 1"),
        [["b", "O"], ["r", "D"]]
    );
}

#[test]
fn partition_trigger_clones_follow_their_parent() {
    let (_db, mut c) = setup();
    trigger_schema(&mut c);
    c.batch_execute("CREATE TRIGGER r AFTER INSERT ON pt FOR EACH ROW EXECUTE FUNCTION lg()").expect("trigger");
    // A clone cannot be dropped, renamed or replaced by itself.
    let e = expect_code(c.batch_execute("DROP TRIGGER r ON pt1"), "2BP01");
    assert_eq!(message(&e), "cannot drop trigger r on table pt1 because trigger r on table pt requires it");
    assert_eq!(hint(&e), "You can drop trigger r on table pt instead.");
    let e = expect_code(c.batch_execute("DROP TRIGGER r ON pt21"), "2BP01");
    assert_eq!(message(&e), "cannot drop trigger r on table pt21 because trigger r on table pt2 requires it");
    let e = expect_code(c.batch_execute("ALTER TRIGGER r ON pt1 RENAME TO q"), "0A000");
    assert_eq!(message(&e), "cannot rename trigger \"r\" on table \"pt1\"");
    assert_eq!(hint(&e), "Rename the trigger on the partitioned table \"pt\" instead.");
    let e = expect_code(c.batch_execute("CREATE OR REPLACE TRIGGER r AFTER INSERT ON pt1 FOR EACH ROW EXECUTE FUNCTION lg()"), "42710");
    assert_eq!(message(&e), "trigger \"r\" for relation \"pt1\" is an internal or a child trigger");
    // A partition's own trigger with the name blocks creating (or renaming) the parent's.
    c.batch_execute("CREATE TRIGGER q AFTER INSERT ON pt21 FOR EACH ROW EXECUTE FUNCTION lg()").expect("own trigger");
    let e = expect_code(c.batch_execute("CREATE TRIGGER q AFTER INSERT ON pt FOR EACH ROW EXECUTE FUNCTION lg()"), "42710");
    assert_eq!(message(&e), "trigger \"q\" for relation \"pt21\" already exists");
    let e = expect_code(c.batch_execute("ALTER TRIGGER r ON pt RENAME TO q"), "42710");
    assert_eq!(message(&e), "trigger \"q\" for relation \"pt21\" already exists");
    // RENAME and ENABLE / DISABLE on the parent recurse to the clones only.
    c.batch_execute("ALTER TRIGGER r ON pt RENAME TO t; ALTER TABLE pt DISABLE TRIGGER ALL").expect("rename, disable");
    assert_eq!(
        pt_triggers(&mut c),
        [["pt", "t", "-", "D"], ["pt1", "t", "pt", "D"], ["pt2", "t", "pt", "D"], ["pt21", "q", "-", "O"], ["pt21", "t", "pt2", "D"]]
    );
    c.batch_execute("ALTER TABLE pt1 ENABLE TRIGGER t; ALTER TABLE pt ENABLE ALWAYS TRIGGER t").expect("enable");
    assert_eq!(
        pt_triggers(&mut c),
        [["pt", "t", "-", "A"], ["pt1", "t", "pt", "A"], ["pt2", "t", "pt", "A"], ["pt21", "q", "-", "O"], ["pt21", "t", "pt2", "A"]]
    );
    // CREATE OR REPLACE on the parent replaces the clones.
    c.batch_execute("CREATE OR REPLACE TRIGGER t BEFORE INSERT ON pt FOR EACH ROW EXECUTE FUNCTION bang()").expect("replace");
    assert_eq!(rows(&mut c, "SELECT DISTINCT tgtype::text FROM pg_trigger WHERE tgname = 't'"), [["7"]]);
    // DETACH drops the clones of the partition and its sub-partitions; ATTACH clones again.
    c.batch_execute("ALTER TABLE pt DETACH PARTITION pt2").expect("detach");
    assert_eq!(pt_triggers(&mut c), [["pt", "t", "-", "O"], ["pt1", "t", "pt", "O"], ["pt21", "q", "-", "O"]]);
    c.batch_execute("ALTER TABLE pt ATTACH PARTITION pt2 FOR VALUES FROM (10) TO (100)").expect("attach");
    assert_eq!(
        pt_triggers(&mut c),
        [["pt", "t", "-", "O"], ["pt1", "t", "pt", "O"], ["pt2", "t", "pt", "O"], ["pt21", "q", "-", "O"], ["pt21", "t", "pt2", "O"]]
    );
    // DROP TRIGGER on the parent drops the clones.
    c.batch_execute("DROP TRIGGER t ON pt").expect("drop");
    assert_eq!(pt_triggers(&mut c), [["pt21", "q", "-", "O"]]);
}

#[test]
fn attached_partition_gets_clones_mapped_by_column_name() {
    let (_db, mut c) = setup();
    trigger_schema(&mut c);
    c.batch_execute(
        "CREATE TRIGGER u AFTER INSERT OR UPDATE OF v ON pt FOR EACH ROW EXECUTE FUNCTION lg('x');
         CREATE TRIGGER w AFTER INSERT ON pt FOR EACH ROW WHEN (NEW.id > 150) EXECUTE FUNCTION lg();
         CREATE TABLE pt3 (v text, id int);
         CREATE TRIGGER own AFTER INSERT ON pt3 FOR EACH ROW EXECUTE FUNCTION lg();
         ALTER TABLE pt ATTACH PARTITION pt3 FOR VALUES FROM (100) TO (200)",
    )
    .expect("attach");
    assert_eq!(
        rows(
            &mut c,
            "SELECT tgname::text, (tgparentid <> 0)::text, coalesce(tgattr[0]::text, '-'), pg_get_triggerdef(oid) FROM pg_trigger WHERE tgrelid = 'pt3'::regclass AND tgname <> 'w' ORDER BY 1"
        ),
        [
            ["own", "false", "-", "CREATE TRIGGER own AFTER INSERT ON public.pt3 FOR EACH ROW EXECUTE FUNCTION lg()"],
            ["u", "true", "1", "CREATE TRIGGER u AFTER INSERT OR UPDATE OF v ON public.pt3 FOR EACH ROW EXECUTE FUNCTION lg('x')"],
        ]
    );
    c.batch_execute(
        "INSERT INTO pt VALUES (150, 'z'); UPDATE pt SET id = 151 WHERE id = 150; UPDATE pt SET v = 'y' WHERE id = 151;
         INSERT INTO pt VALUES (160, 'q')",
    )
    .expect("dml");
    assert_eq!(
        rows(&mut c, "SELECT name, tab, op, v FROM log ORDER BY n"),
        [
            ["own", "pt3", "INSERT", "z"],
            ["u", "pt3", "INSERT", "z"],
            ["u", "pt3", "UPDATE", "y"],
            ["own", "pt3", "INSERT", "q"],
            ["u", "pt3", "INSERT", "q"],
            ["w", "pt3", "INSERT", "q"]
        ]
    );
    // A clone that collides with a trigger of the table being attached fails the ATTACH.
    c.batch_execute("CREATE TABLE pt4 (id int, v text); CREATE TRIGGER u AFTER INSERT ON pt4 FOR EACH ROW EXECUTE FUNCTION lg()")
        .expect("pt4");
    let e = expect_code(c.batch_execute("ALTER TABLE pt ATTACH PARTITION pt4 FOR VALUES FROM (200) TO (300)"), "42710");
    assert_eq!(message(&e), "trigger \"u\" for relation \"pt4\" already exists");
    assert_eq!(rows(&mut c, "SELECT relispartition::text FROM pg_class WHERE relname = 'pt4'"), [["false"]]);
    // Triggers on plain tables are unaffected.
    c.batch_execute(
        "CREATE TABLE plain (id int, v text);
         CREATE TRIGGER p AFTER INSERT ON plain FOR EACH ROW EXECUTE FUNCTION lg();
         CREATE TRIGGER ps AFTER INSERT ON plain FOR EACH STATEMENT EXECUTE FUNCTION lg();
         DELETE FROM log; INSERT INTO plain VALUES (1, 'p')",
    )
    .expect("plain");
    assert_eq!(rows(&mut c, "SELECT name, tab, coalesce(v, '-') FROM log ORDER BY n"), [["p", "plain", "p"], ["ps", "plain", "-"]]);
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
