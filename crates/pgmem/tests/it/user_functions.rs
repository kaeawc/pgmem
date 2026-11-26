//! User-defined functions and procedures (SQL and PL/pgSQL), DO blocks, CALL, triggers
//! (BEFORE / AFTER, row / statement, WHEN, UPDATE OF, INSTEAD OF on views), and their
//! catalog entries.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

#[test]
fn sql_functions_overloads_and_procedures() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE sm (version text); INSERT INTO sm VALUES ('20240101'), ('20240102');
         CREATE FUNCTION add_one(x int) RETURNS int LANGUAGE sql IMMUTABLE AS $$ SELECT x + 1 $$",
    )
    .expect("setup");
    let e = expect_code(c.batch_execute("CREATE FUNCTION add_one(x int) RETURNS int LANGUAGE sql AS $$ SELECT x + 2 $$"), "42723");
    assert_eq!(message(&e), "function \"add_one\" already exists with same argument types");
    c.batch_execute("CREATE OR REPLACE FUNCTION add_one(x int) RETURNS int LANGUAGE sql AS $$ SELECT x + 2 $$").expect("replace");
    let e = expect_code(
        c.batch_execute("CREATE OR REPLACE FUNCTION add_one(x int) RETURNS bigint LANGUAGE sql AS $$ SELECT 1::bigint $$"),
        "42P13",
    );
    assert_eq!(message(&e), "cannot change return type of existing function");
    c.batch_execute("CREATE FUNCTION add_one(x text) RETURNS text LANGUAGE sql AS $$ SELECT x || '1' $$").expect("overload");
    assert_eq!(rows(&mut c, "SELECT add_one(40), add_one('a')"), [["42", "a1"]]);
    let e = expect_code(c.batch_execute("DROP FUNCTION add_one"), "42725");
    assert_eq!(message(&e), "function name \"add_one\" is not unique");
    c.batch_execute(
        "CREATE FUNCTION versions() RETURNS SETOF text LANGUAGE sql STABLE AS $$ SELECT version FROM sm ORDER BY version $$;
         CREATE FUNCTION mul(a int, b int) RETURNS int LANGUAGE sql RETURN a * b;
         CREATE FUNCTION defs(a int, b int DEFAULT 10) RETURNS int LANGUAGE sql AS 'SELECT a + b';
         CREATE FUNCTION strictf(a int) RETURNS int LANGUAGE sql STRICT AS 'SELECT 42'",
    )
    .expect("more functions");
    assert_eq!(rows(&mut c, "SELECT v, mul(6, 7) FROM versions() AS v"), [["20240101", "42"], ["20240102", "42"]]);
    assert_eq!(rows(&mut c, "SELECT defs(1), defs(1, 2), strictf(NULL), strictf(1)"), [["11", "3", "NULL", "42"]]);
    c.batch_execute("CREATE PROCEDURE log_version(v text) LANGUAGE sql AS $$ INSERT INTO sm VALUES (v) $$; CALL log_version('proc')")
        .expect("procedure");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM sm WHERE version = 'proc'"), [["1"]]);
    let e = expect_code(c.batch_execute("SELECT log_version('x')"), "42809");
    assert_eq!(message(&e), "log_version(unknown) is a procedure");
    assert_eq!(
        rows(&mut c, "SELECT pg_get_function_arguments('defs'::regproc), pg_get_function_result('versions'::regproc)"),
        [["a integer, b integer DEFAULT 10", "SETOF text"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT proname, prokind, proretset, pronargs, pronargdefaults FROM pg_proc WHERE proname IN ('defs', 'log_version', 'versions') ORDER BY 1"
        ),
        [["defs", "f", "f", "2", "1"], ["log_version", "p", "f", "1", "0"], ["versions", "f", "t", "0", "0"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT routine_name, routine_type, data_type FROM information_schema.routines WHERE routine_name IN ('mul', 'log_version') ORDER BY 1"
        ),
        [["log_version", "PROCEDURE", "NULL"], ["mul", "FUNCTION", "integer"]]
    );
    c.batch_execute("COMMENT ON FUNCTION mul(int, int) IS 'multiplies'").expect("comment");
    assert_eq!(rows(&mut c, "SELECT obj_description('mul'::regproc, 'pg_proc')"), [["multiplies"]]);
    // A function a default uses cannot go without CASCADE.
    c.batch_execute("CREATE FUNCTION f_one() RETURNS int LANGUAGE sql AS 'SELECT 1'; CREATE TABLE fdep (a int DEFAULT f_one())")
        .expect("dep");
    let e = expect_code(c.batch_execute("DROP FUNCTION f_one()"), "2BP01");
    assert_eq!(message(&e), "cannot drop function f_one() because other objects depend on it");
    c.batch_execute("DROP FUNCTION f_one() CASCADE; INSERT INTO fdep DEFAULT VALUES").expect("cascade");
    assert_eq!(rows(&mut c, "SELECT a FROM fdep"), [["NULL"]]);
}

#[test]
fn plpgsql_control_flow_and_errors() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE pt (id int PRIMARY KEY, v text); INSERT INTO pt VALUES (1, 'a'), (2, 'b'), (3, 'c');
         CREATE FUNCTION classify(n int) RETURNS text LANGUAGE plpgsql AS $$ DECLARE r text; BEGIN IF n < 0 THEN r := 'neg'; ELSIF n = 0 THEN r := 'zero'; ELSE r := 'pos'; END IF; RETURN r; END $$;
         CREATE FUNCTION safe_div(a int, b int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN a / b; EXCEPTION WHEN division_by_zero THEN RETURN NULL; END $$;
         CREATE FUNCTION f_into(i int) RETURNS text LANGUAGE plpgsql AS $$ DECLARE r text; BEGIN SELECT v INTO r FROM pt WHERE id = i; IF NOT FOUND THEN RETURN 'none'; END IF; RETURN r; END $$;
         CREATE FUNCTION f_strict(i int) RETURNS text LANGUAGE plpgsql AS $$ DECLARE r text; BEGIN SELECT v INTO STRICT r FROM pt WHERE id > i; RETURN r; END $$;
         CREATE FUNCTION f_loop() RETURNS int LANGUAGE plpgsql AS $$ DECLARE s int := 0; BEGIN <<outer>> FOR i IN 1..10 LOOP CONTINUE WHEN i % 2 = 0; FOR j IN REVERSE 3..1 LOOP EXIT outer WHEN i > 7; s := s + j; END LOOP; END LOOP; RETURN s; END $$;
         CREATE FUNCTION f_set(n int) RETURNS SETOF int LANGUAGE plpgsql AS $$ BEGIN FOR i IN 1..n LOOP RETURN NEXT i * 10; END LOOP; RETURN QUERY SELECT id FROM pt ORDER BY id; END $$;
         CREATE FUNCTION f_out(a int, OUT doubled int, OUT tripled int) LANGUAGE plpgsql AS $$ BEGIN doubled := a * 2; tripled := a * 3; END $$;
         CREATE FUNCTION f_exc() RETURNS text LANGUAGE plpgsql AS $$ BEGIN INSERT INTO pt VALUES (1, 'dup'); RETURN 'ok'; EXCEPTION WHEN unique_violation THEN RETURN 'caught ' || SQLSTATE; END $$;
         CREATE FUNCTION rec_fact(n int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN IF n <= 1 THEN RETURN 1; END IF; RETURN n * rec_fact(n - 1); END $$;
         CREATE FUNCTION fail_if(b boolean) RETURNS void LANGUAGE plpgsql AS $$ BEGIN IF b THEN RAISE EXCEPTION 'custom failure %', 42 USING ERRCODE = '22023'; END IF; END $$",
    )
    .expect("functions");
    assert_eq!(
        rows(&mut c, "SELECT classify(-5), classify(0), classify(9), safe_div(10, 2), safe_div(1, 0)"),
        [["neg", "zero", "pos", "5", "NULL"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT f_into(2), f_into(9), f_strict(2), f_loop(), rec_fact(5), f_out(5)::text"),
        [["b", "none", "c", "24", "120", "(10,15)"]]
    );
    let e = expect_code(c.batch_execute("SELECT f_strict(0)"), "P0003");
    assert_eq!(message(&e), "query returned more than one row");
    expect_code(c.batch_execute("SELECT f_strict(5)"), "P0002");
    assert_eq!(rows(&mut c, "SELECT * FROM f_set(2)"), [["10"], ["20"], ["1"], ["2"], ["3"]]);
    let e = expect_code(c.batch_execute("SELECT fail_if(true)"), "22023");
    assert_eq!(message(&e), "custom failure 42");
    assert_eq!(rows(&mut c, "SELECT fail_if(false)::text"), [[""]]);
    let e = expect_code(c.batch_execute("DO $$ BEGIN RAISE EXCEPTION 'boom'; END $$"), "P0001");
    assert_eq!(message(&e), "boom");
    let e = expect_code(
        c.batch_execute("DO $$ BEGIN RAISE EXCEPTION USING MESSAGE = 'custom msg', ERRCODE = 'P0099', HINT = 'a hint'; END $$"),
        "P0099",
    );
    assert_eq!(message(&e), "custom msg");
    expect_code(c.batch_execute("DO $$ BEGIN RAISE division_by_zero; END $$"), "22012");
    let e = expect_code(c.batch_execute("DO $$ BEGIN SELECT 1; END $$"), "42601");
    assert_eq!(message(&e), "query has no destination for result data");
    let e = expect_code(c.batch_execute("DO $$ BEGIN CASE 5 WHEN 1 THEN NULL; END CASE; END $$"), "20000");
    assert_eq!(message(&e), "case not found");
    expect_code(c.batch_execute("DO $$ BEGIN ASSERT 1 = 2, 'nope'; END $$"), "P0004");
    let e =
        expect_code(c.batch_execute("CREATE FUNCTION noret() RETURNS int LANGUAGE plpgsql AS $$ BEGIN END $$; SELECT noret()"), "2F005");
    assert_eq!(message(&e), "control reached end of function without RETURN");
}

#[test]
fn do_blocks_and_procedures_modify_data() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE sm (version text); CREATE TABLE log (m text);
         DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_type WHERE typname = 'status') THEN CREATE TYPE status AS ENUM ('a', 'b'); END IF; END $$;
         DO $$ BEGIN CREATE TYPE status AS ENUM ('a'); EXCEPTION WHEN duplicate_object THEN null; END $$;
         DO $$ DECLARE i int; BEGIN FOR i IN 1..3 LOOP INSERT INTO sm VALUES ('do_' || i); END LOOP; END $$;
         DO $$ BEGIN EXECUTE 'ALTER TABLE sm ADD COLUMN IF NOT EXISTS dirty boolean NOT NULL DEFAULT false'; END $$;
         CREATE PROCEDURE p_exc() LANGUAGE plpgsql AS $$ BEGIN BEGIN INSERT INTO log VALUES ('x'); RAISE EXCEPTION 'undo'; EXCEPTION WHEN OTHERS THEN NULL; END; INSERT INTO log VALUES ('y'); END $$;
         CALL p_exc()",
    )
    .expect("DO and CALL");
    assert_eq!(rows(&mut c, "SELECT version, dirty FROM sm ORDER BY version"), [["do_1", "f"], ["do_2", "f"], ["do_3", "f"]]);
    // The exception undid the inner block's INSERT.
    assert_eq!(rows(&mut c, "SELECT m FROM log"), [["y"]]);
    c.batch_execute("DO $$ DECLARE n int; BEGIN EXECUTE 'SELECT count(*) FROM sm WHERE version > $1' INTO n USING 'do_1'; INSERT INTO log VALUES (n::text); END $$")
        .expect("EXECUTE USING");
    assert_eq!(rows(&mut c, "SELECT m FROM log ORDER BY m"), [["2"], ["y"]]);
}

