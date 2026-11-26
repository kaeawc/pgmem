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

/// Roles are cluster-wide on PostgreSQL: each test creates its own (unique to it) and drops
/// them at the end.
fn fresh_roles(c: &mut postgres::Client, roles: &[&str]) {
    drop_roles(c, roles);
    for r in roles {
        c.batch_execute(&format!("CREATE ROLE {r}")).expect("create role");
    }
}

fn drop_roles(c: &mut postgres::Client, roles: &[&str]) {
    let _ = c.batch_execute("RESET ROLE");
    for r in roles {
        let _ = c.batch_execute(&format!("DROP OWNED BY {r}"));
        let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {r}"));
    }
}

const OWNER_OF: &str = "SELECT r.rolname FROM pg_proc p JOIN pg_roles r ON r.oid = p.proowner WHERE p.oid = ";

#[test]
fn security_definer_runs_as_the_owner() {
    let (_db, mut c) = setup();
    let roles = ["itfa_sd_owner", "itfa_sd_caller"];
    fresh_roles(&mut c, &roles);
    let session = rows(&mut c, "SELECT session_user::text")[0][0].clone();
    c.batch_execute(
        "GRANT CREATE ON SCHEMA public TO itfa_sd_owner;
         CREATE TABLE sd_secret (v int); INSERT INTO sd_secret VALUES (42);
         GRANT SELECT ON sd_secret TO itfa_sd_owner;
         SET ROLE itfa_sd_owner;
         CREATE FUNCTION sd_who() RETURNS text LANGUAGE sql SECURITY DEFINER
           AS $$ SELECT current_user || '/' || session_user || '/' || current_setting('role') $$;
         CREATE FUNCTION sd_inv() RETURNS text LANGUAGE sql AS $$ SELECT current_user::text $$;
         CREATE FUNCTION sd_nested() RETURNS text LANGUAGE sql SECURITY DEFINER AS $$ SELECT sd_inv() $$;
         CREATE FUNCTION sd_read() RETURNS int LANGUAGE sql SECURITY DEFINER AS $$ SELECT v FROM sd_secret $$;
         CREATE FUNCTION sd_read_inv() RETURNS int LANGUAGE sql AS $$ SELECT v FROM sd_secret $$;
         CREATE FUNCTION sd_read_pl() RETURNS int LANGUAGE plpgsql SECURITY DEFINER
           AS $$ DECLARE n int; BEGIN SELECT v INTO n FROM sd_secret; RETURN n; END $$;
         CREATE FUNCTION sd_err() RETURNS int LANGUAGE plpgsql SECURITY DEFINER
           AS $$ BEGIN RAISE EXCEPTION 'boom %', current_user; END $$;
         RESET ROLE",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, &format!("{OWNER_OF}'sd_who()'::regprocedure")), [["itfa_sd_owner"]]);
    assert_eq!(rows(&mut c, "SELECT prosecdef FROM pg_proc WHERE proname = 'sd_who'"), [["t"]]);
    c.batch_execute("SET ROLE itfa_sd_caller").expect("set role");
    assert_eq!(
        rows(&mut c, "SELECT sd_who(), sd_inv(), sd_nested(), current_user"),
        [[format!("itfa_sd_owner/{session}/itfa_sd_caller"), "itfa_sd_caller".into(), "itfa_sd_owner".into(), "itfa_sd_caller".into()]]
    );
    // Privileges inside the body are the owner's (definer) or the caller's (invoker).
    assert_eq!(rows(&mut c, "SELECT sd_read(), sd_read_pl()"), [["42", "42"]]);
    let e = expect_code(c.batch_execute("SELECT sd_read_inv()"), "42501");
    assert_eq!(message(&e), "permission denied for table sd_secret");
    let e = expect_code(c.batch_execute("SELECT * FROM sd_secret"), "42501");
    assert_eq!(message(&e), "permission denied for table sd_secret");
    // The caller's identity is restored after an error inside the function.
    let e = expect_code(c.batch_execute("SELECT sd_err()"), "P0001");
    assert_eq!(message(&e), "boom itfa_sd_owner");
    assert_eq!(rows(&mut c, "SELECT current_user"), [["itfa_sd_caller"]]);
    let caught = c
        .batch_execute(
            "DO $$ BEGIN PERFORM sd_err(); EXCEPTION WHEN others THEN
               IF current_user <> 'itfa_sd_caller' THEN RAISE EXCEPTION 'not restored: %', current_user; END IF; END $$",
        )
        .map_err(|e| message(&e));
    assert_eq!(caught, Ok(()));
    c.batch_execute("RESET ROLE").expect("reset role");
    // SET role is refused inside a security-definer function, when it runs.
    c.batch_execute(
        "CREATE FUNCTION sd_setrole() RETURNS text LANGUAGE sql SECURITY DEFINER SET role = itfa_sd_owner AS $$ SELECT 'x' $$;
         CREATE FUNCTION inv_setrole() RETURNS text LANGUAGE sql SET role = itfa_sd_owner
           AS $$ SELECT current_user || '/' || current_setting('role') $$",
    )
    .expect("create with SET role");
    let e = expect_code(c.batch_execute("SELECT sd_setrole()"), "42501");
    assert_eq!(message(&e), "cannot set parameter \"role\" within security-definer function");
    assert_eq!(rows(&mut c, "SELECT inv_setrole(), current_user = session_user"), [["itfa_sd_owner/itfa_sd_owner", "t"]]);
    // Ownership moves with REASSIGN OWNED; DROP OWNED drops the functions.
    c.batch_execute("REASSIGN OWNED BY itfa_sd_owner TO itfa_sd_caller").expect("reassign");
    assert_eq!(rows(&mut c, &format!("{OWNER_OF}'sd_who()'::regprocedure")), [["itfa_sd_caller"]]);
    c.batch_execute("DROP OWNED BY itfa_sd_caller").expect("drop owned");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_proc WHERE proname LIKE 'sd\\_%'"), [["1"]]);
    drop_roles(&mut c, &roles);
}

#[test]
fn security_definer_procedure_and_writes() {
    let (_db, mut c) = setup();
    let roles = ["itfa_sw_owner", "itfa_sw_caller"];
    fresh_roles(&mut c, &roles);
    c.batch_execute(
        "GRANT CREATE ON SCHEMA public TO itfa_sw_owner;
         CREATE TABLE sw_log (who text);
         GRANT INSERT ON sw_log TO itfa_sw_owner;
         SET ROLE itfa_sw_owner;
         CREATE PROCEDURE sw_note() LANGUAGE sql SECURITY DEFINER AS $$ INSERT INTO sw_log VALUES (current_user) $$;
         CREATE PROCEDURE sw_note_inv() LANGUAGE sql AS $$ INSERT INTO sw_log VALUES (current_user) $$;
         RESET ROLE;
         SET ROLE itfa_sw_caller;
         CALL sw_note()",
    )
    .expect("setup");
    let e = expect_code(c.batch_execute("CALL sw_note_inv()"), "42501");
    assert_eq!(message(&e), "permission denied for table sw_log");
    c.batch_execute("RESET ROLE").expect("reset");
    assert_eq!(rows(&mut c, "SELECT who FROM sw_log"), [["itfa_sw_owner"]]);
    drop_roles(&mut c, &roles);
}

#[test]
fn execute_privilege_on_functions() {
    let (_db, mut c) = setup();
    let roles = ["itfa_ex_owner", "itfa_ex_caller"];
    fresh_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE ex_empty (i int); GRANT SELECT ON ex_empty TO itfa_ex_caller;
         CREATE FUNCTION ex_f() RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION ex_open() RETURNS int LANGUAGE sql AS 'select 2';
         CREATE PROCEDURE ex_p() LANGUAGE sql AS 'select 1';
         CREATE FUNCTION ex_via() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'select ex_f()';
         REVOKE EXECUTE ON FUNCTION ex_f() FROM PUBLIC;
         REVOKE ALL ON PROCEDURE ex_p() FROM PUBLIC",
    )
    .expect("setup");
    assert_eq!(
        rows(
            &mut c,
            "SELECT has_function_privilege('itfa_ex_caller', 'ex_f()', 'EXECUTE'), has_function_privilege('itfa_ex_caller', 'ex_open()', 'EXECUTE')"
        ),
        [["f", "t"]]
    );
    c.batch_execute("SET ROLE itfa_ex_caller").expect("set role");
    assert_eq!(rows(&mut c, "SELECT ex_open()"), [["2"]]);
    let e = expect_code(c.batch_execute("SELECT ex_f()"), "42501");
    assert_eq!(message(&e), "permission denied for function ex_f");
    // Checked when the statement starts, not when a row calls it.
    let e = expect_code(c.batch_execute("SELECT ex_f() FROM ex_empty"), "42501");
    assert_eq!(message(&e), "permission denied for function ex_f");
    let e = expect_code(c.batch_execute("CALL ex_p()"), "42501");
    assert_eq!(message(&e), "permission denied for procedure ex_p");
    let e = expect_code(c.batch_execute("DO $$ BEGIN PERFORM ex_f(); END $$"), "42501");
    assert_eq!(message(&e), "permission denied for function ex_f");
    // A security-definer function calls it with its owner's privileges.
    assert_eq!(rows(&mut c, "SELECT ex_via()"), [["1"]]);
    c.batch_execute("RESET ROLE; GRANT EXECUTE ON FUNCTION ex_f() TO itfa_ex_caller; GRANT EXECUTE ON PROCEDURE ex_p() TO PUBLIC")
        .expect("grant");
    c.batch_execute("SET ROLE itfa_ex_caller").expect("set role");
    assert_eq!(rows(&mut c, "SELECT ex_f()"), [["1"]]);
    c.batch_execute("CALL ex_p()").expect("call after grant");
    c.batch_execute("RESET ROLE").expect("reset");
    drop_roles(&mut c, &roles);
}

