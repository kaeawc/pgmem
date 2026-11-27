//! Table inheritance: CREATE TABLE ... INHERITS (column merging, inherited CHECK / NOT NULL
//! constraints and defaults), scans and UPDATE / DELETE of a parent with and without ONLY,
//! ALTER TABLE recursion and its errors, ALTER TABLE ... [NO] INHERIT, the catalogs
//! (pg_inherits, attinhcount / attislocal, coninhcount / conislocal / connoinherit,
//! relhassubclass) and DROP TABLE.

use crate::support::{RawConn, TestDb, expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

/// The NOTICE messages of one simple query (which must succeed).
fn notices(db: &TestDb, c: &mut postgres::Client, sql: &str) -> Vec<String> {
    let mut raw = RawConn::connect(db, c);
    let r = raw.query(sql);
    assert!(r.errors.is_empty(), "{sql}: {:?}", r.errors);
    r.notices
}

const ATTS: &str = "SELECT attrelid::regclass::text, attname::text, attnum::text, attnotnull::text, attislocal::text, attinhcount::text
    FROM pg_attribute WHERE attrelid IN ('p'::regclass, 'q'::regclass, 'c'::regclass, 'm'::regclass) AND attnum > 0 AND NOT attisdropped
    ORDER BY 1, attnum";

const CONS: &str = "SELECT conrelid::regclass::text, conname::text, contype::text, conislocal::text, coninhcount::text, connoinherit::text
    FROM pg_constraint WHERE conrelid IN (SELECT oid FROM pg_class WHERE relnamespace = 'public'::regnamespace) ORDER BY 1, 2";

#[test]
fn create_inherits_merges_columns() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int NOT NULL, b text DEFAULT 'pb', CONSTRAINT pc CHECK (a > 0), CONSTRAINT pn CHECK (a < 1000) NO INHERIT);
         CREATE TABLE q (a int, d int NOT NULL)",
    )
    .expect("parents");
    assert_eq!(
        notices(&db, &mut c, "CREATE TABLE c (b text, c int, a int) INHERITS (p)"),
        [
            "moving and merging column \"b\" with inherited definition\nUser-specified column moved to the position of the inherited column.",
            "moving and merging column \"a\" with inherited definition\nUser-specified column moved to the position of the inherited column."
        ]
    );
    assert_eq!(notices(&db, &mut c, "CREATE TABLE m (e int) INHERITS (p, q)"), ["merging multiple inherited definitions of column \"a\""]);
    assert_eq!(
        rows(&mut c, ATTS),
        [
            ["c", "a", "1", "true", "true", "1"],
            ["c", "b", "2", "false", "true", "1"],
            ["c", "c", "3", "false", "true", "0"],
            ["m", "a", "1", "true", "false", "2"],
            ["m", "b", "2", "false", "false", "1"],
            ["m", "d", "3", "true", "false", "1"],
            ["m", "e", "4", "false", "true", "0"],
            ["p", "a", "1", "true", "true", "0"],
            ["p", "b", "2", "false", "true", "0"],
            ["q", "a", "1", "false", "true", "0"],
            ["q", "d", "2", "true", "true", "0"],
        ]
    );
    assert_eq!(
        rows(&mut c, CONS),
        [
            ["c", "p_a_not_null", "n", "false", "1", "false"],
            ["c", "pc", "c", "false", "1", "false"],
            ["m", "p_a_not_null", "n", "false", "1", "false"],
            ["m", "pc", "c", "false", "1", "false"],
            ["m", "q_d_not_null", "n", "false", "1", "false"],
            ["p", "p_a_not_null", "n", "true", "0", "false"],
            ["p", "pc", "c", "true", "0", "false"],
            ["p", "pn", "c", "true", "0", "true"],
            ["q", "q_d_not_null", "n", "true", "0", "false"],
        ]
    );
    // Defaults and constraints are inherited (NO INHERIT ones are not).
    c.batch_execute("INSERT INTO m (a, d) VALUES (5000, 1)").expect("NO INHERIT check stays on the parent");
    assert_eq!(rows(&mut c, "SELECT b FROM m"), [["pb"]]);
    let e = expect_code(c.batch_execute("INSERT INTO c (a) VALUES (0)"), "23514");
    assert_eq!(message(&e), "new row for relation \"c\" violates check constraint \"pc\"");
    expect_code(c.batch_execute("INSERT INTO c (b) VALUES ('x')"), "23502");
    assert_eq!(
        rows(&mut c, "SELECT inhrelid::regclass::text, inhparent::regclass::text, inhseqno::text FROM pg_inherits ORDER BY 1, 3"),
        [["c", "p", "1"], ["m", "p", "1"], ["m", "q", "2"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT relname::text, relhassubclass::text FROM pg_class WHERE relname IN ('p', 'q', 'c', 'm') ORDER BY relname"),
        [["c", "false"], ["m", "false"], ["p", "true"], ["q", "true"]]
    );
    // A local CHECK with the inherited name and expression merges into it.
    assert_eq!(
        notices(&db, &mut c, "CREATE TABLE c2 (a int, CONSTRAINT pc CHECK (a > 0)) INHERITS (p)"),
        ["merging column \"a\" with inherited definition", "merging constraint \"pc\" with inherited definition"]
    );
    assert_eq!(
        rows(&mut c, "SELECT conislocal::text, coninhcount::text FROM pg_constraint WHERE conrelid = 'c2'::regclass AND conname = 'pc'"),
        [["true", "1"]]
    );
}

#[test]
fn create_inherits_errors() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int NOT NULL, b text DEFAULT 'pb', CONSTRAINT ck CHECK (a > 0));
         CREATE TABLE q (b text DEFAULT 'qb');
         CREATE TABLE r (a bigint);
         CREATE VIEW v AS SELECT 1 AS x;
         CREATE TABLE pt (a int) PARTITION BY LIST (a)",
    )
    .expect("schema");
    let e = expect_code(c.batch_execute("CREATE TABLE c (a bigint) INHERITS (p)"), "42804");
    assert_eq!(message(&e), "column \"a\" has a type conflict");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("integer versus bigint"));
    let e = expect_code(c.batch_execute("CREATE TABLE c () INHERITS (p, r)"), "42804");
    assert_eq!(message(&e), "inherited column \"a\" has a type conflict");
    let e = expect_code(c.batch_execute("CREATE TABLE c (b text COLLATE \"C\") INHERITS (p)"), "42P21");
    assert_eq!(message(&e), "column \"b\" has a collation conflict");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("\"default\" versus \"C\""));
    let e = expect_code(c.batch_execute("CREATE TABLE c () INHERITS (p, q)"), "42611");
    assert_eq!(message(&e), "column \"b\" inherits conflicting default values");
    c.batch_execute("CREATE TABLE c_ok (b text DEFAULT 'own') INHERITS (p, q)").expect("an explicit default resolves the conflict");
    let e = expect_code(c.batch_execute("CREATE TABLE c () INHERITS (p, p)"), "42P07");
    assert_eq!(message(&e), "relation \"p\" would be inherited from more than once");
    let e = expect_code(c.batch_execute("CREATE TABLE c () INHERITS (nope)"), "42P01");
    assert_eq!(message(&e), "relation \"nope\" does not exist");
    let e = expect_code(c.batch_execute("CREATE TABLE c () INHERITS (v)"), "42809");
    assert_eq!(message(&e), "inherited relation \"v\" is not a table or foreign table");
    let e = expect_code(c.batch_execute("CREATE TABLE c () INHERITS (pt)"), "42809");
    assert_eq!(message(&e), "cannot inherit from partitioned table \"pt\"");
    let e = expect_code(c.batch_execute("CREATE TABLE c (CONSTRAINT ck CHECK (a > 1)) INHERITS (p)"), "42710");
    assert_eq!(message(&e), "constraint \"ck\" for relation \"c\" already exists");
}

