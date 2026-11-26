//! Storage parameters: `WITH (...)` on tables, indexes, views and materialized views,
//! `ALTER ... SET / RESET (...)`, `pg_class.reloptions` and its readers.

use crate::support::{expect_code, message, setup, texts};

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