#[test]
fn triggers_fire_around_dml() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE docs (id int PRIMARY KEY, body text, version int NOT NULL DEFAULT 0);
         CREATE TABLE audit (op text, doc_id int, lvl text);
         CREATE FUNCTION bump() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.version := OLD.version + 1; RETURN NEW; END $$;
         CREATE TRIGGER docs_bump BEFORE UPDATE ON docs FOR EACH ROW EXECUTE FUNCTION bump();
         CREATE FUNCTION aud() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_LEVEL = 'STATEMENT' THEN INSERT INTO audit VALUES (TG_OP, NULL, TG_WHEN || ' stmt'); RETURN NULL; END IF; IF TG_OP = 'DELETE' THEN INSERT INTO audit VALUES (TG_OP, OLD.id, 'row'); RETURN OLD; END IF; INSERT INTO audit VALUES (TG_OP, NEW.id, 'row'); RETURN NEW; END $$;
         CREATE TRIGGER docs_aud AFTER INSERT OR UPDATE OR DELETE ON docs FOR EACH ROW EXECUTE FUNCTION aud();
         CREATE TRIGGER docs_aud_s BEFORE INSERT ON docs FOR EACH STATEMENT EXECUTE FUNCTION aud();
         INSERT INTO docs (id, body) VALUES (1, 'a'), (2, 'b'); UPDATE docs SET body = body || '!' WHERE id = 1; DELETE FROM docs WHERE id = 2",
    )
    .expect("triggers");
    assert_eq!(rows(&mut c, "SELECT id, body, version FROM docs"), [["1", "a!", "1"]]);
    assert_eq!(
        rows(&mut c, "SELECT op, coalesce(doc_id::text, '-'), lvl FROM audit ORDER BY op, doc_id, lvl"),
        [["DELETE", "2", "row"], ["INSERT", "1", "row"], ["INSERT", "2", "row"], ["INSERT", "-", "BEFORE stmt"], ["UPDATE", "1", "row"]]
    );
    // A BEFORE trigger returning NULL skips the row; WHEN filters the rows it fires for.
    c.batch_execute(
        "CREATE FUNCTION skip_odd() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.id % 2 = 1 THEN RETURN NULL; END IF; RETURN NEW; END $$;
         CREATE TRIGGER skip BEFORE INSERT ON docs FOR EACH ROW WHEN (NEW.id > 10) EXECUTE FUNCTION skip_odd();
         INSERT INTO docs (id, body) VALUES (11, 'x'), (12, 'y'), (3, 'z')",
    )
    .expect("skip");
    assert_eq!(rows(&mut c, "SELECT id FROM docs ORDER BY id"), [["1"], ["3"], ["12"]]);
    // UPDATE OF fires only when a listed column is SET; DISABLE TRIGGER stops it.
    c.batch_execute("CREATE TRIGGER t_upd_of BEFORE UPDATE OF body ON docs FOR EACH ROW EXECUTE FUNCTION bump(); UPDATE docs SET version = 100 WHERE id = 3")
        .expect("update of");
    assert_eq!(rows(&mut c, "SELECT version FROM docs WHERE id = 3"), [["1"]]);
    c.batch_execute("ALTER TABLE docs DISABLE TRIGGER ALL; UPDATE docs SET body = 'q' WHERE id = 3; ALTER TABLE docs ENABLE TRIGGER ALL")
        .expect("disable");
    assert_eq!(rows(&mut c, "SELECT body, version FROM docs WHERE id = 3"), [["q", "1"]]);
    let e = expect_code(c.batch_execute("CREATE TRIGGER docs_bump BEFORE UPDATE ON docs FOR EACH ROW EXECUTE FUNCTION bump()"), "42710");
    assert_eq!(message(&e), "trigger \"docs_bump\" for relation \"docs\" already exists");
    let e = expect_code(c.batch_execute("CREATE TRIGGER bad INSTEAD OF INSERT ON docs FOR EACH ROW EXECUTE FUNCTION bump()"), "42809");
    assert_eq!(message(&e), "\"docs\" is a table");
    let e = expect_code(c.batch_execute("CREATE FUNCTION notrig() RETURNS int LANGUAGE sql AS 'SELECT 1'; CREATE TRIGGER bad3 BEFORE INSERT ON docs FOR EACH ROW EXECUTE FUNCTION notrig()"), "42P17");
    assert_eq!(message(&e), "function notrig must return type trigger");
    let e = expect_code(c.batch_execute("DROP FUNCTION bump()"), "2BP01");
    assert_eq!(message(&e), "cannot drop function bump() because other objects depend on it");
    assert_eq!(
        rows(&mut c, "SELECT tgname, tgtype, tgenabled FROM pg_trigger WHERE tgrelid = 'docs'::regclass ORDER BY 1"),
        [["docs_aud", "29", "O"], ["docs_aud_s", "6", "O"], ["docs_bump", "19", "O"], ["skip", "7", "O"], ["t_upd_of", "19", "O"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT trigger_name, event_manipulation, action_timing, action_orientation FROM information_schema.triggers WHERE event_object_table = 'docs' AND trigger_name LIKE 'docs_aud%' ORDER BY 1, 2"
        ),
        [
            ["docs_aud", "DELETE", "AFTER", "ROW"],
            ["docs_aud", "INSERT", "AFTER", "ROW"],
            ["docs_aud", "UPDATE", "AFTER", "ROW"],
            ["docs_aud_s", "INSERT", "BEFORE", "STATEMENT"]
        ]
    );
    let e = expect_code(c.batch_execute("DROP TRIGGER nosuch ON docs"), "42704");
    assert_eq!(message(&e), "trigger \"nosuch\" for table \"docs\" does not exist");
    c.batch_execute("DROP TRIGGER IF EXISTS nosuch ON docs").expect("IF EXISTS");
}