#[test]
fn scans_and_dml_recurse_unless_only() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (id int, v text);
         CREATE TABLE c (extra int DEFAULT 9) INHERITS (p);
         CREATE TABLE g (v2 int) INHERITS (c);
         CREATE UNIQUE INDEX p_id ON p (id);
         INSERT INTO p VALUES (1, 'p');
         INSERT INTO c VALUES (1, 'c');
         INSERT INTO g VALUES (3, 'g', 0, 7)",
    )
    .expect("schema");
    let all = "SELECT tableoid::regclass::text, id::text, v FROM p ORDER BY id, v";
    assert_eq!(rows(&mut c, all), [["c", "1", "c"], ["p", "1", "p"], ["g", "3", "g"]]);
    assert_eq!(rows(&mut c, "SELECT * FROM p* ORDER BY id, v"), [["1", "c"], ["1", "p"], ["3", "g"]]);
    assert_eq!(rows(&mut c, "SELECT id::text, v FROM ONLY p"), [["1", "p"]]);
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM c"), [["2"]]);
    // INSERT goes only to the named table; the parent's unique index does not cover children.
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM ONLY c"), [["1"]]);
    let mut updated = rows(&mut c, "UPDATE p SET v = v || '!' WHERE id = 1 RETURNING tableoid::regclass::text, *");
    updated.sort();
    assert_eq!(updated, [["c", "1", "c!"], ["p", "1", "p!"]]);
    assert_eq!(rows(&mut c, "UPDATE ONLY p SET v = 'only' RETURNING v"), [["only"]]);
    assert_eq!(rows(&mut c, all), [["c", "1", "c!"], ["p", "1", "only"], ["g", "3", "g"]]);
    assert_eq!(rows(&mut c, "SELECT v, extra::text FROM c ORDER BY id"), [["c!", "9"], ["g", "0"]]);
    assert_eq!(rows(&mut c, "DELETE FROM ONLY p RETURNING id::text"), [["1"]]);
    assert_eq!(rows(&mut c, "DELETE FROM p WHERE v = 'g' RETURNING tableoid::regclass::text"), [["g"]]);
    assert_eq!(rows(&mut c, all), [["c", "1", "c!"]]);
    c.batch_execute("TRUNCATE ONLY p").expect("truncate only");
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM p"), [["1"]]);
    c.batch_execute("TRUNCATE p").expect("truncate");
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM p"), [["0"]]);
}

