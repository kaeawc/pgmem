//! CREATE / ALTER / DROP STATISTICS and CREATE / DROP CAST, checked against PostgreSQL 18.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

const STATS_TABLE: &str = "CREATE SCHEMA s1; CREATE TABLE s1.t (a int, b int, c text, d int[], f json)";

#[test]
fn create_statistics_catalog_rows() {
    let (_db, mut c) = setup();
    c.batch_execute(STATS_TABLE).expect("table");
    c.batch_execute(
        "CREATE STATISTICS s1.st1 (ndistinct, dependencies, mcv) ON a, b FROM s1.t;
         CREATE STATISTICS s1.st2 (mcv, ndistinct) ON a, b, c FROM s1.t;
         CREATE STATISTICS s1.st3 ON a, (a + b), c FROM s1.t;
         CREATE STATISTICS s1.st4 (dependencies) ON (a + b), (a * b) FROM s1.t;
         CREATE STATISTICS s1.st5 ON (a + b) FROM s1.t;
         CREATE STATISTICS s1.st6 ON c, a FROM s1.t",
    )
    .expect("create");
    assert_eq!(
        rows(
            &mut c,
            "SELECT stxname, stxrelid::regclass::text, stxnamespace::regnamespace::text, stxowner = (SELECT oid FROM pg_roles WHERE rolname = current_user), stxkeys, \
             stxstattarget, stxkind::text FROM pg_statistic_ext ORDER BY stxname"
        ),
        [
            ["st1", "s1.t", "s1", "t", "1 2", "NULL", "{d,f,m}"],
            ["st2", "s1.t", "s1", "t", "1 2 3", "NULL", "{d,m}"],
            ["st3", "s1.t", "s1", "t", "1 3", "NULL", "{d,f,m,e}"],
            ["st4", "s1.t", "s1", "t", "", "NULL", "{f,e}"],
            ["st5", "s1.t", "s1", "t", "", "NULL", "{e}"],
            ["st6", "s1.t", "s1", "t", "1 3", "NULL", "{d,f,m}"],
        ]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT pg_get_statisticsobjdef(oid), pg_get_statisticsobjdef_columns(oid), pg_get_statisticsobjdef_expressions(oid)::text \
             FROM pg_statistic_ext ORDER BY stxname"
        ),
        [
            ["CREATE STATISTICS s1.st1 ON a, b FROM s1.t", "a, b", "NULL"],
            ["CREATE STATISTICS s1.st2 (ndistinct, mcv) ON a, b, c FROM s1.t", "a, b, c", "NULL"],
            ["CREATE STATISTICS s1.st3 ON a, c, (a + b) FROM s1.t", "a, c, (a + b)", "{\"(a + b)\"}"],
            ["CREATE STATISTICS s1.st4 (dependencies) ON (a + b), (a * b) FROM s1.t", "(a + b), (a * b)", "{\"(a + b)\",\"(a * b)\"}"],
            ["CREATE STATISTICS s1.st5 ON (a + b) FROM s1.t", "(a + b)", "{\"(a + b)\"}"],
            ["CREATE STATISTICS s1.st6 ON a, c FROM s1.t", "a, c", "NULL"],
        ]
    );
    assert_eq!(rows(&mut c, "SELECT pg_get_statisticsobjdef(0) IS NULL, pg_get_statisticsobjdef_columns(0) IS NULL"), [["t", "t"]]);
}

#[test]
fn statistics_expression_forms_and_unnamed() {
    let (_db, mut c) = setup();
    c.batch_execute(STATS_TABLE).expect("table");
    c.batch_execute(
        "CREATE STATISTICS s1.x1 ON lower(c), a FROM s1.t;
         CREATE STATISTICS s1.x2 ON (c || 'x'), (a::text), (- b) FROM s1.t;
         CREATE STATISTICS ON c, (a + b), a FROM s1.t;
         CREATE STATISTICS ON b, a FROM s1.t;
         CREATE STATISTICS ON b, a FROM s1.t;
         CREATE STATISTICS ON lower(c), a FROM s1.t",
    )
    .expect("create");
    assert_eq!(
        rows(&mut c, "SELECT stxname, pg_get_statisticsobjdef_columns(oid) FROM pg_statistic_ext ORDER BY stxname"),
        [
            ["t_b_a_stat", "a, b"],
            ["t_b_a_stat1", "a, b"],
            ["t_c_expr_a_stat", "a, c, (a + b)"],
            ["t_expr_a_stat", "a, lower(c)"],
            ["x1", "a, lower(c)"],
            ["x2", "(c || 'x'::text), (a::text), (- b)"],
        ]
    );
    // The statistics name is always schema-qualified; the table is, off the search path.
    c.batch_execute("SET search_path = s1; CREATE STATISTICS x3 ON a, b FROM t").expect("create on search path");
    assert_eq!(
        rows(&mut c, "SELECT pg_get_statisticsobjdef(oid) FROM pg_statistic_ext WHERE stxname = 'x3'"),
        [["CREATE STATISTICS s1.x3 ON a, b FROM t"]]
    );
}

