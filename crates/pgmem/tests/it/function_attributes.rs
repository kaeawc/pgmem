//! Function attributes (SECURITY DEFINER, SET, PARALLEL, COST, ROWS, LEAKPROOF, STRICT,
//! volatility) in `pg_proc` and `pg_get_functiondef`, and `ALTER FUNCTION / PROCEDURE` actions.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn def(c: &mut postgres::Client, name: &str) -> String {
    rows(c, &format!("SELECT pg_get_functiondef('{name}'::regproc)"))[0][0].clone()
}

const PROC: &str =
    "SELECT prosecdef, proconfig, proparallel, procost, prorows, proleakproof, proisstrict, provolatile FROM pg_proc WHERE proname = ";

#[test]
fn create_function_records_attributes() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE FUNCTION f() RETURNS int LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog PARALLEL SAFE COST 5 AS 'select 1';
         CREATE FUNCTION g() RETURNS setof int LANGUAGE sql STABLE STRICT LEAKPROOF PARALLEL RESTRICTED ROWS 7
           SET work_mem = '1MB' SET search_path TO a, \"B c\", 'd' SET statement_timeout = 5 AS 'select 1';
         CREATE FUNCTION h() RETURNS int LANGUAGE sql SET x.y = on SET DateStyle = 'ISO, DMY' PARALLEL UNSAFE COST 0.5 AS 'select 1';
         CREATE FUNCTION k() RETURNS setof int LANGUAGE sql ROWS 1000 COST 100 AS 'select 1';
         CREATE PROCEDURE p() LANGUAGE sql SECURITY DEFINER SET search_path = public AS 'select 1'",
    )
    .expect("create");
    assert_eq!(rows(&mut c, &format!("{PROC}'f'")), [["t", "{search_path=pg_catalog}", "s", "5", "0", "f", "f", "v"]]);
    assert_eq!(
        rows(&mut c, &format!("{PROC}'g'")),
        [["f", "{work_mem=1MB,\"search_path=a, \\\"B c\\\", d\",statement_timeout=5}", "r", "100", "7", "t", "t", "s"]]
    );
    assert_eq!(rows(&mut c, &format!("{PROC}'h'")), [["f", "{x.y=on,\"DateStyle=ISO, DMY\"}", "u", "0.5", "0", "f", "f", "v"]]);
    assert_eq!(
        def(&mut c, "f"),
        "CREATE OR REPLACE FUNCTION public.f()\n RETURNS integer\n LANGUAGE sql\n PARALLEL SAFE SECURITY DEFINER COST 5\n SET search_path TO 'pg_catalog'\nAS $function$select 1$function$\n"
    );
    assert_eq!(
        def(&mut c, "g"),
        "CREATE OR REPLACE FUNCTION public.g()\n RETURNS SETOF integer\n LANGUAGE sql\n STABLE PARALLEL RESTRICTED STRICT LEAKPROOF ROWS 7\n SET work_mem TO '1MB'\n SET search_path TO 'a', 'B c', 'd'\n SET statement_timeout TO '5'\nAS $function$select 1$function$\n"
    );
    assert_eq!(
        def(&mut c, "h"),
        "CREATE OR REPLACE FUNCTION public.h()\n RETURNS integer\n LANGUAGE sql\n COST 0.5\n SET \"x.y\" TO 'on'\n SET \"DateStyle\" TO 'ISO, DMY'\nAS $function$select 1$function$\n"
    );
    // The defaults print nothing.
    assert_eq!(
        def(&mut c, "k"),
        "CREATE OR REPLACE FUNCTION public.k()\n RETURNS SETOF integer\n LANGUAGE sql\nAS $function$select 1$function$\n"
    );
    assert_eq!(
        def(&mut c, "p"),
        "CREATE OR REPLACE PROCEDURE public.p()\n LANGUAGE sql\n SECURITY DEFINER\n SET search_path TO 'public'\nAS $procedure$select 1$procedure$\n"
    );
}

#[test]
fn plain_function_deparse_is_unchanged() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE FUNCTION plain(a int) RETURNS int LANGUAGE sql IMMUTABLE STRICT AS 'select a'").expect("create");
    assert_eq!(
        def(&mut c, "plain"),
        "CREATE OR REPLACE FUNCTION public.plain(a integer)\n RETURNS integer\n LANGUAGE sql\n IMMUTABLE STRICT\nAS $function$select a$function$\n"
    );
    assert_eq!(rows(&mut c, &format!("{PROC}'plain'")), [["f", "NULL", "u", "100", "0", "f", "t", "i"]]);
}

