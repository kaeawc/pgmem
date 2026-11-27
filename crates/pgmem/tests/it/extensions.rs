//! CREATE / DROP EXTENSION and the functions and types of pgcrypto, uuid-ossp, citext,
//! pg_trgm and hstore.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn install(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS pgcrypto; CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"; CREATE EXTENSION IF NOT EXISTS citext;
         CREATE EXTENSION IF NOT EXISTS pg_trgm; CREATE EXTENSION IF NOT EXISTS hstore",
    )
    .expect("extensions");
}

#[test]
fn extension_ddl_and_catalog() {
    let (_db, mut c) = setup();
    install(&mut c);
    let e = expect_code(c.batch_execute("CREATE EXTENSION pgcrypto"), "42710");
    assert_eq!(message(&e), "extension \"pgcrypto\" already exists");
    let e = expect_code(c.batch_execute("CREATE EXTENSION no_such_extension"), "0A000");
    assert_eq!(message(&e), "extension \"no_such_extension\" is not available");
    assert_eq!(
        rows(
            &mut c,
            "SELECT extname FROM pg_extension WHERE extname IN ('citext', 'hstore', 'pg_trgm', 'pgcrypto', 'uuid-ossp') ORDER BY 1"
        ),
        [["citext"], ["hstore"], ["pg_trgm"], ["pgcrypto"], ["uuid-ossp"]]
    );
    c.batch_execute("CREATE TABLE members (email citext PRIMARY KEY)").expect("citext column");
    let e = expect_code(c.batch_execute("DROP EXTENSION citext"), "2BP01");
    assert_eq!(message(&e), "cannot drop extension citext because other objects depend on it");
    c.batch_execute("DROP EXTENSION hstore; DROP EXTENSION IF EXISTS hstore").expect("drop");
    let e = expect_code(c.batch_execute("DROP EXTENSION hstore"), "42704");
    assert_eq!(message(&e), "extension \"hstore\" does not exist");
}

#[test]
fn pgcrypto_and_uuid_ossp() {
    let (_db, mut c) = setup();
    install(&mut c);
    assert_eq!(
        rows(
            &mut c,
            "SELECT encode(digest('abc', 'sha256'), 'hex'), encode(digest('abc', 'sha1'), 'hex'), encode(hmac('data', 'key', 'sha1'), 'hex')"
        ),
        [[
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            "104152c5bfdca07bc633eebd46199f0255c9f49d"
        ]]
    );
    let e = expect_code(c.batch_execute("SELECT digest('x', 'nope')"), "22023");
    assert_eq!(message(&e), "Cannot use \"nope\": No such hash algorithm");
    assert_eq!(rows(&mut c, "SELECT crypt('pw', '$1$abcdefgh$')"), [["$1$abcdefgh$IQtUouv7y7Q9dRWkQEPCc."]]);
    // A bcrypt hash verifies with crypt(password, hash), as applications check passwords.
    assert_eq!(
        rows(&mut c, "SELECT crypt('hunter2', h) = h, crypt('wrong', h) = h FROM (SELECT crypt('hunter2', gen_salt('bf', 4)) AS h) s"),
        [["t", "f"]]
    );
    assert_eq!(rows(&mut c, "SELECT length(gen_random_bytes(16)), substr(gen_salt('bf'), 1, 7)"), [["16", "$2a$06$"]]);
    assert_eq!(
        rows(
            &mut c,
            "SELECT uuid_generate_v5(uuid_ns_url(), 'https://example.com'), uuid_generate_v3(uuid_ns_dns(), 'example.com'), uuid_nil()"
        ),
        [["4fd35a71-71ef-5a55-a9d9-aa75c889a6d0", "9073926b-929f-31c2-abc9-fad77ae3e8eb", "00000000-0000-0000-0000-000000000000"]]
    );
    assert_eq!(rows(&mut c, "SELECT uuid_generate_v4() <> uuid_generate_v4(), substr(uuid_generate_v1()::text, 15, 1)"), [["t", "1"]]);
}