#[test]
fn instead_of_triggers_make_views_writable() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE docs (id int PRIMARY KEY, body text);
         CREATE VIEW dv AS SELECT id, upper(body) AS shout FROM docs;
         CREATE FUNCTION dv_ins() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO docs VALUES (NEW.id, lower(NEW.shout)); RETURN NEW; END $$;
         CREATE FUNCTION dv_upd() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE docs SET body = lower(NEW.shout) WHERE id = OLD.id; RETURN NEW; END $$;
         CREATE FUNCTION dv_del() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN DELETE FROM docs WHERE id = OLD.id; RETURN OLD; END $$;
         CREATE TRIGGER dv_i INSTEAD OF INSERT ON dv FOR EACH ROW EXECUTE FUNCTION dv_ins();
         CREATE TRIGGER dv_u INSTEAD OF UPDATE ON dv FOR EACH ROW EXECUTE FUNCTION dv_upd();
         CREATE TRIGGER dv_d INSTEAD OF DELETE ON dv FOR EACH ROW EXECUTE FUNCTION dv_del();
         INSERT INTO dv VALUES (1, 'ONE'), (2, 'TWO'); UPDATE dv SET shout = 'UNO' WHERE id = 1; DELETE FROM dv WHERE id = 2",
    )
    .expect("instead of");
    assert_eq!(rows(&mut c, "SELECT id, body FROM docs ORDER BY id"), [["1", "uno"]]);
}

