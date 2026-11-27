//! Schemas: CREATE/ALTER/DROP SCHEMA, search_path resolution, qualified names, temporary
//! tables and how catalogs and deparsed definitions name objects in other schemas.

use crate::support::{expect_code, message, setup, text_value, texts};

#[test]
fn search_path_resolves_and_chooses_the_creation_schema() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA app; CREATE TABLE app.items (id int, name text); INSERT INTO app.items VALUES (1, 'in app');
         CREATE TABLE public.items (id int, name text); INSERT INTO public.items VALUES (2, 'in public')",
    )
    .expect("setup");
    assert_eq!(text_value(&mut c, "SELECT name FROM items"), Some("in public".into()));
    assert_eq!(text_value(&mut c, "SELECT current_schema()"), Some("public".into()));
    c.batch_execute("SET search_path TO app, public").expect("set");
    assert_eq!(text_value(&mut c, "SELECT name FROM items"), Some("in app".into()));
    assert_eq!(text_value(&mut c, "SELECT current_schemas(true)::text"), Some("{pg_catalog,app,public}".into()));
    c.batch_execute("CREATE TABLE created_here (a int)").expect("create");
    assert_eq!(
        text_value(&mut c, "SELECT table_schema FROM information_schema.tables WHERE table_name = 'created_here'"),
        Some("app".into())
    );
    c.batch_execute("SET search_path TO nosuch").expect("set");
    let e = expect_code(c.simple_query("CREATE TABLE nowhere (a int)"), "3F000");
    assert_eq!(message(&e), "no schema has been selected to create in");
    assert_eq!(text_value(&mut c, "SELECT current_schema()"), None);
    // Queries report the relation, DDL the schema.
    let e = expect_code(c.simple_query("SELECT * FROM nosuch.t"), "42P01");
    assert_eq!(message(&e), "relation \"nosuch.t\" does not exist");
    let e = expect_code(c.simple_query("DROP TABLE nosuch.t"), "3F000");
    assert_eq!(message(&e), "schema \"nosuch\" does not exist");
    c.batch_execute("DROP TABLE IF EXISTS nosuch.t").expect("if exists");
    // pg_catalog is searched first even when not listed.
    assert_eq!(text_value(&mut c, "SELECT count(*) > 0 FROM pg_class"), Some("t".into()));
}

#[test]
fn create_schema_with_elements_and_views_keep_their_names() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA billing CREATE TABLE invoices (id int PRIMARY KEY, total numeric) CREATE VIEW big AS SELECT id FROM invoices WHERE total > 100;
         INSERT INTO billing.invoices VALUES (1, 50), (2, 500)",
    )
    .expect("schema with elements");
    // The view resolves `invoices` as at creation, whatever the current search path.
    assert_eq!(text_value(&mut c, "SELECT id FROM billing.big"), Some("2".into()));
    assert_eq!(
        text_value(&mut c, "SELECT pg_get_viewdef('billing.big'::regclass)"),
        Some(" SELECT id\n   FROM billing.invoices\n  WHERE (total > (100)::numeric);".into())
    );
    assert_eq!(text_value(&mut c, "SELECT 'billing.invoices'::regclass::text"), Some("billing.invoices".into()));
    let e = expect_code(c.simple_query("CREATE SCHEMA billing"), "42P06");
    assert_eq!(message(&e), "schema \"billing\" already exists");
    let e = expect_code(c.simple_query("CREATE SCHEMA pg_mine"), "42939");
    assert_eq!(message(&e), "unacceptable schema name \"pg_mine\"");
}

#[test]
fn drop_and_rename_schemas() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA app; CREATE TABLE app.t (id serial PRIMARY KEY); CREATE TABLE app.u (a int);
         CREATE TABLE public.child (tid int REFERENCES app.t)",
    )
    .expect("setup");
    let e = expect_code(c.simple_query("DROP SCHEMA app"), "2BP01");
    assert_eq!(message(&e), "cannot drop schema app because other objects depend on it");
    let e = expect_code(c.simple_query("DROP SCHEMA nosuch"), "3F000");
    assert_eq!(message(&e), "schema \"nosuch\" does not exist");
    c.batch_execute("ALTER SCHEMA app RENAME TO app2").expect("rename");
    // The serial default follows the schema.
    assert_eq!(text_value(&mut c, "INSERT INTO app2.t DEFAULT VALUES RETURNING id"), Some("1".into()));
    assert_eq!(text_value(&mut c, "SELECT pg_get_serial_sequence('app2.t', 'id')"), Some("app2.t_id_seq".into()));
    c.batch_execute("DROP SCHEMA app2 CASCADE").expect("drop cascade");
    assert_eq!(text_value(&mut c, "SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'app%'"), Some("0".into()));
    // The foreign key referencing the dropped table went with it.
    assert_eq!(
        text_value(&mut c, "SELECT count(*) FROM pg_constraint WHERE conrelid = 'child'::regclass AND contype = 'f'"),
        Some("0".into())
    );
}

#[test]
fn set_schema_moves_tables_with_their_sequences() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE ty2 (id serial, v text); INSERT INTO ty2 (v) VALUES ('a'), ('b'); CREATE SCHEMA other; ALTER TABLE ty2 SET SCHEMA other")
        .expect("set schema");
    assert_eq!(text_value(&mut c, "SELECT count(*) FROM other.ty2"), Some("2".into()));
    assert_eq!(text_value(&mut c, "INSERT INTO other.ty2 (v) VALUES ('c') RETURNING id"), Some("3".into()));
    assert_eq!(text_value(&mut c, "SELECT schemaname FROM pg_sequences WHERE sequencename = 'ty2_id_seq'"), Some("other".into()));
    let e = expect_code(c.simple_query("SELECT * FROM ty2"), "42P01");
    assert_eq!(message(&e), "relation \"ty2\" does not exist");
    c.batch_execute("CREATE TYPE mood AS ENUM ('ok'); ALTER TYPE mood SET SCHEMA other").expect("type set schema");
    assert_eq!(text_value(&mut c, "SELECT 'ok'::other.mood::text"), Some("ok".into()));
    assert_eq!(text_value(&mut c, "SELECT pg_typeof('ok'::other.mood)::text"), Some("other.mood".into()));
}