#[test]
fn create_statistics_errors() {
    let (_db, mut c) = setup();
    c.batch_execute(STATS_TABLE).expect("table");
    c.batch_execute("CREATE STATISTICS s1.dup ON a, b FROM s1.t").expect("create");
    let mut check = |sql: &str, code: &str, msg: &str| {
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    };
    check("CREATE STATISTICS s1.dup ON a, b FROM s1.t", "42710", "statistics object \"dup\" already exists");
    check("CREATE STATISTICS s1.e ON a FROM s1.t", "42P17", "extended statistics require at least 2 columns");
    check("CREATE STATISTICS s1.e ON a, a FROM s1.t", "42701", "duplicate column name in statistics definition");
    check("CREATE STATISTICS s1.e ON (a + b), (a + b) FROM s1.t", "42701", "duplicate expression in statistics definition");
    check("CREATE STATISTICS s1.e (foo) ON a, b FROM s1.t", "42601", "unrecognized statistics kind \"foo\"");
    check("CREATE STATISTICS s1.e ON a, zz FROM s1.t", "42703", "column \"zz\" does not exist");
    check("CREATE STATISTICS s1.e ON a, b FROM s1.nosuch", "42P01", "relation \"s1.nosuch\" does not exist");
    check("CREATE STATISTICS s1.e ON a, ctid FROM s1.t", "0A000", "statistics creation on system columns is not supported");
    check(
        "CREATE STATISTICS s1.e ON a, f FROM s1.t",
        "0A000",
        "column \"f\" cannot be used in statistics because its type json has no default btree operator class",
    );
    check(
        "CREATE STATISTICS s1.e ON a, (f -> 'x') FROM s1.t",
        "0A000",
        "expression cannot be used in multivariate statistics because its type json has no default btree operator class",
    );
    check(
        "CREATE STATISTICS s1.e (ndistinct) ON (a + 1) FROM s1.t",
        "0A000",
        "when building statistics on a single expression, statistics kinds may not be specified",
    );
    check("CREATE STATISTICS s1.e ON a, b FROM s1.t, s1.t", "0A000", "only a single relation is allowed in CREATE STATISTICS");
    check("CREATE STATISTICS s1.e ON a, b, a, b, a, b, a, b, (a + 1) FROM s1.t", "54011", "cannot have more than 8 columns in statistics");
    c.batch_execute("CREATE VIEW s1.v AS SELECT 1 a, 2 b").expect("view");
    let e = expect_code(c.batch_execute("CREATE STATISTICS s1.e ON a, b FROM s1.v"), "42809");
    assert_eq!(message(&e), "cannot define statistics for relation \"v\"");
    // IF NOT EXISTS skips; a statistics object may share its table's name.
    c.batch_execute("CREATE STATISTICS IF NOT EXISTS s1.dup ON a, b FROM s1.t; CREATE STATISTICS s1.t ON a, b FROM s1.t").expect("skip");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_statistic_ext"), [["2"]]);
}

