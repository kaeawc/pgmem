//! Storage parameters: `WITH (...)` on tables, indexes, views and materialized views,
//! `ALTER ... SET / RESET (...)`, `pg_class.reloptions` and its readers.

use crate::support::{expect_code, message, setup, sqlstate, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn reloptions(c: &mut postgres::Client, rel: &str) -> String {
    rows(c, &format!("SELECT reloptions::text FROM pg_class WHERE relname = '{rel}'"))[0][0].clone()
}

#[test]
fn table_options_are_stored_in_order() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE plain (id int); CREATE TABLE t (id int) WITH (fillfactor = 70, Autovacuum_enabled = false, toast.autovacuum_enabled = off)")
        .expect("create");
    // Relations without options keep a NULL reloptions; toast.* options are validated, not kept.
    assert_eq!(reloptions(&mut c, "plain"), "NULL");
    assert_eq!(reloptions(&mut c, "t"), "{fillfactor=70,autovacuum_enabled=false}");
    // Values stay as written; a bare word is lower-cased, a bare name means true.
    c.batch_execute(
        "CREATE TABLE v (id int) WITH (fillfactor=' 70', vacuum_index_cleanup=Auto, autovacuum_enabled, autovacuum_vacuum_scale_factor=0.50)",
    )
    .expect("create");
    assert_eq!(
        reloptions(&mut c, "v"),
        "{\"fillfactor= 70\",vacuum_index_cleanup=auto,autovacuum_enabled=true,autovacuum_vacuum_scale_factor=0.50}"
    );
    // CREATE TABLE AS and unlogged / temporary tables take them too; LIKE does not copy them.
    c.batch_execute("CREATE TABLE a WITH (fillfactor = 50) AS SELECT 1 AS x; CREATE TABLE l (LIKE t INCLUDING ALL)").expect("ctas");
    assert_eq!(reloptions(&mut c, "a"), "{fillfactor=50}");
    assert_eq!(reloptions(&mut c, "l"), "NULL");
    c.batch_execute("CREATE TABLE o (id int) WITH (oids = false)").expect("oids=false is accepted");
    assert_eq!(reloptions(&mut c, "o"), "NULL");
}

#[test]
fn table_option_errors() {
    let (_db, mut c) = setup();
    let cases: [(&str, &str, &str); 13] = [
        ("CREATE TABLE e (id int) WITH (foo = 1)", "22023", "unrecognized parameter \"foo\""),
        ("CREATE TABLE e (id int) WITH (\"Fillfactor\" = 50)", "22023", "unrecognized parameter \"Fillfactor\""),
        ("CREATE TABLE e (id int) WITH (ns.fillfactor = 70)", "22023", "unrecognized parameter namespace \"ns\""),
        ("CREATE TABLE e (id int) WITH (toast.fillfactor = 70)", "22023", "unrecognized parameter \"fillfactor\""),
        ("CREATE TABLE e (id int) WITH (fillfactor = 5)", "22023", "value 5 out of bounds for option \"fillfactor\""),
        ("CREATE TABLE e (id int) WITH (fillfactor = 9.4)", "22023", "value 9.4 out of bounds for option \"fillfactor\""),
        ("CREATE TABLE e (id int) WITH (fillfactor = abc)", "22023", "invalid value for integer option \"fillfactor\": abc"),
        ("CREATE TABLE e (id int) WITH (fillfactor)", "22023", "invalid value for integer option \"fillfactor\": true"),
        ("CREATE TABLE e (id int) WITH (fillfactor = 70, fillfactor = 80)", "22023", "parameter \"fillfactor\" specified more than once"),
        (
            "CREATE TABLE e (id int) WITH (autovacuum_enabled = maybe)",
            "22023",
            "invalid value for boolean option \"autovacuum_enabled\": maybe",
        ),
        (
            "CREATE TABLE e (id int) WITH (autovacuum_vacuum_scale_factor = abc)",
            "22023",
            "invalid value for floating point option \"autovacuum_vacuum_scale_factor\": abc",
        ),
        (
            "CREATE TABLE e (id int) WITH (vacuum_index_cleanup = bad)",
            "22023",
            "invalid value for enum option \"vacuum_index_cleanup\": bad",
        ),
        ("CREATE TABLE e (id int) WITH (oids = true)", "0A000", "tables declared WITH OIDS are not supported"),
    ];
    for (sql, code, msg) in cases {
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    }
    let e = expect_code(c.batch_execute("CREATE TABLE e (id int) WITH (fillfactor = 500)"), "22023");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("Valid values are between \"10\" and \"100\"."));
    let e = expect_code(c.batch_execute("CREATE TABLE e (id int) WITH (autovacuum_vacuum_scale_factor = 101)"), "22023");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("Valid values are between \"0.000000\" and \"100.000000\"."));
    let e = expect_code(c.batch_execute("CREATE TABLE e (id int) WITH (vacuum_index_cleanup = bad)"), "22023");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("Valid values are \"on\", \"off\", and \"auto\"."));
    // Integers accept decimals (rounded half to even) and a bare number's exponent.
    c.batch_execute("CREATE TABLE ok1 (id int) WITH (fillfactor = 9.5); CREATE TABLE ok2 (id int) WITH (fillfactor = 100.5)")
        .expect("rounds");
}