#[test]
fn before_row_trigger_writes_another_table() {
    // #174: a BEFORE row trigger may modify other tables (BEFORE UPDATE and DELETE too).
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE a (x int); CREATE TABLE log (m text);
         CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
           INSERT INTO log VALUES (TG_OP || ' ' || coalesce(NEW.x, OLD.x));
           IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; RETURN NEW; END $$;
         CREATE TRIGGER t BEFORE INSERT OR UPDATE OR DELETE ON a FOR EACH ROW EXECUTE FUNCTION tf();
         INSERT INTO a VALUES (1), (2); UPDATE a SET x = x + 10 WHERE x = 1; DELETE FROM a WHERE x = 2",
    )
    .expect("BEFORE trigger writes");
    assert_eq!(rows(&mut c, "SELECT m FROM log ORDER BY m"), [["DELETE 2"], ["INSERT 1"], ["INSERT 2"], ["UPDATE 11"]]);
    assert_eq!(rows(&mut c, "SELECT x FROM a"), [["11"]]);
    // A failed statement undoes the trigger's writes with it.
    c.batch_execute("CREATE TABLE b (x int CHECK (x > 0)); CREATE TRIGGER tb BEFORE INSERT ON b FOR EACH ROW EXECUTE FUNCTION tf()")
        .expect("b");
    expect_code(c.batch_execute("INSERT INTO b VALUES (-1)"), "23514");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM log"), [["4"]]);
}