#[test]
fn create_function_option_errors() {
    let (_db, mut c) = setup();
    let create = |opts: &str| format!("CREATE FUNCTION e() RETURNS int LANGUAGE sql {opts} AS 'select 1'");
    let e = expect_code(c.batch_execute(&create("SET nosuch = 1")), "42704");
    assert_eq!(message(&e), "unrecognized configuration parameter \"nosuch\"");
    expect_code(c.batch_execute(&create("SET work_mem = 'abc'")), "22023");
    let e = expect_code(c.batch_execute(&create("ROWS 5")), "22023");
    assert_eq!(message(&e), "ROWS is not applicable when function does not return a set");
    let e = expect_code(c.batch_execute(&create("COST -1")), "22023");
    assert_eq!(message(&e), "COST must be positive");
    let e = expect_code(c.batch_execute(&create("PARALLEL foo")), "42601");
    assert_eq!(message(&e), "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE");
    let e = expect_code(c.batch_execute(&create("SECURITY DEFINER SECURITY INVOKER")), "42601");
    assert_eq!(message(&e), "conflicting or redundant options");
    let e = expect_code(c.batch_execute("CREATE PROCEDURE pe() LANGUAGE sql IMMUTABLE AS 'select 1'"), "42P13");
    assert_eq!(message(&e), "invalid attribute in procedure definition");
}

#[test]
fn alter_function_actions() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE FUNCTION a1(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION a1(text) RETURNS int LANGUAGE sql AS 'select 2';
         CREATE FUNCTION a2() RETURNS setof int LANGUAGE sql AS 'select 1';
         CREATE PROCEDURE ap(int) LANGUAGE sql AS 'select 1'",
    )
    .expect("setup");
    let e = expect_code(c.batch_execute("ALTER FUNCTION a1 IMMUTABLE"), "42725");
    assert_eq!(message(&e), "function name \"a1\" is not unique");
    c.batch_execute(
        "ALTER FUNCTION a1(int) IMMUTABLE STRICT EXTERNAL SECURITY DEFINER LEAKPROOF PARALLEL SAFE COST 3
           SET search_path = pg_catalog SET work_mem = '2MB'",
    )
    .expect("alter");
    assert_eq!(
        rows(
            &mut c,
            "SELECT prosecdef, proconfig, proparallel, procost, proleakproof, proisstrict, provolatile FROM pg_proc WHERE oid = 'a1(int)'::regprocedure"
        ),
        [["t", "{search_path=pg_catalog,work_mem=2MB}", "s", "3", "t", "t", "i"]]
    );
    c.batch_execute("ALTER FUNCTION a1(int) SET search_path = public").expect("replace in place");
    c.batch_execute("ALTER FUNCTION a1(int) RESET work_mem").expect("reset");
    assert_eq!(rows(&mut c, "SELECT proconfig FROM pg_proc WHERE oid = 'a1(int)'::regprocedure"), [["{search_path=public}"]]);
    c.batch_execute("SET work_mem = '3MB'; ALTER FUNCTION a1(int) SET work_mem FROM CURRENT").expect("from current");
    assert_eq!(rows(&mut c, "SELECT proconfig FROM pg_proc WHERE oid = 'a1(int)'::regprocedure"), [["{search_path=public,work_mem=3MB}"]]);
    c.batch_execute("ALTER FUNCTION a1(int) RESET ALL NOT LEAKPROOF CALLED ON NULL INPUT EXTERNAL SECURITY INVOKER").expect("reset all");
    assert_eq!(
        rows(&mut c, "SELECT prosecdef, proconfig, proleakproof, proisstrict FROM pg_proc WHERE oid = 'a1(int)'::regprocedure"),
        [["f", "NULL", "f", "f"]]
    );
    c.batch_execute("ALTER FUNCTION a2() ROWS 3 COST 2 VOLATILE").expect("rows");
    assert_eq!(rows(&mut c, "SELECT prorows, procost FROM pg_proc WHERE proname = 'a2'"), [["3", "2"]]);
    // Errors.
    let e = expect_code(c.batch_execute("ALTER FUNCTION a1(int) ROWS 3"), "22023");
    assert_eq!(message(&e), "ROWS is not applicable when function does not return a set");
    let e = expect_code(c.batch_execute("ALTER FUNCTION a2() ROWS 0"), "22023");
    assert_eq!(message(&e), "ROWS must be positive");
    expect_code(c.batch_execute("ALTER FUNCTION a1(int) COST 0"), "22023");
    expect_code(c.batch_execute("ALTER FUNCTION a1(int) PARALLEL x"), "42601");
    let e = expect_code(c.batch_execute("ALTER FUNCTION a1(int) VOLATILE STABLE"), "42601");
    assert_eq!(message(&e), "conflicting or redundant options");
    let e = expect_code(c.batch_execute("ALTER FUNCTION a1(int) SET nosuch = 1"), "42704");
    assert_eq!(message(&e), "unrecognized configuration parameter \"nosuch\"");
    expect_code(c.batch_execute("ALTER FUNCTION a1(int) RESET nosuch"), "42704");
    let e = expect_code(c.batch_execute("ALTER FUNCTION nope() IMMUTABLE"), "42883");
    assert_eq!(message(&e), "function nope() does not exist");
    let e = expect_code(c.batch_execute("ALTER FUNCTION a1(bool) IMMUTABLE"), "42883");
    assert_eq!(message(&e), "function a1(boolean) does not exist");
    let e = expect_code(c.batch_execute("ALTER FUNCTION ap(int) IMMUTABLE"), "42809");
    assert_eq!(message(&e), "ap(integer) is not a function");
    let e = expect_code(c.batch_execute("ALTER PROCEDURE a2() IMMUTABLE"), "42809");
    assert_eq!(message(&e), "a2() is not a procedure");
}