#[test]
fn alter_table_set_and_reset() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int) WITH (fillfactor = 70, autovacuum_enabled = false)").expect("create");
    // SET drops the old entry of the same name and appends the new ones.
    c.batch_execute("ALTER TABLE t SET (fillfactor = 80, parallel_workers = 2)").expect("set");
    assert_eq!(reloptions(&mut c, "t"), "{autovacuum_enabled=false,fillfactor=80,parallel_workers=2}");
    c.batch_execute("ALTER TABLE t RESET (fillfactor, nonsense, toast.whatever)").expect("reset ignores what is not set");
    assert_eq!(reloptions(&mut c, "t"), "{autovacuum_enabled=false,parallel_workers=2}");
    c.batch_execute("ALTER TABLE t RESET (autovacuum_enabled, parallel_workers)").expect("reset all");
    assert_eq!(reloptions(&mut c, "t"), "NULL");
    c.batch_execute("ALTER TABLE t SET (toast.autovacuum_enabled = false)").expect("toast options");
    assert_eq!(reloptions(&mut c, "t"), "NULL");
    let e = expect_code(c.batch_execute("ALTER TABLE t SET (security_barrier = true)"), "22023");
    assert_eq!(message(&e), "unrecognized parameter \"security_barrier\"");
    let e = expect_code(c.batch_execute("ALTER TABLE t SET (foo.bar = 1)"), "22023");
    assert_eq!(message(&e), "unrecognized parameter namespace \"foo\"");
    let e = expect_code(c.batch_execute("ALTER TABLE t SET (fillfactor = 60, fillfactor = 60)"), "22023");
    assert_eq!(message(&e), "parameter \"fillfactor\" specified more than once");
    let e = expect_code(c.batch_execute("ALTER TABLE nonexistent SET (fillfactor = 60)"), "42P01");
    assert_eq!(message(&e), "relation \"nonexistent\" does not exist");
    c.batch_execute("CREATE TABLE p (id int) PARTITION BY RANGE (id); ALTER TABLE p RESET (fillfactor)").expect("reset on partitioned");
    let e = expect_code(c.batch_execute("ALTER TABLE p SET (fillfactor = 50)"), "42809");
    assert_eq!(message(&e), "cannot specify storage parameters for a partitioned table");
    // Materialized views take table options and are altered with either keyword.
    c.batch_execute(
        "CREATE MATERIALIZED VIEW m WITH (fillfactor = 50) AS SELECT 1 AS x;
         ALTER MATERIALIZED VIEW m SET (fillfactor = 60, autovacuum_enabled = false);
         ALTER TABLE m SET (parallel_workers = 3)",
    )
    .expect("matview");
    assert_eq!(reloptions(&mut c, "m"), "{fillfactor=60,autovacuum_enabled=false,parallel_workers=3}");
    let e = expect_code(c.batch_execute("CREATE MATERIALIZED VIEW m2 WITH (check_option = local) AS SELECT 1"), "22023");
    assert_eq!(message(&e), "unrecognized parameter \"check_option\"");
    let e = expect_code(c.batch_execute("ALTER MATERIALIZED VIEW t SET (fillfactor = 60)"), "42809");
    assert_eq!(message(&e), "\"t\" is not a materialized view");
}