#[test]
fn alter_comment_and_drop_statistics() {
    let (_db, mut c) = setup();
    c.batch_execute(STATS_TABLE).expect("table");
    c.batch_execute("CREATE SCHEMA s2; CREATE STATISTICS s1.a1 ON a, b FROM s1.t; CREATE STATISTICS s1.a2 ON a, b FROM s1.t")
        .expect("create");
    c.batch_execute("ALTER STATISTICS s1.a1 RENAME TO a1r; ALTER STATISTICS s1.a1r SET SCHEMA s2").expect("rename and move");
    c.batch_execute("ALTER STATISTICS s2.a1r SET STATISTICS 100").expect("target");
    assert_eq!(
        rows(&mut c, "SELECT stxname, stxnamespace::regnamespace::text, stxstattarget FROM pg_statistic_ext ORDER BY stxname"),
        [["a1r", "s2", "100"], ["a2", "s1", "NULL"]]
    );
    c.batch_execute("ALTER STATISTICS s2.a1r SET STATISTICS -1").expect("default target");
    assert_eq!(rows(&mut c, "SELECT stxstattarget FROM pg_statistic_ext WHERE stxname = 'a1r'"), [["NULL"]]);
    let e = expect_code(c.batch_execute("ALTER STATISTICS s2.a1r SET STATISTICS -2"), "22023");
    assert_eq!(message(&e), "statistics target -2 is too low");
    let e = expect_code(c.batch_execute("ALTER STATISTICS s1.a2 RENAME TO a2"), "42710");
    assert_eq!(message(&e), "statistics object \"a2\" already exists in schema \"s1\"");
    let e = expect_code(c.batch_execute("ALTER STATISTICS s1.nosuch RENAME TO x"), "42704");
    assert_eq!(message(&e), "statistics object \"s1.nosuch\" does not exist");
    c.batch_execute("ALTER STATISTICS IF EXISTS s1.nosuch SET STATISTICS 1").expect("if exists");
    c.batch_execute("DROP ROLE IF EXISTS statsowner; CREATE ROLE statsowner; ALTER STATISTICS s1.a2 OWNER TO statsowner").expect("owner");
    assert_eq!(
        rows(&mut c, "SELECT (SELECT rolname FROM pg_roles WHERE oid = stxowner) FROM pg_statistic_ext WHERE stxname = 'a2'"),
        [["statsowner"]]
    );
    c.batch_execute("COMMENT ON STATISTICS s1.a2 IS 'hello'").expect("comment");
    assert_eq!(rows(&mut c, "SELECT obj_description(oid, 'pg_statistic_ext') FROM pg_statistic_ext WHERE stxname = 'a2'"), [["hello"]]);
    let e = expect_code(c.batch_execute("DROP STATISTICS s1.nosuch"), "42704");
    assert_eq!(message(&e), "statistics object \"s1.nosuch\" does not exist");
    c.batch_execute("DROP STATISTICS IF EXISTS s1.nosuch, s2.a1r; DROP STATISTICS s1.a2 CASCADE").expect("drop");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_statistic_ext"), [["0"]]);
    c.batch_execute("DROP ROLE statsowner").expect("drop role");
}

#[test]
fn statistics_follow_their_table_and_columns() {
    let (_db, mut c) = setup();
    c.batch_execute(STATS_TABLE).expect("table");
    c.batch_execute(
        "CREATE STATISTICS s1.k1 ON a, b, c FROM s1.t;
         CREATE STATISTICS s1.k2 ON a, (b + 1) FROM s1.t;
         CREATE STATISTICS s1.k3 ON a, c FROM s1.t",
    )
    .expect("create");
    c.batch_execute("ALTER TABLE s1.t RENAME COLUMN b TO bb").expect("rename");
    assert_eq!(
        rows(&mut c, "SELECT stxname, pg_get_statisticsobjdef_columns(oid) FROM pg_statistic_ext ORDER BY stxname"),
        [["k1", "a, bb, c"], ["k2", "a, (bb + 1)"], ["k3", "a, c"]]
    );
    // Dropping a column drops every statistics object that reads it.
    c.batch_execute("ALTER TABLE s1.t DROP COLUMN bb").expect("drop column");
    assert_eq!(rows(&mut c, "SELECT stxname FROM pg_statistic_ext ORDER BY stxname"), [["k3"]]);
    // psql's \d footer reads them.
    assert_eq!(
        rows(
            &mut c,
            "SELECT stxrelid::regclass::text, stxnamespace::regnamespace::text AS nsp, stxname, pg_get_statisticsobjdef_columns(oid), \
             'd' = any(stxkind), 'f' = any(stxkind), 'm' = any(stxkind), stxstattarget FROM pg_statistic_ext \
             WHERE stxrelid = 's1.t'::regclass ORDER BY nsp, stxname"
        ),
        [["s1.t", "s1", "k3", "a, c", "t", "t", "t", "NULL"]]
    );
    c.batch_execute("DROP TABLE s1.t").expect("drop table");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_statistic_ext"), [["0"]]);
}