#[test]
fn alter_procedure_and_routine() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE PROCEDURE ap(int) LANGUAGE sql AS 'select 1'; CREATE FUNCTION af() RETURNS int LANGUAGE sql AS 'select 1'")
        .expect("setup");
    c.batch_execute("ALTER PROCEDURE ap(int) SECURITY DEFINER SET search_path = public").expect("alter procedure");
    assert_eq!(rows(&mut c, "SELECT prosecdef, proconfig FROM pg_proc WHERE proname = 'ap'"), [["t", "{search_path=public}"]]);
    for sql in ["ALTER PROCEDURE ap(int) IMMUTABLE", "ALTER PROCEDURE ap(int) PARALLEL SAFE", "ALTER PROCEDURE ap(int) ROWS 5"] {
        let e = expect_code(c.batch_execute(sql), "42P13");
        assert_eq!(message(&e), "invalid attribute in procedure definition");
    }
    c.batch_execute("ALTER ROUTINE af() STABLE; ALTER ROUTINE ap(int) RESET ALL").expect("routine");
    assert_eq!(rows(&mut c, "SELECT provolatile FROM pg_proc WHERE proname = 'af'"), [["s"]]);
    assert_eq!(rows(&mut c, "SELECT proconfig FROM pg_proc WHERE proname = 'ap'"), [["NULL"]]);
}

#[test]
fn alter_function_rename_and_set_schema() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA other;
         CREATE FUNCTION r1(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION r1(text) RETURNS int LANGUAGE sql AS 'select 2';
         CREATE FUNCTION r2(int) RETURNS int LANGUAGE sql AS 'select 3';
         CREATE FUNCTION other.r2(int) RETURNS int LANGUAGE sql AS 'select 4'",
    )
    .expect("setup");
    let e = expect_code(c.batch_execute("ALTER FUNCTION r1(int) RENAME TO r2"), "42723");
    assert_eq!(message(&e), "function r2(integer) already exists in schema \"public\"");
    c.batch_execute("ALTER FUNCTION r1(int) RENAME TO r3; ALTER FUNCTION r1(text) RENAME TO r3").expect("rename");
    assert_eq!(rows(&mut c, "SELECT r3(1), r3('a')"), [["1", "2"]]);
    let e = expect_code(c.batch_execute("ALTER FUNCTION r1(int) RENAME TO r4"), "42883");
    assert_eq!(message(&e), "function r1(integer) does not exist");
    let e = expect_code(c.batch_execute("ALTER FUNCTION r2(int) SET SCHEMA other"), "42723");
    assert_eq!(message(&e), "function r2(integer) already exists in schema \"other\"");
    let e = expect_code(c.batch_execute("ALTER FUNCTION r3(int) SET SCHEMA nosuch"), "3F000");
    assert_eq!(message(&e), "schema \"nosuch\" does not exist");
    c.batch_execute("ALTER FUNCTION r3(int) SET SCHEMA other").expect("set schema");
    assert_eq!(rows(&mut c, "SELECT other.r3(1)"), [["1"]]);
}

#[test]
fn function_set_applies_during_the_call() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE FUNCTION cur() RETURNS text LANGUAGE sql SET myapp.mode = 'inner' AS $$ SELECT current_setting('myapp.mode') $$;
         SET myapp.mode = 'outer'",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT cur(), current_setting('myapp.mode')"), [["inner", "outer"]]);
}