#[test]
fn triggers_fire_per_table() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (id int);
         CREATE TABLE c () INHERITS (p);
         CREATE TABLE log (what text);
         CREATE FUNCTION note() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN INSERT INTO log VALUES (TG_TABLE_NAME || ' ' || TG_LEVEL || ' ' || TG_OP); RETURN NULL; END $$;
         CREATE TRIGGER p_row AFTER UPDATE ON p FOR EACH ROW EXECUTE FUNCTION note();
         CREATE TRIGGER p_stmt AFTER UPDATE ON p FOR EACH STATEMENT EXECUTE FUNCTION note();
         CREATE TRIGGER c_row AFTER UPDATE ON c FOR EACH ROW EXECUTE FUNCTION note();
         CREATE TRIGGER c_stmt AFTER UPDATE ON c FOR EACH STATEMENT EXECUTE FUNCTION note();
         INSERT INTO p VALUES (1); INSERT INTO c VALUES (2);
         UPDATE p SET id = id + 10",
    )
    .expect("schema");
    let mut log = rows(&mut c, "SELECT what FROM log");
    log.sort();
    assert_eq!(log, [["c ROW UPDATE"], ["p ROW UPDATE"], ["p STATEMENT UPDATE"]]);
}

#[test]
fn alter_table_recurses() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int NOT NULL, b text);
         CREATE TABLE c (x int) INHERITS (p);
         CREATE TABLE g () INHERITS (c);
         INSERT INTO c VALUES (1, 'one', 2)",
    )
    .expect("schema");
    c.batch_execute("ALTER TABLE p ADD COLUMN d int DEFAULT 5").expect("add column");
    assert_eq!(rows(&mut c, "SELECT d::text FROM c"), [["5"]]);
    c.batch_execute("ALTER TABLE c ADD COLUMN e int").expect("add to child");
    assert_eq!(notices(&db, &mut c, "ALTER TABLE p ADD COLUMN e int"), ["merging definition of column \"e\" for child \"c\""]);
    let atts = "SELECT attrelid::regclass::text || '.' || attname || ' ' || attislocal || ' ' || attinhcount FROM pg_attribute
        WHERE attrelid IN ('c'::regclass, 'g'::regclass) AND attnum > 0 AND NOT attisdropped ORDER BY attrelid::regclass::text, attnum";
    assert_eq!(
        rows(&mut c, atts).concat(),
        [
            "c.a false 1",
            "c.b false 1",
            "c.x true 0",
            "c.d false 1",
            "c.e true 1",
            "g.a false 1",
            "g.b false 1",
            "g.x false 1",
            "g.d false 1",
            "g.e false 1"
        ]
    );
    let e = expect_code(c.batch_execute("ALTER TABLE c DROP COLUMN a"), "42P16");
    assert_eq!(message(&e), "cannot drop inherited column \"a\"");
    let e = expect_code(c.batch_execute("ALTER TABLE c RENAME COLUMN a TO z"), "42P16");
    assert_eq!(message(&e), "cannot rename inherited column \"a\"");
    let e = expect_code(c.batch_execute("ALTER TABLE c ALTER COLUMN a TYPE bigint"), "42P16");
    assert_eq!(message(&e), "cannot alter inherited column \"a\"");
    let e = expect_code(c.batch_execute("ALTER TABLE c ALTER COLUMN a DROP NOT NULL"), "42P16");
    assert_eq!(message(&e), "cannot drop inherited constraint \"p_a_not_null\" of relation \"c\"");
    let e = expect_code(c.batch_execute("ALTER TABLE ONLY p ADD COLUMN z int"), "42P16");
    assert_eq!(message(&e), "column must be added to child tables too");
    let e = expect_code(c.batch_execute("ALTER TABLE ONLY p RENAME COLUMN b TO bb"), "42P16");
    assert_eq!(message(&e), "inherited column \"b\" must be renamed in child tables too");
    c.batch_execute("ALTER TABLE p RENAME COLUMN b TO bb").expect("rename recurses");
    assert_eq!(rows(&mut c, "SELECT bb FROM g UNION ALL SELECT bb FROM c"), [["one"]]);
    c.batch_execute("ALTER TABLE ONLY p DROP COLUMN d").expect("drop only");
    c.batch_execute("ALTER TABLE p DROP COLUMN e").expect("drop recurses");
    assert_eq!(
        rows(&mut c, atts).concat(),
        [
            "c.a false 1",
            "c.bb false 1",
            "c.x true 0",
            "c.d true 0",
            "c.e true 0",
            "g.a false 1",
            "g.bb false 1",
            "g.x false 1",
            "g.d false 1",
            "g.e false 1"
        ]
    );
    c.batch_execute("ALTER TABLE p ALTER COLUMN bb SET DEFAULT 'dflt'; INSERT INTO g (a) VALUES (7)").expect("set default recurses");
    assert_eq!(rows(&mut c, "SELECT bb FROM g"), [["dflt"]]);
    c.batch_execute("ALTER TABLE p ALTER COLUMN a TYPE bigint").expect("type recurses");
    assert_eq!(
        rows(
            &mut c,
            "SELECT attrelid::regclass::text, atttypid::regtype::text FROM pg_attribute WHERE attname = 'a' AND attrelid IN ('p'::regclass, 'g'::regclass) ORDER BY 1"
        ),
        [["g", "bigint"], ["p", "bigint"]]
    );
}