#[test]
fn index_options() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int, name text)").expect("create");
    c.batch_execute(
        "CREATE INDEX i1 ON t (id) WITH (fillfactor = 60, deduplicate_items = on);
         CREATE INDEX i2 ON t (id) WITH (FILLFACTOR = 60, Deduplicate_items = 'off') WHERE id > 3;
         CREATE UNIQUE INDEX i3 ON t (name) WITH (fillfactor = 80);
         CREATE INDEX i4 ON t USING hash (id) WITH (fillfactor = 70);
         CREATE INDEX plain ON t (name)",
    )
    .expect("create indexes");
    assert_eq!(reloptions(&mut c, "i1"), "{fillfactor=60,deduplicate_items=on}");
    assert_eq!(reloptions(&mut c, "i2"), "{fillfactor=60,deduplicate_items=off}");
    assert_eq!(reloptions(&mut c, "plain"), "NULL");
    // LIKE ... INCLUDING INDEXES copies an index's options.
    c.batch_execute("CREATE TABLE copy (LIKE t INCLUDING INDEXES)").expect("like");
    assert_eq!(reloptions(&mut c, "copy_id_idx"), "{fillfactor=60,deduplicate_items=on}");
    // pg_get_indexdef: after the columns, before WHERE; a value is bare only when it needs no quoting.
    assert_eq!(
        rows(&mut c, "SELECT pg_get_indexdef('i1'::regclass), pg_get_indexdef('i2'::regclass), pg_get_indexdef('i3'::regclass)"),
        [[
            "CREATE INDEX i1 ON public.t USING btree (id) WITH (fillfactor='60', deduplicate_items='on')",
            "CREATE INDEX i2 ON public.t USING btree (id) WITH (fillfactor='60', deduplicate_items=off) WHERE (id > 3)",
            "CREATE UNIQUE INDEX i3 ON public.t USING btree (name) WITH (fillfactor='80')",
        ]]
    );
    assert_eq!(
        rows(&mut c, "SELECT indexdef FROM pg_indexes WHERE indexname = 'i3'"),
        [["CREATE UNIQUE INDEX i3 ON public.t USING btree (name) WITH (fillfactor='80')"]]
    );
    c.batch_execute("ALTER INDEX i1 SET (fillfactor = 70, deduplicate_items = off)").expect("alter index");
    assert_eq!(reloptions(&mut c, "i1"), "{fillfactor=70,deduplicate_items=off}");
    c.batch_execute("ALTER TABLE i1 RESET (deduplicate_items); ALTER INDEX IF EXISTS i1 RESET (fillfactor, nonsense)").expect("reset");
    assert_eq!(reloptions(&mut c, "i1"), "NULL");
    let n = c.simple_query("ALTER INDEX IF EXISTS nonexistent SET (fillfactor = 40)").expect("if exists");
    assert!(n.iter().any(|m| matches!(m, postgres::SimpleQueryMessage::CommandComplete(_))));
    let cases: [(&str, &str, &str); 8] = [
        ("CREATE INDEX e ON t (id) WITH (foo = 1)", "22023", "unrecognized parameter \"foo\""),
        ("CREATE INDEX e ON t (id) WITH (fillfactor = 5)", "22023", "value 5 out of bounds for option \"fillfactor\""),
        ("CREATE INDEX e ON t (id) WITH (fillfactor)", "22023", "invalid value for integer option \"fillfactor\": true"),
        ("CREATE INDEX e ON t (id) WITH (toast.fillfactor = 20)", "22023", "unrecognized parameter namespace \"toast\""),
        ("CREATE INDEX e ON t USING hash (id) WITH (deduplicate_items = off)", "22023", "unrecognized parameter \"deduplicate_items\""),
        ("ALTER INDEX i1 SET (autovacuum_enabled = true)", "22023", "unrecognized parameter \"autovacuum_enabled\""),
        ("ALTER INDEX t SET (fillfactor = 40)", "42809", "\"t\" is not an index"),
        ("ALTER INDEX i1 SET (deduplicate_items = bad)", "22023", "invalid value for boolean option \"deduplicate_items\": bad"),
    ];
    for (sql, code, msg) in cases {
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    }
}