#[test]
fn citext_trgm_and_hstore() {
    let (_db, mut c) = setup();
    install(&mut c);
    c.batch_execute("CREATE TABLE members (email citext PRIMARY KEY); INSERT INTO members VALUES ('Alice@Example.com')").expect("setup");
    assert_eq!(rows(&mut c, "SELECT email FROM members WHERE email = 'alice@example.COM'"), [["Alice@Example.com"]]);
    expect_code(c.batch_execute("INSERT INTO members VALUES ('ALICE@example.com')"), "23505");
    assert_eq!(
        rows(
            &mut c,
            "SELECT 'ABC'::citext = 'abc'::citext, 'a'::citext < 'B'::citext, 'abc'::citext LIKE 'A%', pg_typeof('x'::citext)::text"
        ),
        [["t", "t", "t", "citext"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT similarity('word', 'words'), word_similarity('word', 'two words'), 'postgres' % 'postgre', show_trgm('cat')"),
        [["0.5714286", "0.8", "t", "{\"  c\",\" ca\",\"at \",cat}"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT 'a=>1, b=>2'::hstore -> 'b', 'a=>1, b=>2'::hstore ? 'a', 'a=>1'::hstore || 'c=>3'::hstore, akeys('x=>1, y=>2'::hstore), hstore('k', 'v')"
        ),
        [["2", "t", "\"a\"=>\"1\", \"c\"=>\"3\"", "{x,y}", "\"k\"=>\"v\""]]
    );
    assert_eq!(
        rows(&mut c, "SELECT 'b=>2, a=>1, a=>3'::hstore, hstore_to_jsonb('a=>1, b=>NULL')"),
        [["\"a\"=>\"1\", \"b\"=>\"2\"", "{\"a\": \"1\", \"b\": null}"]]
    );
}

/// Functions marked DEPENDS ON EXTENSION go with the extension, but what depends on them
/// (column defaults, triggers) blocks DROP EXTENSION unless CASCADE drops it too (#234).
#[test]
fn drop_extension_checks_dependents_of_member_functions() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE EXTENSION hstore;
         CREATE FUNCTION xdf() RETURNS int LANGUAGE sql AS 'select 1';
         ALTER FUNCTION xdf() DEPENDS ON EXTENSION hstore;
         CREATE TABLE xdt (id int DEFAULT xdf());
         CREATE FUNCTION xdtf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
         ALTER FUNCTION xdtf() DEPENDS ON EXTENSION hstore;
         CREATE TABLE xdu (id int);
         CREATE TRIGGER xdtr BEFORE INSERT ON xdu FOR EACH ROW EXECUTE FUNCTION xdtf()",
    )
    .expect("setup");
    let e = expect_code(c.batch_execute("DROP EXTENSION hstore"), "2BP01");
    let db = e.as_db_error().expect("db error");
    assert_eq!(db.message(), "cannot drop extension hstore because other objects depend on it");
    assert_eq!(
        db.detail(),
        Some("default value for column id of table xdt depends on function xdf()\ntrigger xdtr on table xdu depends on function xdtf()")
    );
    assert_eq!(db.hint(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    c.batch_execute("DROP EXTENSION hstore CASCADE").expect("cascade");
    assert_eq!(
        rows(
            &mut c,
            "SELECT (SELECT count(*) FROM pg_trigger WHERE tgname = 'xdtr'), (SELECT count(*) FROM pg_proc WHERE proname IN ('xdf', 'xdtf')),
                    (SELECT count(*) FROM information_schema.columns WHERE table_name = 'xdt' AND column_default IS NOT NULL)"
        ),
        [["0", "0", "0"]]
    );
    c.batch_execute("INSERT INTO xdu VALUES (1); INSERT INTO xdt DEFAULT VALUES").expect("tables still work");
}

fn extension_row(c: &mut postgres::Client, name: &str) -> Vec<Vec<String>> {
    rows(
        c,
        &format!("SELECT extversion, extnamespace::regnamespace::text, extrelocatable::text FROM pg_extension WHERE extname = '{name}'"),
    )
}

#[test]
fn alter_extension_update() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE EXTENSION citext VERSION '1.4'").expect("create");
    assert_eq!(extension_row(&mut c, "citext"), [["1.4", "public", "true"]]);
    // UPDATE without TO goes to the default version; again it is a notice, not an error.
    c.batch_execute("ALTER EXTENSION citext UPDATE").expect("update");
    assert_eq!(extension_row(&mut c, "citext")[0][0], "1.8");
    c.batch_execute("ALTER EXTENSION citext UPDATE; ALTER EXTENSION citext UPDATE TO '1.8'").expect("already current");
    for target in ["1.5", "9.9", "1.1"] {
        let e = expect_code(c.batch_execute(&format!("ALTER EXTENSION citext UPDATE TO '{target}'")), "22023");
        assert_eq!(message(&e), format!("extension \"citext\" has no update path from version \"1.8\" to version \"{target}\""));
    }
    c.batch_execute("DROP EXTENSION citext; CREATE EXTENSION hstore VERSION '1.5'").expect("hstore");
    c.batch_execute("ALTER EXTENSION hstore UPDATE TO '1.7'").expect("explicit target");
    assert_eq!(extension_row(&mut c, "hstore")[0][0], "1.7");
    let e = expect_code(c.batch_execute("ALTER EXTENSION no_such_extension UPDATE"), "42704");
    assert_eq!(message(&e), "extension \"no_such_extension\" does not exist");
    let e = expect_code(c.batch_execute("ALTER EXTENSION plpgsql UPDATE TO '1.1'"), "22023");
    assert_eq!(message(&e), "extension \"plpgsql\" has no update path from version \"1.0\" to version \"1.1\"");
}

#[test]
fn alter_extension_set_schema() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE EXTENSION citext; CREATE SCHEMA s1").expect("setup");
    c.batch_execute("ALTER EXTENSION citext SET SCHEMA s1").expect("move");
    assert_eq!(extension_row(&mut c, "citext"), [["1.8", "s1", "true"]]);
    c.batch_execute("ALTER EXTENSION citext SET SCHEMA s1").expect("same schema");
    let e = expect_code(c.batch_execute("ALTER EXTENSION citext SET SCHEMA nope"), "3F000");
    assert_eq!(message(&e), "schema \"nope\" does not exist");
    let e = expect_code(c.batch_execute("ALTER EXTENSION plpgsql SET SCHEMA s1"), "0A000");
    assert_eq!(message(&e), "extension \"plpgsql\" does not support SET SCHEMA");
    // Members move with the extension; one outside its schema blocks the move.
    c.batch_execute(
        "CREATE SCHEMA s2; CREATE TABLE s1.t1 (a int); CREATE SEQUENCE s1.q1; CREATE TYPE s1.ty1 AS ENUM ('a');
         ALTER EXTENSION citext ADD TABLE s1.t1; ALTER EXTENSION citext ADD SEQUENCE s1.q1; ALTER EXTENSION citext ADD TYPE s1.ty1",
    )
    .expect("members");
    c.batch_execute("ALTER EXTENSION citext SET SCHEMA s2").expect("move with members");
    assert_eq!(
        rows(&mut c, "SELECT relname, relnamespace::regnamespace::text FROM pg_class WHERE relname IN ('t1', 'q1') ORDER BY 1"),
        [["q1", "s2"], ["t1", "s2"]]
    );
    assert_eq!(rows(&mut c, "SELECT typnamespace::regnamespace::text FROM pg_type WHERE typname = 'ty1'"), [["s2"]]);
    c.batch_execute("CREATE TABLE public.outside (a int); ALTER EXTENSION citext ADD TABLE public.outside").expect("outside");
    let e = expect_code(c.batch_execute("ALTER EXTENSION citext SET SCHEMA s1"), "0A000");
    assert_eq!(message(&e), "extension \"citext\" does not support SET SCHEMA");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("table outside is not in the extension's schema \"s2\""));
}

