//! regproc / regprocedure over builtin and user functions, UPDATE pg_settings, column
//! collations, information_schema.element_types, system view definitions, and ON CONFLICT
//! on expression index keys.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

#[test]
fn regproc_and_regprocedure() {
    let (_db, mut c) = setup();
    assert_eq!(
        rows(
            &mut c,
            "SELECT 'now'::regproc::oid::text, 'lower(text)'::regprocedure::text, 870::regproc::text, 'substr(text,int)'::regprocedure::text, \
             'concat_ws(text, \"any\")'::regprocedure::text, pg_typeof('now()'::regprocedure)::text"
        ),
        [["1299", "lower(text)", "pg_catalog.lower", "substr(text,integer)", "concat_ws(text,\"any\")", "regprocedure"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT to_regproc('upper')::text, to_regprocedure('abs(integer)')::text, to_regprocedure('nope()')::text"),
        [["NULL", "abs(integer)", "NULL"]]
    );
    let e = expect_code(c.batch_execute("SELECT 'lower'::regproc"), "42725");
    assert_eq!(message(&e), "more than one function named \"lower\"");
    let e = expect_code(c.batch_execute("SELECT 'lower(int)'::regprocedure"), "42883");
    assert_eq!(message(&e), "function \"lower(int)\" does not exist");
    let e = expect_code(c.batch_execute("SELECT 'lower'::regprocedure"), "22P02");
    assert_eq!(message(&e), "expected a left parenthesis");
    c.batch_execute("CREATE FUNCTION add_one(n int, label text) RETURNS int LANGUAGE sql AS 'SELECT n + 1'").expect("function");
    assert_eq!(
        rows(
            &mut c,
            "SELECT 'add_one'::regproc::text, 'add_one(integer, text)'::regprocedure::text, 'add_one'::regproc = 'add_one(int, text)'::regprocedure::oid::regproc"
        ),
        [["add_one", "add_one(integer,text)", "t"]]
    );
}

#[test]
fn update_pg_settings_and_system_viewdefs() {
    let (_db, mut c) = setup();
    c.batch_execute("UPDATE pg_settings SET setting = 'via_update' WHERE name = 'application_name'").expect("update pg_settings");
    assert_eq!(rows(&mut c, "SHOW application_name"), [["via_update"]]);
    let e = expect_code(c.batch_execute("UPDATE pg_settings SET setting = 'x' WHERE name = 'application_name' RETURNING name"), "0A000");
    assert_eq!(message(&e), "cannot perform UPDATE RETURNING on relation \"pg_settings\"");
    assert_eq!(
        rows(
            &mut c,
            "SELECT left(pg_get_viewdef('pg_roles'::regclass), 30), position('information_schema.sql_identifier' in pg_get_viewdef('information_schema.tables'::regclass)) > 0"
        ),
        [[" SELECT pg_authid.rolname,\n   ", "t"]]
    );
}

#[test]
fn collations_and_element_types() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE coll (a text COLLATE \"C\", b text, c varchar(3)[] COLLATE \"POSIX\", d int[])").expect("create");
    assert_eq!(
        rows(
            &mut c,
            "SELECT attname::text, attcollation::int FROM pg_attribute WHERE attrelid = 'coll'::regclass AND attnum > 0 ORDER BY attnum"
        ),
        [["a", "950"], ["b", "100"], ["c", "951"], ["d", "0"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT column_name::text, coalesce(collation_name::text, '-') FROM information_schema.columns WHERE table_name = 'coll' ORDER BY ordinal_position"
        ),
        [["a", "C"], ["b", "-"], ["c", "POSIX"], ["d", "-"]]
    );
    let e = expect_code(c.batch_execute("CREATE TABLE bad (a int COLLATE \"C\")"), "42804");
    assert_eq!(message(&e), "collations are not supported by type integer");
    let e = expect_code(c.batch_execute("CREATE TABLE bad (a text COLLATE \"c\")"), "42704");
    assert_eq!(message(&e), "collation \"c\" for encoding \"UTF8\" does not exist");
    assert_eq!(
        rows(
            &mut c,
            "SELECT c.column_name::text, e.data_type::text, e.udt_name::text FROM information_schema.columns c \
             JOIN information_schema.element_types e ON (c.table_catalog, c.table_schema, c.table_name, 'TABLE', c.dtd_identifier) \
               = (e.object_catalog, e.object_schema, e.object_name, e.object_type, e.collection_type_identifier) \
             WHERE c.table_name = 'coll' ORDER BY 1"
        ),
        [["c", "character varying", "varchar"], ["d", "integer", "int4"]]
    );
}

#[test]
fn on_conflict_expression_keys_and_varchar_assignment() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE acc (id int, org int, email text, code varchar(3));
         CREATE UNIQUE INDEX acc_org_email ON acc (org, lower(email));
         INSERT INTO acc VALUES (1, 1, 'A@x', 'a')",
    )
    .expect("setup");
    assert_eq!(
        rows(
            &mut c,
            "INSERT INTO acc VALUES (2, 1, 'a@X') ON CONFLICT (lower(email), org) DO UPDATE SET id = acc.id + 10 RETURNING id::text, email"
        ),
        [["11", "A@x"]]
    );
    assert_eq!(
        rows(&mut c, "INSERT INTO acc VALUES (3, 1, 'a@X') ON CONFLICT ((lower(email)), org) DO NOTHING RETURNING id::text"),
        Vec::<Vec<String>>::new()
    );
    let e = expect_code(c.batch_execute("INSERT INTO acc VALUES (4, 1, 'a@X') ON CONFLICT (lower(email)) DO NOTHING"), "42P10");
    assert_eq!(message(&e), "there is no unique or exclusion constraint matching the ON CONFLICT specification");
    // Assigning a computed string longer than varchar(n) fails like a constant one does.
    let e = expect_code(c.batch_execute("INSERT INTO acc (id, code) VALUES (5, 'ab'::text || 'cd')"), "22001");
    assert_eq!(message(&e), "value too long for type character varying(3)");
    let e = expect_code(c.batch_execute("UPDATE acc SET code = email || email"), "22001");
    assert_eq!(message(&e), "value too long for type character varying(3)");
    assert_eq!(rows(&mut c, "SELECT ('ab'::text || 'cd')::varchar(3)"), [["abc"]]);
}