const CAST_SETUP: &str = "CREATE TYPE mood AS ENUM ('sad', 'ok');
    CREATE DOMAIN pos AS int CHECK (VALUE > 0);
    CREATE TYPE pt AS (x int, y int);
    CREATE FUNCTION mood_to_int(m mood) RETURNS int LANGUAGE sql AS 'select case m when ''sad'' then 0 else 1 end';
    CREATE FUNCTION int_to_mood(i int) RETURNS mood LANGUAGE sql AS 'select case i when 0 then ''sad''::mood else ''ok''::mood end';
    CREATE FUNCTION pt_text(p pt) RETURNS text LANGUAGE sql AS 'select (p).x || '','' || (p).y'";

#[test]
fn create_cast_applies_and_lists_in_pg_cast() {
    let (_db, mut c) = setup();
    c.batch_execute(CAST_SETUP).expect("setup");
    c.batch_execute(
        "CREATE CAST (mood AS int) WITH FUNCTION mood_to_int(mood);
         CREATE CAST (int AS mood) WITH FUNCTION int_to_mood(int) AS ASSIGNMENT;
         CREATE CAST (pt AS text) WITH FUNCTION pt_text(pt) AS IMPLICIT",
    )
    .expect("create casts");
    assert_eq!(rows(&mut c, "SELECT 'ok'::mood::int, 0::mood, (1, 2)::pt::text, 1::mood::text"), [["1", "sad", "1,2", "ok"]]);
    c.batch_execute("CREATE TABLE ct (m mood); INSERT INTO ct VALUES (1)").expect("assignment cast");
    assert_eq!(rows(&mut c, "SELECT m::text FROM ct"), [["ok"]]);
    let e = expect_code(c.batch_execute("SELECT 'ok'::mood = 1"), "42883");
    assert!(message(&e).starts_with("operator does not exist"), "{}", message(&e));
    assert_eq!(
        rows(
            &mut c,
            "SELECT oid >= 16384, castsource::regtype::text, casttarget::regtype::text, castfunc::regproc::text, castcontext, castmethod \
             FROM pg_cast WHERE oid >= 16384 ORDER BY oid"
        ),
        [
            ["t", "mood", "integer", "mood_to_int", "e", "f"],
            ["t", "integer", "mood", "int_to_mood", "a", "f"],
            ["t", "pt", "text", "pt_text", "i", "f"],
        ]
    );
    // The builtin rows are there too.
    assert_eq!(
        rows(
            &mut c,
            "SELECT castfunc <> 0, castcontext, castmethod FROM pg_cast WHERE castsource = 'int4'::regtype AND casttarget = 'int8'::regtype"
        ),
        [["t", "i", "f"]]
    );
}

#[test]
fn create_cast_inout_and_binary() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE pt AS (x int, y int)").expect("type");
    c.batch_execute("CREATE CAST (varchar AS pt) WITH INOUT AS ASSIGNMENT; CREATE CAST (bigint AS pt) WITH INOUT").expect("inout");
    assert_eq!(rows(&mut c, "SELECT castmethod, castcontext FROM pg_cast WHERE oid >= 16384 ORDER BY oid"), [["i", "a"], ["i", "e"]]);
    assert_eq!(rows(&mut c, "SELECT '(1,2)'::varchar::pt::text"), [["(1,2)"]]);
    let e = expect_code(c.batch_execute("SELECT 5::bigint::pt"), "22P02");
    assert_eq!(message(&e), "malformed record literal: \"5\"");
}