#[test]
fn view_options() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (id int PRIMARY KEY, n int);
         CREATE VIEW v1 WITH (security_barrier, security_invoker = true) AS SELECT * FROM t;
         CREATE VIEW v2 WITH (check_option = CASCADED, security_barrier = 1) AS SELECT * FROM t WHERE n > 0;
         CREATE VIEW v3 AS SELECT * FROM t WHERE n > 0 WITH LOCAL CHECK OPTION;
         CREATE VIEW v4 AS SELECT * FROM t WHERE n > 0 WITH CHECK OPTION;
         CREATE VIEW v5 AS SELECT * FROM t",
    )
    .expect("create");
    assert_eq!(reloptions(&mut c, "v1"), "{security_barrier=true,security_invoker=true}");
    assert_eq!(reloptions(&mut c, "v2"), "{check_option=cascaded,security_barrier=1}");
    assert_eq!(reloptions(&mut c, "v3"), "{check_option=local}");
    assert_eq!(reloptions(&mut c, "v4"), "{check_option=cascaded}");
    assert_eq!(reloptions(&mut c, "v5"), "NULL");
    assert_eq!(
        rows(&mut c, "SELECT table_name, check_option FROM information_schema.views WHERE table_schema = 'public' ORDER BY 1"),
        [["v1", "NONE"], ["v2", "CASCADED"], ["v3", "LOCAL"], ["v4", "CASCADED"], ["v5", "NONE"]]
    );
    // The check option is enforced when set through the options too.
    c.batch_execute("CREATE VIEW v6 WITH (check_option = local) AS SELECT * FROM t WHERE n > 0").expect("create");
    let e = expect_code(c.batch_execute("INSERT INTO v6 VALUES (1, -1)"), "44000");
    assert_eq!(message(&e), "new row violates check option for view \"v6\"");
    // ALTER VIEW (and ALTER TABLE on a view): the old entries not mentioned come first.
    c.batch_execute("ALTER VIEW v1 SET (security_barrier = false, check_option = local)").expect("alter");
    assert_eq!(reloptions(&mut c, "v1"), "{security_invoker=true,security_barrier=false,check_option=local}");
    c.batch_execute("ALTER TABLE v1 SET (security_invoker = off); ALTER VIEW v1 RESET (check_option, security_barrier)").expect("alter");
    assert_eq!(reloptions(&mut c, "v1"), "{security_invoker=off}");
    assert_eq!(rows(&mut c, "SELECT check_option FROM information_schema.views WHERE table_name = 'v1'"), [["NONE"]]);
    c.batch_execute("ALTER VIEW v4 SET (check_option = local)").expect("alter");
    assert_eq!(reloptions(&mut c, "v4"), "{check_option=local}");
    // CREATE OR REPLACE sets the options anew.
    c.batch_execute("CREATE OR REPLACE VIEW v2 AS SELECT * FROM t WHERE n > 0").expect("replace");
    assert_eq!(reloptions(&mut c, "v2"), "NULL");
    let cases: [(&str, &str, &str); 8] = [
        ("CREATE VIEW e WITH (fillfactor = 50) AS SELECT * FROM t", "22023", "unrecognized parameter \"fillfactor\""),
        ("CREATE VIEW e WITH (check_option = bad) AS SELECT * FROM t", "22023", "invalid value for enum option \"check_option\": bad"),
        (
            "CREATE VIEW e WITH (security_barrier = maybe) AS SELECT * FROM t",
            "22023",
            "invalid value for boolean option \"security_barrier\": maybe",
        ),
        ("CREATE VIEW e WITH (foo.bar = 1) AS SELECT * FROM t", "22023", "unrecognized parameter namespace \"foo\""),
        (
            "CREATE VIEW e WITH (check_option = local) AS SELECT * FROM t WITH CHECK OPTION",
            "22023",
            "parameter \"check_option\" specified more than once",
        ),
        ("ALTER VIEW v1 SET (fillfactor = 1)", "22023", "unrecognized parameter \"fillfactor\""),
        ("ALTER VIEW t SET (security_barrier = true)", "42809", "\"t\" is not a view"),
        ("ALTER TABLE t SET (security_barrier = true)", "22023", "unrecognized parameter \"security_barrier\""),
    ];
    for (sql, code, msg) in cases {
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    }
}

fn hint(e: &postgres::Error) -> Option<&str> {
    e.as_db_error().and_then(|d| d.hint())
}