#[test]
fn function_search_path_resolves_names_in_the_body() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA sp_a; CREATE TABLE sp_a.t (v int); INSERT INTO sp_a.t VALUES (5);
         CREATE TABLE t (v int); INSERT INTO t VALUES (1);
         CREATE FUNCTION sp_sql() RETURNS int LANGUAGE sql SET search_path = sp_a AS 'select v from t';
         CREATE FUNCTION sp_pl() RETURNS int LANGUAGE plpgsql SET search_path = sp_a, public
           AS $$ DECLARE n int; BEGIN SELECT v INTO n FROM t; RETURN n; END $$;
         CREATE FUNCTION sp_path() RETURNS text LANGUAGE sql SET search_path = sp_a, pg_catalog
           AS $$ SELECT current_schemas(true)::text || ' ' || current_setting('search_path') $$;
         CREATE FUNCTION sp_plain() RETURNS int LANGUAGE sql AS 'select v from t';
         CREATE PROCEDURE sp_ins() LANGUAGE sql SET search_path = sp_a AS 'insert into t values (6)'",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT sp_sql(), sp_pl(), sp_plain(), (SELECT v FROM t)"), [["5", "5", "1", "1"]]);
    assert_eq!(rows(&mut c, "SELECT sp_path()"), [["{sp_a,pg_catalog} sp_a, pg_catalog"]]);
    c.batch_execute("CALL sp_ins()").expect("call");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM sp_a.t"), [["2"]]);
    assert_eq!(rows(&mut c, "SELECT count(*), current_setting('search_path') FROM t"), [["1", "\"$user\", public"]]);
}