#[test]
fn volatile_functions_in_queries_write() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE a (x int); CREATE TABLE log (m text); INSERT INTO a VALUES (1), (2);
         CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('f'); RETURN (SELECT count(*) FROM log); END $$;
         CREATE FUNCTION g() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO a VALUES (100); RETURN 1; END $$;
         CREATE FUNCTION sq() RETURNS int LANGUAGE sql AS $$ INSERT INTO log VALUES ('sq'); SELECT count(*)::int FROM log $$",
    )
    .expect("setup");
    // Each call sees the writes of the calls before it.
    assert_eq!(rows(&mut c, "SELECT f(), f()"), [["1", "2"]]);
    assert_eq!(rows(&mut c, "SELECT sq()"), [["3"]]);
    // The calling statement keeps its snapshot: it does not see rows its functions added.
    assert_eq!(rows(&mut c, "SELECT g(), (SELECT count(*) FROM a) FROM a"), [["1", "2"], ["1", "2"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM a"), [["4"]]);
    c.batch_execute("INSERT INTO a VALUES (g()); CREATE TABLE d (x int DEFAULT f()); INSERT INTO d DEFAULT VALUES")
        .expect("VALUES / DEFAULT");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM a"), [["6"]]);
    assert_eq!(rows(&mut c, "SELECT x FROM d"), [["4"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM log WHERE m = 'f'"), [["3"]]);
    // Writes inside an explicit transaction roll back with it.
    c.batch_execute("BEGIN; SELECT f(); ROLLBACK").expect("rollback");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM log"), [["4"]]);
}

#[test]
fn non_volatile_functions_cannot_write() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE log (m text);
         CREATE FUNCTION s() RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('s'); RETURN 1; END $$;
         CREATE FUNCTION ss() RETURNS int STABLE LANGUAGE sql AS $$ INSERT INTO log VALUES ('s') RETURNING 1 $$;
         CREATE FUNCTION im() RETURNS int IMMUTABLE LANGUAGE plpgsql AS $$ BEGIN UPDATE log SET m = m; RETURN 1; END $$;
         CREATE FUNCTION sd() RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN DELETE FROM log; RETURN 1; END $$;
         CREATE FUNCTION stf() RETURNS trigger STABLE LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('x'); RETURN NEW; END $$;
         CREATE TABLE b (x int); CREATE TRIGGER t AFTER INSERT ON b FOR EACH ROW EXECUTE FUNCTION stf()",
    )
    .expect("setup");
    for (sql, msg) in [
        ("SELECT s()", "INSERT is not allowed in a non-volatile function"),
        ("SELECT ss()", "INSERT is not allowed in a non-volatile function"),
        ("SELECT im()", "UPDATE is not allowed in a non-volatile function"),
        ("SELECT sd()", "DELETE is not allowed in a non-volatile function"),
        ("INSERT INTO b VALUES (1)", "INSERT is not allowed in a non-volatile function"),
    ] {
        let e = expect_code(c.batch_execute(sql), "0A000");
        assert_eq!(message(&e), msg, "{sql}");
    }
    assert_eq!(rows(&mut c, "SELECT count(*) FROM log"), [["0"]]);
}

#[test]
fn function_writes_in_read_only_transactions() {
    // Writes by functions, triggers, CALL and DO obey READ ONLY like top-level statements.
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE log (m text);
         CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('f'); RETURN 1; END $$;
         CREATE FUNCTION fs() RETURNS int LANGUAGE sql AS $$ INSERT INTO log VALUES ('f') RETURNING 1 $$;
         CREATE FUNCTION fd() RETURNS int LANGUAGE plpgsql AS $$ BEGIN DROP TABLE log; RETURN 1; END $$;
         CREATE PROCEDURE p() LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('p'); END $$;
         CREATE TEMP TABLE tmp (x int);
         CREATE FUNCTION ft() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO tmp VALUES (1); RETURN (SELECT count(*) FROM tmp); END $$;
         CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('t'); RETURN NEW; END $$;
         CREATE TABLE tgt (x int);
         CREATE TRIGGER t BEFORE INSERT ON tmp FOR EACH ROW WHEN (NEW.x > 1) EXECUTE FUNCTION trg()",
    )
    .expect("setup");
    for (sql, what) in [
        ("BEGIN READ ONLY; SELECT f()", "INSERT"),
        ("BEGIN READ ONLY; SELECT fs()", "INSERT"),
        ("BEGIN READ ONLY; SELECT fd()", "DROP TABLE"),
        ("BEGIN; SET TRANSACTION READ ONLY; SELECT f()", "INSERT"),
        ("BEGIN READ ONLY; INSERT INTO tmp VALUES (2)", "INSERT"),
        ("BEGIN READ ONLY; CALL p()", "INSERT"),
        ("BEGIN READ ONLY; DO $$ BEGIN INSERT INTO log VALUES ('d'); END $$", "INSERT"),
        ("BEGIN READ ONLY; CREATE TEMP TABLE t2 (x int)", "CREATE TABLE"),
    ] {
        let e = expect_code(c.batch_execute(sql), "25006");
        assert_eq!(message(&e), format!("cannot execute {what} in a read-only transaction"), "{sql}");
        c.batch_execute("ROLLBACK").expect("rollback");
    }
    c.batch_execute("SET default_transaction_read_only = on").expect("SET");
    let e = expect_code(c.batch_execute("SELECT f()"), "25006");
    assert_eq!(message(&e), "cannot execute INSERT in a read-only transaction");
    // Writes to temporary tables are allowed.
    assert_eq!(rows(&mut c, "SELECT ft()"), [["1"]]);
    c.batch_execute("INSERT INTO tmp VALUES (1); SET default_transaction_read_only = off").expect("temp writes");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM log"), [["0"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM tmp"), [["2"]]);
}