#[test]
fn partition_by_comes_before_with() {
    let (_db, mut c) = setup();
    // Postgres' grammar puts PARTITION BY first; a partitioned table takes no options.
    let e = expect_code(c.batch_execute("CREATE TABLE p (a int) PARTITION BY RANGE (a) WITH (fillfactor = 70)"), "42809");
    assert_eq!(message(&e), "cannot specify storage parameters for a partitioned table");
    assert_eq!(hint(&e), Some("Specify storage parameters for its leaf partitions instead."));
    let e = expect_code(c.batch_execute("CREATE TABLE p (a int) WITH (fillfactor = 70) PARTITION BY RANGE (a)"), "42601");
    assert_eq!(message(&e), "syntax error at or near \"PARTITION\"");
    // Leaf partitions take options, also when they are partitioned in turn (hash bounds use WITH too).
    c.batch_execute(
        "CREATE TABLE p (a int) PARTITION BY HASH (a);
         CREATE TABLE p0 PARTITION OF p FOR VALUES WITH (modulus 2, remainder 0) PARTITION BY RANGE (a);
         CREATE TABLE p1 PARTITION OF p FOR VALUES WITH (modulus 2, remainder 1) WITH (fillfactor = 70)",
    )
    .expect("partitions");
    assert_eq!(reloptions(&mut c, "p1"), "{fillfactor=70}");
    assert_eq!(reloptions(&mut c, "p0"), "NULL");
}

#[test]
fn constraint_index_options() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (
            a int,
            b int UNIQUE WITH (fillfactor = 60),
            c int,
            d int PRIMARY KEY WITH (fillfactor = 70),
            e int,
            CONSTRAINT u_c UNIQUE (c) WITH (fillfactor = 0x32) USING INDEX TABLESPACE pg_default,
            CONSTRAINT ex_e EXCLUDE USING btree (e WITH =) WITH (fillfactor = 40) WHERE (a > 0)
        )",
    )
    .expect("create");
    assert_eq!(reloptions(&mut c, "t_b_key"), "{fillfactor=60}");
    assert_eq!(reloptions(&mut c, "t_pkey"), "{fillfactor=70}");
    assert_eq!(reloptions(&mut c, "u_c"), "{fillfactor=50}");
    assert_eq!(reloptions(&mut c, "ex_e"), "{fillfactor=40}");
    assert_eq!(
        rows(&mut c, "SELECT pg_get_indexdef('t_pkey'::regclass), pg_get_indexdef('u_c'::regclass)"),
        [[
            "CREATE UNIQUE INDEX t_pkey ON public.t USING btree (d) WITH (fillfactor='70')",
            "CREATE UNIQUE INDEX u_c ON public.t USING btree (c) WITH (fillfactor='50')",
        ]]
    );
    // An exclusion constraint prints its options (a key constraint's definition does not).
    assert_eq!(
        rows(
            &mut c,
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 't'::regclass AND contype IN ('x', 'p', 'u') ORDER BY 1"
        ),
        [
            ["ex_e", "EXCLUDE USING btree (e WITH =) WITH (fillfactor='40') WHERE ((a > 0))"],
            ["t_b_key", "UNIQUE (b)"],
            ["t_pkey", "PRIMARY KEY (d)"],
            ["u_c", "UNIQUE (c)"],
        ]
    );
    // ALTER TABLE ... ADD, table- and column-level.
    c.batch_execute(
        "CREATE TABLE a1 (x int, y int, z int);
         ALTER TABLE a1 ADD CONSTRAINT a1_x UNIQUE (x) WITH (fillfactor = 33);
         ALTER TABLE a1 ADD PRIMARY KEY (y) WITH (fillfactor = 22);
         ALTER TABLE a1 ADD COLUMN w int UNIQUE WITH (fillfactor = 44)",
    )
    .expect("alter");
    assert_eq!(reloptions(&mut c, "a1_x"), "{fillfactor=33}");
    assert_eq!(reloptions(&mut c, "a1_pkey"), "{fillfactor=22}");
    assert_eq!(reloptions(&mut c, "a1_w_key"), "{fillfactor=44}");
    // Without options the indexes have none.
    c.batch_execute("CREATE TABLE n (a int PRIMARY KEY, b int, UNIQUE (b))").expect("create");
    assert_eq!(reloptions(&mut c, "n_pkey"), "NULL");
    assert_eq!(reloptions(&mut c, "n_b_key"), "NULL");
    let cases: [(&str, &str, &str); 3] = [
        ("CREATE TABLE e (a int PRIMARY KEY WITH (fillfactor = 5))", "22023", "value 5 out of bounds for option \"fillfactor\""),
        ("CREATE TABLE e (a int, UNIQUE (a) WITH (nope = 5))", "22023", "unrecognized parameter \"nope\""),
        (
            "CREATE TABLE e (a int, EXCLUDE USING btree (a WITH =) WITH (deduplicate_items = maybe))",
            "22023",
            "invalid value for boolean option \"deduplicate_items\": maybe",
        ),
    ];
    for (sql, code, msg) in cases {
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    }
}