#[test]
fn constraints_recurse() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int NOT NULL, b text);
         CREATE TABLE c () INHERITS (p);
         CREATE TABLE g () INHERITS (c)",
    )
    .expect("schema");
    c.batch_execute("ALTER TABLE p ADD CONSTRAINT ck CHECK (a > 0)").expect("add check");
    let e = expect_code(c.batch_execute("INSERT INTO g VALUES (0)"), "23514");
    assert_eq!(message(&e), "new row for relation \"g\" violates check constraint \"ck\"");
    let e = expect_code(c.batch_execute("ALTER TABLE ONLY p ADD CONSTRAINT ck2 CHECK (a > 0)"), "42P16");
    assert_eq!(message(&e), "constraint must be added to child tables too");
    c.batch_execute("ALTER TABLE ONLY p ADD CONSTRAINT ck3 CHECK (a > 1) NO INHERIT").expect("no inherit");
    c.batch_execute("ALTER TABLE p ALTER COLUMN b SET NOT NULL").expect("set not null recurses");
    let e = expect_code(c.batch_execute("ALTER TABLE c DROP CONSTRAINT ck"), "42P16");
    assert_eq!(message(&e), "cannot drop inherited constraint \"ck\" of relation \"c\"");
    let e = expect_code(c.batch_execute("ALTER TABLE c RENAME CONSTRAINT ck TO ck9"), "42P16");
    assert_eq!(message(&e), "cannot rename inherited constraint \"ck\"");
    let e = expect_code(c.batch_execute("ALTER TABLE c ALTER COLUMN b DROP NOT NULL"), "42P16");
    assert_eq!(message(&e), "cannot drop inherited constraint \"p_b_not_null\" of relation \"c\"");
    assert_eq!(
        rows(&mut c, CONS),
        [
            ["c", "ck", "c", "false", "1", "false"],
            ["c", "p_a_not_null", "n", "false", "1", "false"],
            ["c", "p_b_not_null", "n", "false", "1", "false"],
            ["g", "ck", "c", "false", "1", "false"],
            ["g", "p_a_not_null", "n", "false", "1", "false"],
            ["g", "p_b_not_null", "n", "false", "1", "false"],
            ["p", "ck", "c", "true", "0", "false"],
            ["p", "ck3", "c", "true", "0", "true"],
            ["p", "p_a_not_null", "n", "true", "0", "false"],
            ["p", "p_b_not_null", "n", "true", "0", "false"],
        ]
    );
    c.batch_execute("ALTER TABLE p RENAME CONSTRAINT ck TO ckr").expect("rename recurses");
    c.batch_execute("ALTER TABLE p DROP CONSTRAINT ckr").expect("drop recurses");
    c.batch_execute("ALTER TABLE ONLY p ALTER COLUMN a DROP NOT NULL").expect("drop not null only");
    assert_eq!(
        rows(&mut c, CONS),
        [
            ["c", "p_a_not_null", "n", "true", "0", "false"],
            ["c", "p_b_not_null", "n", "false", "1", "false"],
            ["g", "p_a_not_null", "n", "false", "1", "false"],
            ["g", "p_b_not_null", "n", "false", "1", "false"],
            ["p", "ck3", "c", "true", "0", "true"],
            ["p", "p_b_not_null", "n", "true", "0", "false"],
        ]
    );
    c.batch_execute("ALTER TABLE c ADD CONSTRAINT m CHECK (a <> 5)").expect("child check");
    assert_eq!(
        notices(&db, &mut c, "ALTER TABLE p ADD CONSTRAINT m CHECK (a <> 5)"),
        ["merging constraint \"m\" with inherited definition"]
    );
    c.batch_execute("ALTER TABLE g ADD CONSTRAINT g_ck CHECK (a > 100)").expect("grandchild check");
    let e = expect_code(c.batch_execute("ALTER TABLE p ADD CONSTRAINT g_ck CHECK (a > 0)"), "42710");
    assert_eq!(message(&e), "constraint \"g_ck\" for relation \"g\" already exists");
}