/// What psql's `\d`, `\dp` and `\dD` and sqlx's enum support need from the server.
#[test]
fn client_catalog_support() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE mood AS ENUM ('happy', 'sad');
         CREATE DOMAIN positive AS int CHECK (VALUE > 0);
         CREATE TABLE notes (id int, m mood, updated timestamptz);
         CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER notes_touch BEFORE UPDATE ON notes FOR EACH ROW EXECUTE FUNCTION touch();
         CREATE ROLE note_reader NOLOGIN;",
    )
    .expect("setup");
    let user: String = c.query_one("SELECT current_user::text", &[]).expect("user").get(0);
    assert_eq!(rows(&mut c, "SELECT relacl IS NULL FROM pg_class WHERE relname = 'notes'"), [["t"]]);
    c.batch_execute("GRANT SELECT, UPDATE ON notes TO note_reader").expect("grant");
    assert_eq!(
        rows(&mut c, "SELECT array_to_string(relacl, ' ') FROM pg_class WHERE relname = 'notes'"),
        [[format!("{user}=arwdDxtm/{user} note_reader=rw/{user}")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT pg_get_triggerdef(oid), pg_get_triggerdef(oid, true) FROM pg_trigger WHERE tgname = 'notes_touch'"),
        [[
            "CREATE TRIGGER notes_touch BEFORE UPDATE ON public.notes FOR EACH ROW EXECUTE FUNCTION touch()",
            "CREATE TRIGGER notes_touch BEFORE UPDATE ON notes FOR EACH ROW EXECUTE FUNCTION touch()"
        ]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT pg_get_constraintdef(oid), pg_get_constraintdef(oid, true) FROM pg_constraint WHERE contypid = 'positive'::regtype"
        ),
        [["CHECK ((VALUE > 0))", "CHECK (VALUE > 0)"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT (SELECT count(*) FROM pg_statistic_ext)::text, (SELECT count(*) FROM pg_publication)::text, \
             pg_relation_is_publishable('notes'::regclass)::text"
        ),
        [["0", "0", "true"]]
    );
    c.batch_execute("DROP OWNED BY note_reader; DROP ROLE note_reader").expect("cleanup");
    // A parameter declared with a user type's OID (as sqlx does for enums).
    let oid: u32 = c.query_one("SELECT 'mood'::regtype::oid", &[]).expect("oid").get(0);
    let mood =
        postgres::types::Type::new("mood".into(), oid, postgres::types::Kind::Enum(vec!["happy".into(), "sad".into()]), "public".into());
    let stmt = c.prepare_typed("SELECT $1::text", std::slice::from_ref(&mood)).expect("prepare with a user type OID");
    assert_eq!(stmt.params(), [mood]);
}

/// `pg_get_triggerdef` qualifies the trigger function exactly when the search path would not
/// find it by its bare name (ruleutils `generate_function_name`).
#[test]
fn triggerdef_qualifies_function_by_visibility() {
    let (_db, mut c) = setup();
    c.batch_execute(
        r#"CREATE SCHEMA app; CREATE SCHEMA "Odd s";
           CREATE FUNCTION app.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
           CREATE FUNCTION public.pt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
           CREATE FUNCTION "Odd s"."My F"() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
           CREATE TABLE app.t (id int);
           CREATE TRIGGER tr BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION app.touch();
           CREATE TRIGGER tr2 BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION public.pt();
           CREATE TRIGGER tr3 BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION "Odd s"."My F"();"#,
    )
    .expect("setup");
    let q =
        "SELECT tgname, pg_get_triggerdef(oid), pg_get_triggerdef(oid, true) FROM pg_trigger WHERE tgrelid = 'app.t'::regclass ORDER BY 1";
    assert_eq!(
        rows(&mut c, q),
        [
            [
                "tr",
                "CREATE TRIGGER tr BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION app.touch()",
                "CREATE TRIGGER tr BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION app.touch()"
            ],
            [
                "tr2",
                "CREATE TRIGGER tr2 BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION pt()",
                "CREATE TRIGGER tr2 BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION pt()"
            ],
            [
                "tr3",
                "CREATE TRIGGER tr3 BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION \"Odd s\".\"My F\"()",
                "CREATE TRIGGER tr3 BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION \"Odd s\".\"My F\"()"
            ],
        ]
    );
    c.batch_execute("SET search_path = app, public").expect("path");
    assert_eq!(
        rows(&mut c, q),
        [
            [
                "tr",
                "CREATE TRIGGER tr BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION touch()",
                "CREATE TRIGGER tr BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION touch()"
            ],
            [
                "tr2",
                "CREATE TRIGGER tr2 BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION pt()",
                "CREATE TRIGGER tr2 BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION pt()"
            ],
            [
                "tr3",
                "CREATE TRIGGER tr3 BEFORE UPDATE ON app.t FOR EACH ROW EXECUTE FUNCTION \"Odd s\".\"My F\"()",
                "CREATE TRIGGER tr3 BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION \"Odd s\".\"My F\"()"
            ],
        ]
    );
}

/// `ALTER TRIGGER ... RENAME TO` and `ALTER TABLE ... ENABLE / DISABLE [REPLICA | ALWAYS] TRIGGER`.
#[test]
fn alter_trigger_rename_and_enable_modes() {
    let (_db, mut c) = setup();
    c.batch_execute(
        r#"CREATE TABLE t (id int); CREATE TABLE log (msg text);
           CREATE FUNCTION f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO log VALUES (TG_NAME); RETURN NEW; END $$;
           CREATE TRIGGER tr AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
           CREATE TRIGGER tr2 AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();
           CREATE TRIGGER tr3 AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION f();"#,
    )
    .expect("setup");
    c.batch_execute("ALTER TRIGGER tr ON t RENAME TO tr_new").expect("rename");
    c.batch_execute(
        "ALTER TABLE t DISABLE TRIGGER tr2; ALTER TABLE t ENABLE REPLICA TRIGGER tr3; ALTER TABLE t ENABLE ALWAYS TRIGGER tr_new",
    )
    .expect("modes");
    let q = "SELECT tgname, tgenabled FROM pg_trigger WHERE tgrelid = 't'::regclass ORDER BY 1";
    assert_eq!(rows(&mut c, q), [["tr2", "D"], ["tr3", "R"], ["tr_new", "A"]]);
    assert_eq!(
        rows(&mut c, "SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = 'tr_new'"),
        [["CREATE TRIGGER tr_new AFTER INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION f()"]]
    );
    // Origin role: ALWAYS fires, REPLICA and DISABLED do not.
    c.batch_execute("INSERT INTO t VALUES (1)").expect("insert");
    assert_eq!(rows(&mut c, "SELECT msg FROM log ORDER BY 1"), [["tr_new"]]);
    // Replica role: ALWAYS and REPLICA fire, ORIGIN and DISABLED do not.
    c.batch_execute("ALTER TABLE t ENABLE TRIGGER tr2; TRUNCATE log; SET session_replication_role = replica; INSERT INTO t VALUES (2); SET session_replication_role = origin")
        .expect("replica");
    assert_eq!(rows(&mut c, "SELECT msg FROM log ORDER BY 1"), [["tr3"], ["tr_new"]]);
    c.batch_execute("ALTER TABLE t DISABLE TRIGGER USER").expect("disable user");
    assert_eq!(rows(&mut c, q), [["tr2", "D"], ["tr3", "D"], ["tr_new", "D"]]);
    c.batch_execute("ALTER TABLE t ENABLE TRIGGER ALL").expect("enable all");
    assert_eq!(rows(&mut c, q), [["tr2", "O"], ["tr3", "O"], ["tr_new", "O"]]);
    let e = expect_code(c.batch_execute("ALTER TRIGGER nope ON t RENAME TO x"), "42704");
    assert_eq!(message(&e), "trigger \"nope\" for table \"t\" does not exist");
    let e = expect_code(c.batch_execute("ALTER TRIGGER tr2 ON t RENAME TO tr3"), "42710");
    assert_eq!(message(&e), "trigger \"tr3\" for relation \"t\" already exists");
    let e = expect_code(c.batch_execute("ALTER TABLE t ENABLE TRIGGER nope"), "42704");
    assert_eq!(message(&e), "trigger \"nope\" for table \"t\" does not exist");
}

#[test]
fn pg_constraint_connamespace_follows_owner() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA app; CREATE TABLE app.t (id int PRIMARY KEY, CHECK (id > 0)); \
         CREATE DOMAIN app.pos AS int CHECK (VALUE > 0); CREATE TABLE pub_t (id int PRIMARY KEY, CHECK (id > 0))",
    )
    .expect("setup");
    assert_eq!(
        rows(
            &mut c,
            "SELECT conname, connamespace::regnamespace::text FROM pg_constraint WHERE conrelid = 'app.t'::regclass ORDER BY conname"
        ),
        [["t_id_check", "app"], ["t_id_not_null", "app"], ["t_pkey", "app"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT conname, connamespace::regnamespace::text FROM pg_constraint WHERE contypid = 'app.pos'::regtype"),
        [["pos_check", "app"]]
    );
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_constraint WHERE connamespace = 'app'::regnamespace"), [["4"]]);
    assert_eq!(
        rows(&mut c, "SELECT DISTINCT connamespace::regnamespace::text FROM pg_constraint WHERE conrelid = 'pub_t'::regclass"),
        [["public"]]
    );
}

/// The deparser quotes every keyword that is not unreserved, exactly as `quote_ident` does.
#[test]
fn deparse_quotes_every_non_unreserved_keyword() {
    let (_db, mut c) = setup();
    let kws: Vec<String> =
        rows(&mut c, "SELECT word FROM pg_get_keywords() WHERE catcode <> 'U' ORDER BY 1").into_iter().map(|r| r[0].clone()).collect();
    assert!(kws.len() > 100);
    for kw in kws {
        c.batch_execute(&format!("CREATE TABLE kwt (\"{kw}\" int); CREATE INDEX kwt_i ON kwt (\"{kw}\")")).expect("create");
        let def = rows(&mut c, "SELECT pg_get_indexdef('kwt_i'::regclass)");
        assert_eq!(def, [[format!("CREATE INDEX kwt_i ON public.kwt USING btree (\"{kw}\")")]], "{kw}");
        c.batch_execute("DROP TABLE kwt").expect("drop");
    }
    c.batch_execute("CREATE TABLE kwu (abort int, \"a$b\" int); CREATE INDEX kwu_i ON kwu (abort, \"a$b\")").expect("create");
    assert_eq!(
        rows(&mut c, "SELECT pg_get_indexdef('kwu_i'::regclass)"),
        [["CREATE INDEX kwu_i ON public.kwu USING btree (abort, \"a$b\")"]]
    );
}