#[test]
fn alter_table_combines_option_actions() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int, b int); ALTER TABLE t SET (autovacuum_vacuum_scale_factor = 5.)").expect("create");
    c.batch_execute("ALTER TABLE t SET (fillfactor = 50), ADD COLUMN c int").expect("set + add");
    assert_eq!(reloptions(&mut c, "t"), "{autovacuum_vacuum_scale_factor=5.,fillfactor=50}");
    c.batch_execute("ALTER TABLE t ADD COLUMN d int, RESET (fillfactor)").expect("add + reset");
    assert_eq!(reloptions(&mut c, "t"), "{autovacuum_vacuum_scale_factor=5.}");
    // The actions apply in order, each on the result of the one before.
    c.batch_execute("ALTER TABLE t SET (fillfactor = 50), RESET (fillfactor), SET (parallel_workers = 2), SET (fillfactor = 60)")
        .expect("sequence");
    assert_eq!(reloptions(&mut c, "t"), "{autovacuum_vacuum_scale_factor=5.,parallel_workers=2,fillfactor=60}");
    c.batch_execute("ALTER TABLE t SET (fillfactor = 70), DROP COLUMN b, ALTER COLUMN a TYPE bigint").expect("drop and alter");
    assert_eq!(reloptions(&mut c, "t"), "{autovacuum_vacuum_scale_factor=5.,parallel_workers=2,fillfactor=70}");
    // A failing action undoes the others.
    let e = expect_code(c.batch_execute("ALTER TABLE t SET (fillfactor = 500), ADD COLUMN e int"), "22023");
    assert_eq!(message(&e), "value 500 out of bounds for option \"fillfactor\"");
    let e = expect_code(c.batch_execute("ALTER TABLE t SET (nope = 1), ADD COLUMN e int"), "22023");
    assert_eq!(message(&e), "unrecognized parameter \"nope\"");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_attribute WHERE attrelid = 't'::regclass AND attname = 'e'"), [["0"]]);
    c.batch_execute("CREATE TABLE p (a int) PARTITION BY RANGE (a)").expect("create");
    let e = expect_code(c.batch_execute("ALTER TABLE p SET (fillfactor = 50), ADD COLUMN q int"), "42809");
    assert_eq!(message(&e), "cannot specify storage parameters for a partitioned table");
    c.batch_execute("ALTER TABLE p RESET (fillfactor), ADD COLUMN q int").expect("reset on a partitioned table");
}

#[test]
fn option_list_syntax() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int); CREATE INDEX i ON t (a); CREATE VIEW v AS SELECT * FROM t").expect("create");
    // RESET takes names only.
    for sql in [
        "ALTER TABLE t RESET (fillfactor = 1)",
        "ALTER TABLE t RESET (fillfactor = 1, nope)",
        "ALTER INDEX i RESET (fillfactor = 1)",
        "ALTER VIEW v RESET (security_barrier = 1)",
    ] {
        let e = expect_code(c.batch_execute(sql), "42601");
        assert_eq!(message(&e), "RESET must not include values for parameters", "{sql}");
    }
    let e = expect_code(c.batch_execute("ALTER TABLE nonexistent RESET (a = 1)"), "42P01");
    assert_eq!(message(&e), "relation \"nonexistent\" does not exist");
    // An empty list is a syntax error at its closing parenthesis.
    for sql in [
        "ALTER TABLE t SET ()",
        "ALTER TABLE t RESET ()",
        "ALTER TABLE t ALTER COLUMN a SET ()",
        "ALTER INDEX i SET ()",
        "ALTER VIEW v SET ()",
        "ALTER VIEW v RESET ()",
        "CREATE TABLE e (a int) WITH ()",
    ] {
        let e = expect_code(c.batch_execute(sql), "42601");
        assert_eq!(message(&e), "syntax error at or near \")\"", "{sql}");
    }
    // Integer literals in any base are stored in decimal; a numeric literal keeps its text.
    for (value, stored) in
        [("0x32", "50"), ("0o62", "50"), ("0b110010", "50"), ("062", "62"), ("1_0", "10"), ("1e1", "1e1"), ("'0x32'", "0x32")]
    {
        c.batch_execute(&format!("ALTER TABLE t SET (fillfactor = {value})")).expect("set");
        assert_eq!(reloptions(&mut c, "t"), format!("{{fillfactor={stored}}}"), "{value}");
    }
    c.batch_execute("ALTER TABLE t SET (autovacuum_vacuum_scale_factor = 0x10)").expect("set");
    assert_eq!(reloptions(&mut c, "t"), "{fillfactor=0x32,autovacuum_vacuum_scale_factor=16}");
}