#[test]
fn alter_inherit_and_no_inherit() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int NOT NULL, b text, CONSTRAINT ck CHECK (a > 0));
         CREATE TABLE c () INHERITS (p);
         CREATE TABLE z1 (q int);
         CREATE TABLE z2 (a text, b text);
         CREATE TABLE z3 (a int, b text);
         CREATE TABLE z4 (a int NOT NULL, b text);
         CREATE TABLE z5 (a int NOT NULL, b text, CONSTRAINT ck CHECK (a > 0), extra int);
         INSERT INTO z5 VALUES (4, 'z5', 0)",
    )
    .expect("schema");
    let e = expect_code(c.batch_execute("ALTER TABLE p INHERIT c"), "42P07");
    assert_eq!(message(&e), "circular inheritance not allowed");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("\"c\" is already a child of \"p\"."));
    let e = expect_code(c.batch_execute("ALTER TABLE c INHERIT p"), "42P07");
    assert_eq!(message(&e), "relation \"p\" would be inherited from more than once");
    let e = expect_code(c.batch_execute("ALTER TABLE z1 INHERIT p"), "42804");
    assert_eq!(message(&e), "child table is missing column \"a\"");
    let e = expect_code(c.batch_execute("ALTER TABLE z2 INHERIT p"), "42804");
    assert_eq!(message(&e), "child table \"z2\" has different type for column \"a\"");
    let e = expect_code(c.batch_execute("ALTER TABLE z3 INHERIT p"), "42804");
    assert_eq!(message(&e), "column \"a\" in child table \"z3\" must be marked NOT NULL");
    let e = expect_code(c.batch_execute("ALTER TABLE z4 INHERIT p"), "42804");
    assert_eq!(message(&e), "child table is missing constraint \"ck\"");
    c.batch_execute("ALTER TABLE z5 INHERIT p").expect("inherit");
    assert_eq!(rows(&mut c, "SELECT tableoid::regclass::text, a::text FROM p"), [["z5", "4"]]);
    let state = "SELECT attname || ' ' || attislocal || ' ' || attinhcount FROM pg_attribute WHERE attrelid = 'z5'::regclass AND attnum > 0 ORDER BY attnum";
    assert_eq!(rows(&mut c, state).concat(), ["a true 1", "b true 1", "extra true 0"]);
    let cons =
        "SELECT conname || ' ' || conislocal || ' ' || coninhcount FROM pg_constraint WHERE conrelid = 'z5'::regclass ORDER BY conname";
    assert_eq!(rows(&mut c, cons).concat(), ["ck true 1", "z5_a_not_null true 1"]);
    let e = expect_code(c.batch_execute("ALTER TABLE z5 DROP COLUMN a"), "42P16");
    assert_eq!(message(&e), "cannot drop inherited column \"a\"");
    c.batch_execute("ALTER TABLE c NO INHERIT p; ALTER TABLE z5 NO INHERIT p").expect("no inherit");
    assert_eq!(rows(&mut c, state).concat(), ["a true 0", "b true 0", "extra true 0"]);
    assert_eq!(rows(&mut c, cons).concat(), ["ck true 0", "z5_a_not_null true 0"]);
    let atts = "SELECT attname || ' ' || attislocal || ' ' || attinhcount FROM pg_attribute WHERE attrelid = 'c'::regclass AND attnum > 0 ORDER BY attnum";
    assert_eq!(rows(&mut c, atts).concat(), ["a true 0", "b true 0"]);
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM p"), [["0"]]);
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM pg_inherits"), [["0"]]);
    // relhassubclass stays set until VACUUM, as in Postgres.
    assert_eq!(rows(&mut c, "SELECT relhassubclass::text FROM pg_class WHERE relname = 'p'"), [["true"]]);
    let e = expect_code(c.batch_execute("ALTER TABLE c NO INHERIT p"), "42P01");
    assert_eq!(message(&e), "relation \"p\" is not a parent of relation \"c\"");
}

#[test]
fn drop_parent_needs_cascade() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int);
         CREATE TABLE c () INHERITS (p);
         CREATE TABLE m () INHERITS (p)",
    )
    .expect("schema");
    let e = expect_code(c.batch_execute("DROP TABLE p"), "2BP01");
    assert_eq!(message(&e), "cannot drop table p because other objects depend on it");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("table c depends on table p\ntable m depends on table p"));
    assert_eq!(e.as_db_error().and_then(|d| d.hint()), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    c.batch_execute("DROP TABLE m").expect("drop child");
    assert_eq!(notices(&db, &mut c, "DROP TABLE p CASCADE"), ["drop cascades to table c"]);
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM pg_class WHERE relname IN ('p', 'c')"), [["0"]]);
    c.batch_execute("CREATE TABLE p (a int); CREATE TABLE c () INHERITS (p); DROP TABLE p, c").expect("drop both");
}

#[test]
fn view_over_only_parent() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int);
         CREATE TABLE c () INHERITS (p);
         INSERT INTO p VALUES (1); INSERT INTO c VALUES (2);
         CREATE VIEW vo AS SELECT a FROM ONLY p;
         CREATE VIEW va AS SELECT a FROM p",
    )
    .expect("schema");
    assert_eq!(rows(&mut c, "SELECT pg_get_viewdef('vo')"), [[" SELECT a\n   FROM ONLY p;"]]);
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM vo"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM va"), [["2"]]);
}

