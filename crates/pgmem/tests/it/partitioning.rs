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