#[test]
fn check_option_needs_an_updatable_view() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int, b int)").expect("create");
    let cases = [
        (
            "CREATE VIEW e AS SELECT a, count(*) FROM t GROUP BY a WITH CHECK OPTION",
            "Views containing GROUP BY are not automatically updatable.",
        ),
        ("CREATE VIEW e AS SELECT DISTINCT a FROM t WITH LOCAL CHECK OPTION", "Views containing DISTINCT are not automatically updatable."),
        (
            "CREATE VIEW e AS SELECT a + 1 AS x FROM t WITH CHECK OPTION",
            "Views that have no updatable columns are not automatically updatable.",
        ),
        (
            "CREATE VIEW e AS SELECT a FROM t UNION SELECT b FROM t WITH CHECK OPTION",
            "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.",
        ),
        (
            "CREATE VIEW e AS SELECT 1 AS x WITH CHECK OPTION",
            "Views that do not select from a single table or view are not automatically updatable.",
        ),
        (
            "CREATE VIEW e AS VALUES (1) WITH CHECK OPTION",
            "Views that do not select from a single table or view are not automatically updatable.",
        ),
        ("CREATE VIEW e AS SELECT a FROM t LIMIT 5 WITH CHECK OPTION", "Views containing LIMIT or OFFSET are not automatically updatable."),
        (
            "CREATE VIEW e WITH (check_option = local) AS SELECT a + 1 AS x FROM t",
            "Views that have no updatable columns are not automatically updatable.",
        ),
    ];
    for (sql, why) in cases {
        let e = expect_code(c.batch_execute(sql), "0A000");
        assert_eq!(message(&e), "WITH CHECK OPTION is supported only on automatically updatable views", "{sql}");
        assert_eq!(hint(&e), Some(why), "{sql}");
    }
    // Updatable ones, however they select columns, are fine; the option is also refused later.
    c.batch_execute(
        "CREATE VIEW ok1 AS SELECT a, b + 1 AS c FROM t WITH CHECK OPTION;
         CREATE VIEW ok2 AS SELECT a, a AS a2 FROM t WITH CHECK OPTION;
         CREATE VIEW grouped AS SELECT a, count(*) FROM t GROUP BY a",
    )
    .expect("updatable views");
    let e = expect_code(c.batch_execute("ALTER VIEW grouped SET (check_option = local)"), "0A000");
    assert_eq!(hint(&e), Some("Views containing GROUP BY are not automatically updatable."));
    c.batch_execute("ALTER VIEW grouped SET (security_barrier = true)").expect("other options are fine");
    assert_eq!(reloptions(&mut c, "grouped"), "{security_barrier=true}");
}