#[test]
fn security_definer_user_schema_is_the_owners() {
    let (_db, mut c) = setup();
    let roles = ["itfa_us_owner", "itfa_us_caller"];
    fresh_roles(&mut c, &roles);
    c.batch_execute(
        "GRANT CREATE ON SCHEMA public TO itfa_us_owner;
         CREATE SCHEMA itfa_us_owner AUTHORIZATION itfa_us_owner;
         SET ROLE itfa_us_owner;
         CREATE FUNCTION public.us_path() RETURNS text LANGUAGE sql SECURITY DEFINER AS $$ SELECT current_schemas(false)::text $$;
         RESET ROLE",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itfa_us_caller").expect("set role");
    assert_eq!(rows(&mut c, "SELECT us_path(), current_schemas(false)::text"), [["{itfa_us_owner,public}", "{public}"]]);
    c.batch_execute("RESET ROLE; DROP SCHEMA itfa_us_owner CASCADE").expect("cleanup");
    drop_roles(&mut c, &roles);
}

#[test]
fn alter_function_owner_resolves_the_overload() {
    let (_db, mut c) = setup();
    let roles = ["itfa_ow_owner"];
    fresh_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE FUNCTION ow(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION ow(text) RETURNS int LANGUAGE sql AS 'select 2';
         CREATE PROCEDURE owp() LANGUAGE sql AS 'select 1';
         ALTER FUNCTION ow(text) OWNER TO itfa_ow_owner",
    )
    .expect("setup");
    assert_eq!(
        rows(
            &mut c,
            "SELECT p.oid::regprocedure::text, r.rolname = current_user FROM pg_proc p JOIN pg_roles r ON r.oid = p.proowner WHERE proname = 'ow' ORDER BY 1"
        ),
        [["ow(integer)", "t"], ["ow(text)", "f"]]
    );
    assert_eq!(rows(&mut c, &format!("{OWNER_OF}'ow(text)'::regprocedure")), [["itfa_ow_owner"]]);
    let e = expect_code(c.batch_execute("ALTER FUNCTION ow OWNER TO itfa_ow_owner"), "42725");
    assert_eq!(message(&e), "function name \"ow\" is not unique");
    let e = expect_code(c.batch_execute("ALTER FUNCTION ow(bool) OWNER TO itfa_ow_owner"), "42883");
    assert_eq!(message(&e), "function ow(boolean) does not exist");
    let e = expect_code(c.batch_execute("ALTER FUNCTION ow(int) OWNER TO itfa_ow_nosuch"), "42704");
    assert_eq!(message(&e), "role \"itfa_ow_nosuch\" does not exist");
    c.batch_execute("ALTER PROCEDURE owp OWNER TO itfa_ow_owner; ALTER ROUTINE ow(text) OWNER TO CURRENT_USER").expect("owner to");
    assert_eq!(rows(&mut c, &format!("{OWNER_OF}'owp()'::regprocedure")), [["itfa_ow_owner"]]);
    assert_eq!(
        rows(
            &mut c,
            "SELECT r.rolname = current_user FROM pg_proc p JOIN pg_roles r ON r.oid = p.proowner WHERE p.oid = 'ow(text)'::regprocedure"
        ),
        [["t"]]
    );
    drop_roles(&mut c, &roles);
}