#[test]
fn temporary_tables_are_private_and_shadow_permanent_ones() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE src (x int); INSERT INTO src VALUES (1), (2), (3); CREATE TEMP TABLE src (x int)").expect("temp");
    assert_eq!(
        texts(&mut c, "SELECT (SELECT count(*) FROM src), (SELECT count(*) FROM public.src)"),
        vec![vec![Some("0".into()), Some("3".into())]]
    );
    assert_eq!(
        text_value(&mut c, "SELECT relpersistence FROM pg_class WHERE relname = 'src' AND relnamespace = pg_my_temp_schema()"),
        Some("t".into())
    );
    c.batch_execute("CREATE TEMP TABLE only_mine (a int)").expect("temp");
    let mut other = db.connect();
    let e = expect_code(other.simple_query("SELECT * FROM only_mine"), "42P01");
    assert_eq!(message(&e), "relation \"only_mine\" does not exist");
    c.batch_execute("DROP TABLE src").expect("drop temp");
    assert_eq!(text_value(&mut c, "SELECT count(*) FROM src"), Some("3".into()));
}

#[test]
fn drop_schema_lists_and_drops_functions_in_creation_order() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA fs; CREATE FUNCTION fs.f(a int, b text) RETURNS int LANGUAGE sql AS 'select 1';
         CREATE TABLE fs.t (a int); CREATE PROCEDURE fs.p(x int) LANGUAGE sql AS 'select 1';
         CREATE SEQUENCE fs.q; CREATE FUNCTION fs.g() RETURNS int LANGUAGE sql AS 'select 1';
         CREATE VIEW fs.v AS SELECT 1 AS x; CREATE MATERIALIZED VIEW fs.mv AS SELECT 1 AS x; CREATE TYPE fs.ty AS ENUM ('a'); CREATE DOMAIN fs.d AS int;
         CREATE FUNCTION fs.tf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
         CREATE TRIGGER tr BEFORE INSERT ON fs.t FOR EACH ROW EXECUTE FUNCTION fs.tf()",
    )
    .expect("setup");
    let e = expect_code(c.simple_query("DROP SCHEMA fs"), "2BP01");
    assert_eq!(message(&e), "cannot drop schema fs because other objects depend on it");
    let detail = e.as_db_error().and_then(|d| d.detail()).unwrap_or_default().to_owned();
    assert_eq!(
        detail,
        "function fs.f(integer,text) depends on schema fs\ntable fs.t depends on schema fs\nfunction fs.p(integer) depends on schema fs\n\
         sequence fs.q depends on schema fs\nfunction fs.g() depends on schema fs\nview fs.v depends on schema fs\nmaterialized view fs.mv depends on schema fs\n\
         type fs.ty depends on schema fs\ntype fs.d depends on schema fs\nfunction fs.tf() depends on schema fs"
    );
    c.batch_execute("DROP SCHEMA fs CASCADE").expect("cascade");
    assert_eq!(
        text_value(
            &mut c,
            "SELECT count(*) FROM pg_proc WHERE pronamespace NOT IN (SELECT oid FROM pg_namespace) OR proname IN ('f', 'g', 'p', 'tf')"
        ),
        Some("0".into())
    );
}

#[test]
fn drop_schema_without_functions_keeps_other_schemas_functions() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE SCHEMA gs; CREATE FUNCTION public.keepme() RETURNS int LANGUAGE sql AS 'select 1'; DROP SCHEMA gs")
        .expect("setup");
    assert_eq!(text_value(&mut c, "SELECT keepme()"), Some("1".into()));
}

#[test]
fn pg_temp_schema_is_created_on_demand() {
    // #213: objects created in pg_temp before any temporary table.
    let (_db, mut c) = setup();
    c.batch_execute("CREATE FUNCTION pg_temp.f() RETURNS int LANGUAGE sql AS 'SELECT 1'").expect("function");
    assert_eq!(text_value(&mut c, "SELECT pg_temp.f()"), Some("1".into()));
    assert_eq!(text_value(&mut c, "SELECT pronamespace = pg_my_temp_schema() FROM pg_proc WHERE proname = 'f'"), Some("t".into()));
    let (_db, mut c) = setup();
    c.batch_execute("CREATE VIEW pg_temp.v AS SELECT 1 AS x").expect("view");
    let (_db, mut c2) = setup();
    c2.batch_execute("CREATE SEQUENCE pg_temp.sq").expect("sequence");
    assert_eq!(text_value(&mut c2, "SELECT relpersistence FROM pg_class WHERE relname = 'sq'"), Some("t".into()));
    let (_db, mut c3) = setup();
    c3.batch_execute("CREATE TYPE pg_temp.ty AS (a int)").expect("type");
    assert_eq!(text_value(&mut c3, "SELECT typnamespace = pg_my_temp_schema() FROM pg_type WHERE typname = 'ty'"), Some("t".into()));
    assert_eq!(text_value(&mut c, "SELECT relpersistence FROM pg_class WHERE relname = 'v'"), Some("t".into()));
}