#[test]
fn alter_extension_members() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE EXTENSION hstore; CREATE EXTENSION citext; CREATE SCHEMA s1; CREATE TABLE t1 (a serial PRIMARY KEY);
         CREATE TABLE s1.t2 (a int); CREATE VIEW v1 AS SELECT 1 AS a; CREATE MATERIALIZED VIEW mv1 AS SELECT 1 AS a;
         CREATE DOMAIN d1 AS int; CREATE TYPE ty1 AS ENUM ('a'); CREATE SEQUENCE q1; CREATE SCHEMA s2;
         CREATE FUNCTION f1(int, text) RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE PROCEDURE p1() LANGUAGE sql AS 'SELECT 1'",
    )
    .expect("setup");
    // The member must be of the named kind.
    for (sql, msg) in [
        ("ALTER EXTENSION hstore ADD TABLE v1", "\"v1\" is not a table"),
        ("ALTER EXTENSION hstore ADD VIEW t1", "\"t1\" is not a view"),
        ("ALTER EXTENSION hstore ADD MATERIALIZED VIEW v1", "\"v1\" is not a materialized view"),
        ("ALTER EXTENSION hstore ADD DOMAIN ty1", "\"ty1\" is not a domain"),
        ("ALTER EXTENSION hstore ADD FUNCTION p1()", "p1() is not a function"),
        ("ALTER EXTENSION hstore ADD PROCEDURE f1(int, text)", "f1(integer, text) is not a procedure"),
    ] {
        assert_eq!(message(&expect_code(c.batch_execute(sql), "42809")), msg, "{sql}");
    }
    let e = expect_code(c.batch_execute("ALTER EXTENSION hstore ADD TABLE nope"), "42P01");
    assert_eq!(message(&e), "relation \"nope\" does not exist");
    let e = expect_code(c.batch_execute("ALTER EXTENSION hstore ADD FUNCTION nope()"), "42883");
    assert_eq!(message(&e), "function nope() does not exist");
    let e = expect_code(c.batch_execute("ALTER EXTENSION hstore ADD SCHEMA nope"), "3F000");
    assert_eq!(message(&e), "schema \"nope\" does not exist");
    let e = expect_code(c.batch_execute("ALTER EXTENSION nope ADD TABLE t1"), "42704");
    assert_eq!(message(&e), "extension \"nope\" does not exist");

    c.batch_execute(
        "ALTER EXTENSION hstore ADD TABLE t1; ALTER EXTENSION hstore ADD TABLE s1.t2; ALTER EXTENSION hstore ADD VIEW v1;
         ALTER EXTENSION hstore ADD MATERIALIZED VIEW mv1; ALTER EXTENSION hstore ADD DOMAIN d1; ALTER EXTENSION hstore ADD TYPE ty1;
         ALTER EXTENSION hstore ADD SEQUENCE q1; ALTER EXTENSION hstore ADD SCHEMA s2; ALTER EXTENSION hstore ADD FUNCTION f1(int, text);
         ALTER EXTENSION hstore ADD PROCEDURE p1()",
    )
    .expect("add members");
    for (sql, msg) in [
        ("ALTER EXTENSION hstore ADD TABLE t1", "table t1 is already a member of extension \"hstore\""),
        ("ALTER EXTENSION citext ADD TABLE t1", "table t1 is already a member of extension \"hstore\""),
        ("ALTER EXTENSION hstore ADD TABLE s1.t2", "table s1.t2 is already a member of extension \"hstore\""),
        ("ALTER EXTENSION hstore ADD MATERIALIZED VIEW mv1", "materialized view mv1 is already a member of extension \"hstore\""),
        ("ALTER EXTENSION hstore ADD TYPE d1", "type d1 is already a member of extension \"hstore\""),
        ("ALTER EXTENSION hstore ADD SCHEMA s2", "schema s2 is already a member of extension \"hstore\""),
        ("ALTER EXTENSION hstore ADD FUNCTION f1", "function f1(integer,text) is already a member of extension \"hstore\""),
    ] {
        assert_eq!(message(&expect_code(c.batch_execute(sql), "55000")), msg, "{sql}");
    }
    // Dropping a member on its own is refused.
    for (sql, msg) in [
        ("DROP TABLE t1", "cannot drop table t1 because extension hstore requires it"),
        ("DROP TABLE s1.t2", "cannot drop table s1.t2 because extension hstore requires it"),
        ("DROP VIEW v1", "cannot drop view v1 because extension hstore requires it"),
        ("DROP MATERIALIZED VIEW mv1", "cannot drop materialized view mv1 because extension hstore requires it"),
        ("DROP DOMAIN d1", "cannot drop type d1 because extension hstore requires it"),
        ("DROP TYPE ty1", "cannot drop type ty1 because extension hstore requires it"),
        ("DROP SEQUENCE q1", "cannot drop sequence q1 because extension hstore requires it"),
        ("DROP SCHEMA s2", "cannot drop schema s2 because extension hstore requires it"),
        ("DROP FUNCTION f1(int, text)", "cannot drop function f1(integer,text) because extension hstore requires it"),
        ("DROP PROCEDURE p1()", "cannot drop function p1() because extension hstore requires it"),
    ] {
        let e = expect_code(c.batch_execute(sql), "2BP01");
        assert_eq!(message(&e), msg, "{sql}");
        assert_eq!(e.as_db_error().and_then(|d| d.hint()), Some("You can drop extension hstore instead."), "{sql}");
    }
    c.batch_execute("DROP TABLE IF EXISTS nope").expect("unrelated drop");
    // DROP ... removes the membership; the object can then be dropped.
    let e = expect_code(c.batch_execute("ALTER EXTENSION citext DROP TABLE t1"), "55000");
    assert_eq!(message(&e), "table t1 is not a member of extension \"citext\"");
    c.batch_execute("ALTER EXTENSION hstore DROP TABLE s1.t2; DROP TABLE s1.t2").expect("detach");
    c.batch_execute("ALTER EXTENSION hstore DROP VIEW v1").expect("detach view");
    let e = expect_code(c.batch_execute("ALTER EXTENSION hstore DROP VIEW v1"), "55000");
    assert_eq!(message(&e), "view v1 is not a member of extension \"hstore\"");
    c.batch_execute("DROP VIEW v1").expect("view");
    // DROP EXTENSION takes the remaining members with it.
    c.batch_execute("DROP EXTENSION hstore").expect("drop extension");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_class WHERE relname IN ('t1', 't2', 'mv1', 'q1', 'v1', 't1_a_seq')"), [["0"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_type WHERE typname IN ('ty1', 'd1')"), [["0"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_proc WHERE proname IN ('f1', 'p1')"), [["0"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_namespace WHERE nspname = 's2'"), [["0"]]);
}