#[test]
fn exception_blocks_undo_function_writes() {
    // A handled exception rolls back the writes made inside its block, also by volatile
    // functions a non-volatile caller (or a volatile one called from a query) ran there.
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE log (m text);
         CREATE FUNCTION vf() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('vf'); RAISE EXCEPTION 'boom'; END $$;
         CREATE FUNCTION vok() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('ok'); RETURN 1; END $$;
         CREATE FUNCTION sf() RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN PERFORM vok();
           BEGIN PERFORM vok(); PERFORM vf(); EXCEPTION WHEN raise_exception THEN RETURN 0; END; RETURN 1; END $$;
         CREATE FUNCTION wf() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES ('w1');
           BEGIN INSERT INTO log VALUES ('w2'); PERFORM vf(); EXCEPTION WHEN raise_exception THEN INSERT INTO log VALUES ('caught'); END;
           RETURN 1; END $$;
         CREATE FUNCTION wf2() RETURNS int LANGUAGE plpgsql AS $$ BEGIN
           BEGIN INSERT INTO log VALUES ('x1'); RAISE EXCEPTION 'b'; EXCEPTION WHEN raise_exception THEN NULL; END; RETURN 1; END $$",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT sf()"), [["0"]]);
    assert_eq!(rows(&mut c, "SELECT m FROM log ORDER BY m"), [["ok"]]);
    c.batch_execute("DELETE FROM log").expect("delete");
    assert_eq!(rows(&mut c, "SELECT wf()"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT m FROM log ORDER BY m"), [["caught"], ["w1"]]);
    c.batch_execute("DELETE FROM log").expect("delete");
    assert_eq!(rows(&mut c, "SELECT wf2()"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM log"), [["0"]]);
}

#[test]
fn before_triggers_that_only_set_columns_stay_cheap() {
    // A volatile BEFORE trigger that writes nothing must not copy the transaction's changes
    // (here 50,000 rows of another table) for every row it fires on.
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE big (x int); CREATE TABLE ev (id int, at text);
         CREATE FUNCTION stamp() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.at := 'now'; RETURN NEW; END $$;
         CREATE TRIGGER s BEFORE INSERT ON ev FOR EACH ROW EXECUTE FUNCTION stamp()",
    )
    .expect("setup");
    let batch: String = (0..1000).map(|i| format!("INSERT INTO ev VALUES ({i});")).collect();
    let start = std::time::Instant::now();
    c.batch_execute(&format!("BEGIN; INSERT INTO big SELECT g FROM generate_series(1, 50000) g; {batch} COMMIT")).expect("inserts");
    let elapsed = start.elapsed();
    assert_eq!(rows(&mut c, "SELECT count(*), min(at) FROM ev"), [["1000", "now"]]);
    assert!(elapsed < std::time::Duration::from_secs(5), "1,000 trigger inserts took {elapsed:?}");
}

#[test]
fn row_movement_inside_before_update_triggers() {
    // A BEFORE UPDATE trigger moving rows of another partitioned table keeps both moves apart.
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE pa (k int, v int) PARTITION BY RANGE (k);
         CREATE TABLE pa1 PARTITION OF pa FOR VALUES FROM (0) TO (10);
         CREATE TABLE pa2 PARTITION OF pa FOR VALUES FROM (10) TO (100);
         CREATE TABLE pb (k int) PARTITION BY RANGE (k);
         CREATE TABLE pb1 PARTITION OF pb FOR VALUES FROM (0) TO (10);
         CREATE TABLE pb2 PARTITION OF pb FOR VALUES FROM (10) TO (100);
         INSERT INTO pa VALUES (1, 0), (2, 0); INSERT INTO pb VALUES (1), (2);
         CREATE FUNCTION mv() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE pb SET k = k + 10 WHERE k < 10; RETURN NEW; END $$;
         CREATE TRIGGER t BEFORE UPDATE ON pa1 FOR EACH ROW EXECUTE FUNCTION mv();
         UPDATE pa SET k = k + 10",
    )
    .expect("update");
    assert_eq!(rows(&mut c, "SELECT tableoid::regclass::text, k, v FROM pa ORDER BY k"), [["pa2", "11", "0"], ["pa2", "12", "0"]]);
    assert_eq!(rows(&mut c, "SELECT tableoid::regclass::text, k FROM pb ORDER BY k"), [["pb2", "11"], ["pb2", "12"]]);
}

#[test]
fn rollback_restores_sequences_reset_by_functions() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE s (id int GENERATED BY DEFAULT AS IDENTITY, v int); INSERT INTO s (v) VALUES (1), (2), (3);
         CREATE FUNCTION rs() RETURNS int LANGUAGE plpgsql AS $$ BEGIN ALTER SEQUENCE s_id_seq RESTART WITH 50; RETURN 1; END $$",
    )
    .expect("setup");
    c.batch_execute("BEGIN; TRUNCATE s RESTART IDENTITY; SELECT rs(); ROLLBACK").expect("rollback");
    assert_eq!(rows(&mut c, "INSERT INTO s (v) VALUES (4) RETURNING id"), [["4"]]);
}