#[test]
fn local_and_cascaded_check_options() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (a int);
         CREATE VIEW base AS SELECT * FROM t WHERE a > 0;
         CREATE VIEW mid_local AS SELECT * FROM base WHERE a < 10 WITH LOCAL CHECK OPTION;
         CREATE VIEW mid_casc AS SELECT * FROM base WHERE a < 10 WITH CASCADED CHECK OPTION;
         CREATE VIEW top_local AS SELECT * FROM mid_local WHERE a <> 5 WITH LOCAL CHECK OPTION;
         CREATE VIEW top_none AS SELECT * FROM mid_local WHERE a <> 5;
         CREATE VIEW over_none AS SELECT * FROM base WHERE a <> 5 WITH LOCAL CHECK OPTION;
         CREATE VIEW casc_over_none AS SELECT * FROM base WHERE a <> 5 WITH CASCADED CHECK OPTION",
    )
    .expect("create");
    let violation = |c: &mut postgres::Client, view: &str, v: i32| -> Option<String> {
        match c.batch_execute(&format!("INSERT INTO {view} VALUES ({v})")) {
            Ok(()) => None,
            Err(e) => {
                assert_eq!(sqlstate(&e), "44000");
                Some(message(&e))
            }
        }
    };
    let new_row = |view: &str| Some(format!("new row violates check option for view \"{view}\""));
    // LOCAL checks the view's own condition, and the check options of the views below it.
    assert_eq!(violation(&mut c, "mid_local", -1), None);
    assert_eq!(violation(&mut c, "mid_local", 10), new_row("mid_local"));
    // CASCADED checks the conditions of the views below too, named for the view that owns them.
    assert_eq!(violation(&mut c, "mid_casc", -2), new_row("base"));
    assert_eq!(violation(&mut c, "mid_casc", 10), new_row("mid_casc"));
    assert_eq!(violation(&mut c, "top_local", -3), None);
    assert_eq!(violation(&mut c, "top_local", 5), new_row("top_local"));
    assert_eq!(violation(&mut c, "top_local", 20), new_row("mid_local"));
    assert_eq!(violation(&mut c, "top_none", -4), None);
    assert_eq!(violation(&mut c, "top_none", 5), None);
    assert_eq!(violation(&mut c, "top_none", 20), new_row("mid_local"));
    assert_eq!(violation(&mut c, "over_none", -5), None);
    assert_eq!(violation(&mut c, "over_none", 5), new_row("over_none"));
    assert_eq!(violation(&mut c, "casc_over_none", -6), new_row("base"));
    assert_eq!(violation(&mut c, "casc_over_none", 5), new_row("casc_over_none"));
    assert_eq!(violation(&mut c, "casc_over_none", 20), None);
    // UPDATE is checked the same way.
    c.batch_execute("INSERT INTO t VALUES (3)").expect("insert");
    let e = expect_code(c.batch_execute("UPDATE mid_casc SET a = -7 WHERE a = 3"), "44000");
    assert_eq!(message(&e), "new row violates check option for view \"base\"");
    let e = expect_code(c.batch_execute("UPDATE mid_local SET a = 12 WHERE a = 3"), "44000");
    assert_eq!(message(&e), "new row violates check option for view \"mid_local\"");
}

#[test]
fn security_invoker_views_run_with_the_callers_privileges() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE ROLE reloptions_invoker;
         CREATE TABLE t (a int, b int);
         INSERT INTO t VALUES (1, 2);
         CREATE VIEW definer AS SELECT * FROM t;
         CREATE VIEW invoker WITH (security_invoker = true) AS SELECT * FROM t;
         CREATE VIEW nested WITH (security_invoker = on) AS SELECT * FROM invoker;
         CREATE VIEW over_definer WITH (security_invoker = true) AS SELECT * FROM definer;
         GRANT SELECT ON definer, invoker, nested, over_definer TO reloptions_invoker",
    )
    .expect("setup");
    c.batch_execute("SET ROLE reloptions_invoker").expect("set role");
    assert_eq!(rows(&mut c, "SELECT * FROM definer"), [["1", "2"]]);
    assert_eq!(rows(&mut c, "SELECT * FROM over_definer"), [["1", "2"]]);
    for view in ["invoker", "nested"] {
        let e = expect_code(c.simple_query(&format!("SELECT * FROM {view}")), "42501");
        assert_eq!(message(&e), "permission denied for table t");
    }
    c.batch_execute("RESET ROLE; GRANT SELECT ON t TO reloptions_invoker; SET ROLE reloptions_invoker").expect("grant");
    assert_eq!(rows(&mut c, "SELECT * FROM nested"), [["1", "2"]]);
    // Switching the option takes effect, and keeps the grants.
    c.batch_execute("RESET ROLE; REVOKE SELECT ON t FROM reloptions_invoker; ALTER VIEW invoker SET (security_invoker = false); SET ROLE reloptions_invoker")
        .expect("alter");
    assert_eq!(rows(&mut c, "SELECT * FROM invoker"), [["1", "2"]]);
    c.batch_execute("RESET ROLE; ALTER VIEW definer SET (security_invoker = true); SET ROLE reloptions_invoker").expect("alter");
    let e = expect_code(c.simple_query("SELECT * FROM definer"), "42501");
    assert_eq!(message(&e), "permission denied for table t");
    c.batch_execute("RESET ROLE; DROP OWNED BY reloptions_invoker; DROP ROLE reloptions_invoker").expect("cleanup");
}