#[test]
fn alter_function_depends_on_extension() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE EXTENSION pg_trgm;
         CREATE FUNCTION de(int) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE FUNCTION de(text) RETURNS int LANGUAGE sql AS 'select 2';
         CREATE PROCEDURE dep() LANGUAGE sql AS 'select 1'",
    )
    .expect("setup");
    let e = expect_code(c.batch_execute("ALTER FUNCTION de(int) DEPENDS ON EXTENSION nosuch"), "42704");
    assert_eq!(message(&e), "extension \"nosuch\" does not exist");
    c.batch_execute(
        "ALTER FUNCTION de(int) DEPENDS ON EXTENSION pg_trgm; ALTER FUNCTION de(int) DEPENDS ON EXTENSION pg_trgm;
         ALTER FUNCTION de(text) DEPENDS ON EXTENSION pg_trgm; ALTER FUNCTION de(text) NO DEPENDS ON EXTENSION pg_trgm;
         ALTER FUNCTION de(text) NO DEPENDS ON EXTENSION pg_trgm;
         ALTER PROCEDURE dep() DEPENDS ON EXTENSION plpgsql; ALTER ROUTINE dep NO DEPENDS ON EXTENSION plpgsql",
    )
    .expect("depends");
    assert_eq!(
        rows(
            &mut c,
            "SELECT d.objid::regprocedure::text, e.extname, d.deptype FROM pg_depend d JOIN pg_extension e ON e.oid = d.refobjid
             WHERE d.classid = 'pg_proc'::regclass AND d.deptype = 'x' ORDER BY 1"
        ),
        [["de(integer)", "pg_trgm", "x"]]
    );
    // Dropping the extension drops the functions that depend on it, without CASCADE.
    c.batch_execute("DROP EXTENSION pg_trgm").expect("drop extension");
    assert_eq!(rows(&mut c, "SELECT oid::regprocedure::text FROM pg_proc WHERE proname = 'de'"), [["de(text)"]]);
}