/// This backend's relation locks after `sql` in a fresh transaction, as "relation mode".
fn locks_of(c: &mut postgres::Client, sql: &str) -> Vec<String> {
    c.batch_execute("BEGIN").expect("BEGIN");
    c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let out = texts(
        c,
        "SELECT l FROM (SELECT relation::regclass::text || ' ' || mode AS l FROM pg_locks \
         WHERE pid = pg_backend_pid() AND locktype = 'relation' AND relation <> 'pg_locks'::regclass) x ORDER BY l COLLATE \"C\"",
    )
    .into_iter()
    .map(|r| r[0].clone().unwrap_or_default())
    .collect();
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    out
}

#[test]
fn statements_on_a_parent_lock_its_children() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE lp (a int PRIMARY KEY); CREATE TABLE lc (b int) INHERITS (lp); CREATE INDEX lc_b ON lc (b)")
        .expect("tables");
    assert_eq!(
        locks_of(&mut c, "SELECT * FROM lp"),
        ["lc AccessShareLock", "lc_b AccessShareLock", "lp AccessShareLock", "lp_pkey AccessShareLock"]
    );
    assert_eq!(locks_of(&mut c, "SELECT * FROM ONLY lp"), ["lp AccessShareLock", "lp_pkey AccessShareLock"]);
    assert_eq!(
        locks_of(&mut c, "UPDATE lp SET a = a"),
        ["lc RowExclusiveLock", "lc_b RowExclusiveLock", "lp RowExclusiveLock", "lp_pkey RowExclusiveLock"]
    );
    assert_eq!(locks_of(&mut c, "DELETE FROM ONLY lp"), ["lp RowExclusiveLock", "lp_pkey RowExclusiveLock"]);
    assert_eq!(locks_of(&mut c, "LOCK TABLE lp IN SHARE MODE"), ["lc ShareLock", "lp ShareLock"]);
    assert_eq!(locks_of(&mut c, "LOCK TABLE ONLY lp IN SHARE MODE"), ["lp ShareLock"]);
}

#[test]
fn parent_policies_and_privileges_cover_children() {
    let (_db, mut c) = setup();
    let role = "inh_rls_user";
    let _ = c.batch_execute(&format!("DROP OWNED BY {role}; DROP ROLE IF EXISTS {role}"));
    c.batch_execute(&format!(
        "CREATE ROLE {role};
         CREATE TABLE rp (id int, owner text); CREATE TABLE rc (x int) INHERITS (rp);
         INSERT INTO rp VALUES (1, '{role}'), (2, 'other'); INSERT INTO rc VALUES (3, '{role}', 0), (4, 'other', 0);
         ALTER TABLE rp ENABLE ROW LEVEL SECURITY; CREATE POLICY own ON rp USING (owner = current_user);
         ALTER TABLE rc ENABLE ROW LEVEL SECURITY;
         GRANT SELECT, UPDATE, DELETE ON rp TO {role}"
    ))
    .expect("setup");
    c.batch_execute(&format!("SET ROLE {role}")).expect("set role");
    // The parent's policies apply to the children's rows; no privilege on rc is needed.
    assert_eq!(rows(&mut c, "SELECT id FROM rp ORDER BY id"), [["1"], ["3"]]);
    assert_eq!(c.execute("UPDATE rp SET id = id + 10", &[]).expect("update"), 2);
    assert_eq!(c.execute("DELETE FROM rp WHERE id > 12", &[]).expect("delete"), 1);
    let e = expect_code(c.query("SELECT * FROM rc", &[]), "42501");
    assert_eq!(message(&e), "permission denied for table rc");
    c.batch_execute("RESET ROLE").expect("reset");
    assert_eq!(rows(&mut c, "SELECT id, owner FROM rp ORDER BY id"), [["2", "other"], ["4", "other"], ["11", role]]);
    c.batch_execute(&format!("DROP TABLE rp CASCADE; DROP ROLE {role}")).expect("cleanup");
}