#[test]
fn create_cast_errors() {
    let (_db, mut c) = setup();
    c.batch_execute(CAST_SETUP).expect("setup");
    c.batch_execute(
        "CREATE FUNCTION two(a int, b int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION setof_pt(i int) RETURNS SETOF pt LANGUAGE sql AS 'select (1,2)::pt';
         CREATE FUNCTION ident(i int) RETURNS int LANGUAGE sql AS 'select i';
         CREATE CAST (mood AS int) WITH FUNCTION mood_to_int(mood)",
    )
    .expect("functions");
    let mut check = |sql: &str, code: &str, msg: &str| {
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    };
    check("CREATE CAST (mood AS int) WITH FUNCTION mood_to_int(mood)", "42710", "cast from type mood to type integer already exists");
    check("CREATE CAST (oid AS int) WITHOUT FUNCTION", "42710", "cast from type oid to type integer already exists");
    check(
        "CREATE CAST (int AS int) WITH FUNCTION mood_to_int(mood)",
        "42P17",
        "argument of cast function must match or be binary-coercible from source data type",
    );
    check("CREATE CAST (int AS int) WITH FUNCTION ident(int)", "42P17", "source data type and target data type are the same");
    check("CREATE CAST (int AS int) WITHOUT FUNCTION", "42P17", "source data type and target data type are the same");
    check(
        "CREATE CAST (bigint AS pt) WITH FUNCTION ident(int)",
        "42P17",
        "argument of cast function must match or be binary-coercible from source data type",
    );
    check(
        "CREATE CAST (int AS pt) WITH FUNCTION ident(int)",
        "42P17",
        "return data type of cast function must match or be binary-coercible to target data type",
    );
    check("CREATE CAST (int AS pt) WITH FUNCTION setof_pt(int)", "42P17", "cast function must not return a set");
    check("CREATE CAST (mood AS mood) WITHOUT FUNCTION", "42P17", "enum data types are not binary-compatible");
    check("CREATE CAST (int AS pos) WITHOUT FUNCTION", "42P17", "domain data types must not be marked binary-compatible");
    check("CREATE CAST (pt AS int[]) WITHOUT FUNCTION", "42P17", "source and target data types are not physically compatible");
    check("CREATE CAST (int AS void) WITHOUT FUNCTION", "42809", "target data type void is a pseudo-type");
    check("CREATE CAST (bigint AS nosuchtype) WITHOUT FUNCTION", "42704", "type \"nosuchtype\" does not exist");
    check("CREATE CAST (int AS pt) WITH FUNCTION nosuch(int)", "42883", "function nosuch(integer) does not exist");
}

#[test]
fn drop_cast_and_dependencies() {
    let (_db, mut c) = setup();
    c.batch_execute(CAST_SETUP).expect("setup");
    c.batch_execute("CREATE CAST (int AS mood) WITH FUNCTION int_to_mood(int); COMMENT ON CAST (int AS mood) IS 'c'").expect("cast");
    assert_eq!(
        rows(
            &mut c,
            "SELECT obj_description(oid, 'pg_cast') FROM pg_cast WHERE castsource = 'int4'::regtype AND casttarget = 'mood'::regtype"
        ),
        [["c"]]
    );
    let e = expect_code(c.batch_execute("DROP FUNCTION int_to_mood(int)"), "2BP01");
    assert_eq!(message(&e), "cannot drop function int_to_mood(integer) because other objects depend on it");
    let e = expect_code(c.batch_execute("DROP TYPE mood"), "2BP01");
    assert_eq!(message(&e), "cannot drop type mood because other objects depend on it");
    c.batch_execute("DROP CAST (int AS mood)").expect("drop");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_cast WHERE oid >= 16384"), [["0"]]);
    let e = expect_code(c.batch_execute("DROP CAST (int AS mood)"), "42704");
    assert_eq!(message(&e), "cast from type integer to type mood does not exist");
    c.batch_execute("DROP CAST IF EXISTS (int AS mood)").expect("if exists");
    let e = expect_code(c.batch_execute("DROP CAST (oid AS int)"), "2BP01");
    assert_eq!(message(&e), "cannot drop cast from oid to integer because it is required by the database system");
    // CASCADE paths take the cast with them.
    c.batch_execute("CREATE CAST (int AS mood) WITH FUNCTION int_to_mood(int)").expect("cast again");
    c.batch_execute("DROP FUNCTION int_to_mood(int) CASCADE").expect("cascade function");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_cast WHERE oid >= 16384"), [["0"]]);
    c.batch_execute("CREATE CAST (pt AS text) WITH FUNCTION pt_text(pt)").expect("cast again");
    c.batch_execute("DROP TYPE pt CASCADE").expect("cascade type");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_cast WHERE oid >= 16384"), [["0"]]);
}