#[test]
fn utility_statements_in_non_volatile_functions() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE log (m text);
         CREATE FUNCTION sdrop() RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN DROP TABLE log; RETURN 1; END $$;
         CREATE FUNCTION sidx() RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN CREATE INDEX ON log (m); RETURN 1; END $$;
         CREATE FUNCTION salter() RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN ALTER TABLE log ADD COLUMN z int; RETURN 1; END $$;
         CREATE FUNCTION scomment() RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN COMMENT ON TABLE log IS 'x'; RETURN 1; END $$;
         CREATE FUNCTION struncate() RETURNS int STABLE LANGUAGE sql AS $$ TRUNCATE log; SELECT 1 $$;
         CREATE FUNCTION scf() RETURNS int STABLE LANGUAGE plpgsql AS $$ BEGIN CREATE FUNCTION zz() RETURNS int LANGUAGE sql AS 'SELECT 1'; RETURN 1; END $$",
    )
    .expect("setup");
    for (sql, what) in [
        ("SELECT sdrop()", "DROP TABLE"),
        ("SELECT sidx()", "CREATE INDEX"),
        ("SELECT salter()", "ALTER TABLE"),
        ("SELECT scomment()", "COMMENT"),
        ("SELECT struncate()", "TRUNCATE TABLE"),
        ("SELECT scf()", "CREATE FUNCTION"),
    ] {
        let e = expect_code(c.batch_execute(sql), "0A000");
        assert_eq!(message(&e), format!("{what} is not allowed in a non-volatile function"), "{sql}");
    }
}