/// `NO INHERIT` CHECK constraints: recorded (`connoinherit`, `pg_get_constraintdef`) from table-
/// and column-level CHECKs and ADD CONSTRAINT (with NOT VALID in either order), not copied to
/// children nor enforced on their rows, and Postgres' merge errors and partitioned-table error.
#[test]
fn no_inherit_checks() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (a int CHECK (a > 0) NO INHERIT, b int, CONSTRAINT pb CHECK (b > 0) NO INHERIT, CHECK (a < 1000) NO INHERIT);
         ALTER TABLE p ADD CONSTRAINT nv1 CHECK (b < 100) NO INHERIT NOT VALID;
         ALTER TABLE p ADD CONSTRAINT nv2 CHECK (b < 200) NOT VALID NO INHERIT;
         ALTER TABLE p ADD CONSTRAINT nv3 CHECK (b < 300) NOT VALID;
         ALTER TABLE p ADD CHECK (b <> 7) NO INHERIT",
    )
    .expect("schema");
    let defs = "SELECT conname::text, connoinherit::text, convalidated::text, pg_get_constraintdef(oid)
        FROM pg_constraint WHERE conrelid = 'p'::regclass ORDER BY 1";
    assert_eq!(
        rows(&mut c, defs),
        [
            ["nv1", "true", "false", "CHECK ((b < 100)) NO INHERIT NOT VALID"],
            ["nv2", "true", "false", "CHECK ((b < 200)) NO INHERIT NOT VALID"],
            ["nv3", "false", "false", "CHECK ((b < 300)) NOT VALID"],
            ["p_a_check", "true", "true", "CHECK ((a > 0)) NO INHERIT"],
            ["p_a_check1", "true", "true", "CHECK ((a < 1000)) NO INHERIT"],
            ["p_b_check", "true", "true", "CHECK ((b <> 7)) NO INHERIT"],
            ["pb", "true", "true", "CHECK ((b > 0)) NO INHERIT"],
        ]
    );
    c.batch_execute("ALTER TABLE p VALIDATE CONSTRAINT nv1").expect("validate");
    assert_eq!(
        rows(&mut c, "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'nv1'"),
        [["CHECK ((b < 100)) NO INHERIT"]]
    );
    let e = expect_code(c.batch_execute("INSERT INTO p VALUES (0, 1)"), "23514");
    assert_eq!(message(&e), "new row for relation \"p\" violates check constraint \"p_a_check\"");

    // Children (by INHERITS or ALTER TABLE ... INHERIT) get only nv3, and take rows p rejects.
    c.batch_execute(
        "CREATE TABLE c1 () INHERITS (p); CREATE TABLE c2 (a int, b int, CONSTRAINT nv3 CHECK (b < 300)); ALTER TABLE c2 INHERIT p",
    )
    .expect("children");
    let child = "SELECT conrelid::regclass::text, conname::text, connoinherit::text FROM pg_constraint
        WHERE conrelid IN ('c1'::regclass, 'c2'::regclass) ORDER BY 1, 2";
    assert_eq!(rows(&mut c, child), [["c1", "nv3", "false"], ["c2", "nv3", "false"]]);
    c.batch_execute("INSERT INTO c1 VALUES (-1, 7); INSERT INTO c2 VALUES (5000, -1)").expect("not enforced on children");
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM p"), [["2"]]);
    // ONLY is fine for a NO INHERIT check, and the children still do not get it.
    c.batch_execute(
        "ALTER TABLE ONLY p ADD CONSTRAINT o CHECK (a <> 3) NO INHERIT; ALTER TABLE p ADD CONSTRAINT r CHECK (a <> 4) NO INHERIT",
    )
    .expect("parent-only checks");
    assert_eq!(rows(&mut c, child), [["c1", "nv3", "false"], ["c2", "nv3", "false"]]);

    // Merging with a same-named constraint whose NO INHERIT differs.
    c.batch_execute("CREATE TABLE q (a int, CONSTRAINT k CHECK (a > 0))").expect("q");
    let e = expect_code(c.batch_execute("CREATE TABLE m1 (a int, CONSTRAINT k CHECK (a > 0) NO INHERIT) INHERITS (q)"), "42P17");
    assert_eq!(message(&e), "constraint \"k\" conflicts with inherited constraint on relation \"m1\"");
    let e = expect_code(c.batch_execute("CREATE TABLE m1 (CONSTRAINT k CHECK (a > 1) NO INHERIT) INHERITS (q)"), "42710");
    assert_eq!(message(&e), "constraint \"k\" for relation \"m1\" already exists");
    c.batch_execute("CREATE TABLE m2 (a int, CONSTRAINT k CHECK (a > 0) NO INHERIT)").expect("m2");
    let e = expect_code(c.batch_execute("ALTER TABLE m2 INHERIT q"), "42P17");
    assert_eq!(message(&e), "constraint \"k\" conflicts with non-inherited constraint on child table \"m2\"");
    c.batch_execute("CREATE TABLE m3 (a int, CONSTRAINT k CHECK (a > 1) NO INHERIT)").expect("m3");
    let e = expect_code(c.batch_execute("ALTER TABLE m3 INHERIT q"), "42804");
    assert_eq!(message(&e), "child table \"m3\" has different definition for check constraint \"k\"");

    c.batch_execute("CREATE TABLE p4 (a int); CREATE TABLE c4 () INHERITS (p4); ALTER TABLE c4 ADD CONSTRAINT k CHECK (a > 0) NO INHERIT")
        .expect("p4");
    let e = expect_code(c.batch_execute("ALTER TABLE p4 ADD CONSTRAINT k CHECK (a > 0)"), "42P17");
    assert_eq!(message(&e), "constraint \"k\" conflicts with non-inherited constraint on relation \"c4\"");
    let e = expect_code(c.batch_execute("ALTER TABLE p4 ADD CONSTRAINT k CHECK (a > 1)"), "42710");
    assert_eq!(message(&e), "constraint \"k\" for relation \"c4\" already exists");

    c.batch_execute("CREATE TABLE p5 (a int); CREATE TABLE c5 () INHERITS (p5); ALTER TABLE p5 ADD CONSTRAINT k CHECK (a > 0)")
        .expect("p5");
    let e = expect_code(c.batch_execute("ALTER TABLE c5 ADD CONSTRAINT k CHECK (a > 0) NO INHERIT"), "42P17");
    assert_eq!(message(&e), "constraint \"k\" conflicts with inherited constraint on relation \"c5\"");
    let e = expect_code(c.batch_execute("ALTER TABLE c5 ADD CONSTRAINT k CHECK (a > 1) NO INHERIT"), "42710");
    assert_eq!(message(&e), "constraint \"k\" for relation \"c5\" already exists");
    // The same constraint without NO INHERIT merges into the inherited one, which becomes local.
    assert_eq!(
        notices(&db, &mut c, "ALTER TABLE c5 ADD CONSTRAINT k CHECK (a > 0)"),
        ["merging constraint \"k\" with inherited definition"]
    );
    assert_eq!(
        rows(&mut c, "SELECT conislocal::text, coninhcount::text, connoinherit::text FROM pg_constraint WHERE conrelid = 'c5'::regclass"),
        [["true", "1", "false"]]
    );
    let e = expect_code(c.batch_execute("ALTER TABLE c5 ADD CONSTRAINT k CHECK (a > 0)"), "42710");
    assert_eq!(message(&e), "constraint \"k\" for relation \"c5\" already exists");

    // A NO INHERIT parent constraint leaves a same-named child constraint alone.
    c.batch_execute(
        "CREATE TABLE pn (a int, CONSTRAINT k CHECK (a > 0) NO INHERIT);
         CREATE TABLE cn1 (a int, CONSTRAINT k CHECK (a > 5) NO INHERIT) INHERITS (pn);
         CREATE TABLE cn2 (a int, CONSTRAINT k CHECK (a > 5)) INHERITS (pn)",
    )
    .expect("pn");
    assert_eq!(
        rows(
            &mut c,
            "SELECT conrelid::regclass::text, conislocal::text, coninhcount::text, connoinherit::text, pg_get_constraintdef(oid)
             FROM pg_constraint WHERE conrelid IN ('cn1'::regclass, 'cn2'::regclass) ORDER BY 1"
        ),
        [["cn1", "true", "0", "true", "CHECK ((a > 5)) NO INHERIT"], ["cn2", "true", "0", "false", "CHECK ((a > 5))"]]
    );

    // Partitioned tables take no NO INHERIT check; their partitions may.
    let e = expect_code(c.batch_execute("CREATE TABLE pt (a int, CHECK (a > 0) NO INHERIT) PARTITION BY LIST (a)"), "42P16");
    assert_eq!(message(&e), "cannot add NO INHERIT constraint to partitioned table \"pt\"");
    let e = expect_code(c.batch_execute("CREATE TABLE pt (a int CHECK (a > 0) NO INHERIT) PARTITION BY LIST (a)"), "42P16");
    assert_eq!(message(&e), "cannot add NO INHERIT constraint to partitioned table \"pt\"");
    c.batch_execute("CREATE TABLE pt (a int) PARTITION BY LIST (a)").expect("pt");
    let e = expect_code(c.batch_execute("ALTER TABLE pt ADD CONSTRAINT x CHECK (a > 0) NO INHERIT"), "42P16");
    assert_eq!(message(&e), "cannot add NO INHERIT constraint to partitioned table \"pt\"");
    let e = expect_code(c.batch_execute("ALTER TABLE ONLY pt ADD CONSTRAINT x CHECK (a > 0) NO INHERIT"), "42P16");
    assert_eq!(message(&e), "cannot add NO INHERIT constraint to partitioned table \"pt\"");
    c.batch_execute(
        "CREATE TABLE pt1 PARTITION OF pt (CONSTRAINT z CHECK (a > 0) NO INHERIT) FOR VALUES IN (1);
         CREATE TABLE pt2 PARTITION OF pt FOR VALUES IN (2);
         ALTER TABLE pt2 ADD CONSTRAINT y CHECK (a > 0) NO INHERIT",
    )
    .expect("partitions");
    assert_eq!(
        rows(
            &mut c,
            "SELECT conrelid::regclass::text, conname::text, connoinherit::text, pg_get_constraintdef(oid)
             FROM pg_constraint WHERE conrelid IN ('pt1'::regclass, 'pt2'::regclass) ORDER BY 1"
        ),
        [["pt1", "z", "true", "CHECK ((a > 0)) NO INHERIT"], ["pt2", "y", "true", "CHECK ((a > 0)) NO INHERIT"]]
    );
}