#[test]
fn information_schema_routines_columns() {
    let (_db, mut c) = setup();
    let roles = ["itfa_ir_caller"];
    fresh_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE FUNCTION ir_sql(a int) RETURNS int LANGUAGE sql IMMUTABLE STRICT AS 'select a';
         CREATE FUNCTION ir_pl() RETURNS text[] LANGUAGE plpgsql SECURITY DEFINER AS $$ BEGIN RETURN NULL; END $$;
         CREATE PROCEDURE ir_proc() LANGUAGE sql AS 'select 1';
         CREATE FUNCTION ir_hidden() RETURNS int LANGUAGE sql AS 'select 1';
         REVOKE EXECUTE ON FUNCTION ir_hidden() FROM PUBLIC",
    )
    .expect("setup");
    let cols = "routine_name, routine_type, data_type, type_udt_catalog IS NOT NULL, type_udt_schema, type_udt_name, dtd_identifier, \
        routine_body, routine_definition, external_name, external_language, parameter_style, is_deterministic, sql_data_access, \
        is_null_call, sql_path, schema_level_routine, max_dynamic_result_sets, is_user_defined_cast, is_implicitly_invocable, \
        security_type, as_locator, created, new_savepoint_level, is_udt_dependent, module_name, udt_name, result_cast_from_data_type";
    let q = format!("SELECT {cols} FROM information_schema.routines WHERE routine_name LIKE 'ir\\_%' ORDER BY routine_name");
    assert_eq!(
        rows(&mut c, &q),
        [
            [
                "ir_hidden",
                "FUNCTION",
                "integer",
                "t",
                "pg_catalog",
                "int4",
                "0",
                "SQL",
                "select 1",
                "NULL",
                "SQL",
                "GENERAL",
                "NO",
                "MODIFIES",
                "NO",
                "NULL",
                "YES",
                "0",
                "NULL",
                "NULL",
                "INVOKER",
                "NO",
                "NULL",
                "NULL",
                "NO",
                "NULL",
                "NULL",
                "NULL"
            ],
            [
                "ir_pl",
                "FUNCTION",
                "ARRAY",
                "t",
                "pg_catalog",
                "_text",
                "0",
                "EXTERNAL",
                " BEGIN RETURN NULL; END ",
                "NULL",
                "PLPGSQL",
                "GENERAL",
                "NO",
                "MODIFIES",
                "NO",
                "NULL",
                "YES",
                "0",
                "NULL",
                "NULL",
                "DEFINER",
                "NO",
                "NULL",
                "NULL",
                "NO",
                "NULL",
                "NULL",
                "NULL"
            ],
            [
                "ir_proc",
                "PROCEDURE",
                "NULL",
                "f",
                "NULL",
                "NULL",
                "NULL",
                "SQL",
                "select 1",
                "NULL",
                "SQL",
                "GENERAL",
                "NO",
                "MODIFIES",
                "NULL",
                "NULL",
                "YES",
                "0",
                "NULL",
                "NULL",
                "INVOKER",
                "NO",
                "NULL",
                "NULL",
                "NO",
                "NULL",
                "NULL",
                "NULL"
            ],
            [
                "ir_sql",
                "FUNCTION",
                "integer",
                "t",
                "pg_catalog",
                "int4",
                "0",
                "SQL",
                "select a",
                "NULL",
                "SQL",
                "GENERAL",
                "YES",
                "MODIFIES",
                "YES",
                "NULL",
                "YES",
                "0",
                "NULL",
                "NULL",
                "INVOKER",
                "NO",
                "NULL",
                "NULL",
                "NO",
                "NULL",
                "NULL",
                "NULL"
            ],
        ]
    );
    assert_eq!(c.query("SELECT * FROM information_schema.routines WHERE routine_name = 'ir_sql'", &[]).expect("select *")[0].len(), 82);
    // Other roles see the routines they may execute, without the definition.
    c.batch_execute("SET ROLE itfa_ir_caller").expect("set role");
    assert_eq!(
        rows(
            &mut c,
            "SELECT routine_name, routine_definition FROM information_schema.routines WHERE routine_name LIKE 'ir\\_%' ORDER BY 1"
        ),
        [["ir_pl", "NULL"], ["ir_proc", "NULL"], ["ir_sql", "NULL"]]
    );
    drop_roles(&mut c, &roles);
}
