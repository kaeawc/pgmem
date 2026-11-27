//! `reg*` output follows the search path: a name prints bare only when it is the first match
//! for that name along the session's effective path (pg_catalog and the temporary schema
//! implicitly first), and schema-qualified otherwise. Postgres stores `reg*` constants in
//! expressions as OIDs, so deparsed definitions print them per the path in force when asked.

use crate::support::{setup, text_value, texts};
use postgres::Client;

fn one(c: &mut Client, sql: &str) -> String {
    text_value(c, sql).unwrap_or_else(|| panic!("{sql} returned NULL"))
}

fn column(c: &mut Client, sql: &str) -> Vec<String> {
    texts(c, sql).into_iter().map(|r| r[0].clone().unwrap_or_default()).collect()
}

fn defaults(c: &mut Client, table: &str) -> Vec<String> {
    column(c, &format!("SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = '{table}'::regclass ORDER BY adnum"))
}

const SETUP: &str = "
    CREATE SCHEMA app; CREATE SCHEMA other; CREATE SCHEMA \"Odd Schema\";
    CREATE TABLE t (id serial PRIMARY KEY, v text);
    CREATE TABLE app.t (id serial);
    CREATE TABLE app.u (id int);
    CREATE TABLE \"Odd Schema\".\"Odd Table\" (id int);
    CREATE TABLE d (
        a int DEFAULT nextval('app.t_id_seq'::regclass),
        b int DEFAULT nextval('t_id_seq'::regclass),
        c regclass DEFAULT 'app.u',
        e regclass DEFAULT '\"Odd Schema\".\"Odd Table\"'::regclass
    )";

#[test]
fn regclass_output_qualifies_what_the_search_path_does_not_find() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    let names =
        "SELECT 't'::regclass, 'public.t'::regclass, 'app.t'::regclass, 'app.u'::regclass, '\"Odd Schema\".\"Odd Table\"'::regclass";
    assert_eq!(texts(&mut c, names)[0], ["t", "t", "app.t", "app.u", "\"Odd Schema\".\"Odd Table\""].map(|s| Some(s.to_owned())));
    // Off the path: the issue's case.
    c.batch_execute("SET search_path = pg_catalog").expect("set");
    assert_eq!(
        texts(
            &mut c,
            "SELECT 'public.t'::regclass, 'public.t'::regclass::text, 'app.t'::regclass, 'pg_class'::regclass, 'information_schema.columns'::regclass"
        )[0],
        ["public.t", "public.t", "app.t", "pg_class", "information_schema.columns"].map(|s| Some(s.to_owned()))
    );
    // On the path, in either position.
    c.batch_execute("SET search_path = app, public").expect("set");
    assert_eq!(
        texts(
            &mut c,
            "SELECT 'app.t'::regclass, 'public.t'::regclass, 'public.t_id_seq'::regclass, 'app.t_id_seq'::regclass, 'u'::regclass"
        )[0],
        ["t", "public.t", "public.t_id_seq", "t_id_seq", "u"].map(|s| Some(s.to_owned()))
    );
    c.batch_execute("SET search_path = public, app").expect("set");
    assert_eq!(
        texts(&mut c, "SELECT 'app.t'::regclass, 'public.t'::regclass, 'app.u'::regclass")[0],
        ["app.t", "t", "u"].map(|s| Some(s.to_owned()))
    );
    // Names that need quotes keep them, qualified or not.
    c.batch_execute("SET search_path = \"Odd Schema\"").expect("set");
    assert_eq!(one(&mut c, "SELECT '\"Odd Schema\".\"Odd Table\"'::regclass"), "\"Odd Table\"");
    c.batch_execute("SET search_path = public").expect("set");
    assert_eq!(one(&mut c, "SELECT '\"Odd Schema\".\"Odd Table\"'::regclass"), "\"Odd Schema\".\"Odd Table\"");
    // A schema that does not exist contributes nothing.
    c.batch_execute("SET search_path = nope, app").expect("set");
    assert_eq!(one(&mut c, "SELECT 'app.u'::regclass"), "u");
}

#[test]
fn a_shadowed_relation_prints_qualified_even_when_its_schema_is_on_the_path() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    // app.t hides public.t, and the other way round.
    c.batch_execute("SET search_path = app, public").expect("set");
    assert_eq!(one(&mut c, "SELECT 'public.t'::regclass"), "public.t");
    assert_eq!(one(&mut c, "SELECT 't'::regclass = 'app.t'::regclass"), "t");
    c.batch_execute("SET search_path = public, app").expect("set");
    assert_eq!(one(&mut c, "SELECT 'app.t'::regclass"), "app.t");
    // pg_catalog is searched first unless listed: a user table named like a catalog is hidden
    // by it, or hides it when listed last.
    c.batch_execute("CREATE TABLE public.pg_proc (a int); SET search_path = public").expect("shadow");
    assert_eq!(
        texts(
            &mut c,
            "SELECT 'public.pg_proc'::regclass, 'pg_catalog.pg_proc'::regclass, 'pg_proc'::regclass::oid = 'pg_catalog.pg_proc'::regclass::oid"
        )[0],
        ["public.pg_proc", "pg_proc", "t"].map(|s| Some(s.to_owned()))
    );
    c.batch_execute("SET search_path = public, pg_catalog").expect("set");
    assert_eq!(
        texts(
            &mut c,
            "SELECT 'public.pg_proc'::regclass, 'pg_catalog.pg_proc'::regclass, 'pg_proc'::regclass::oid = 'public.pg_proc'::regclass::oid"
        )[0],
        ["pg_proc", "pg_catalog.pg_proc", "t"].map(|s| Some(s.to_owned()))
    );
}

#[test]
fn temporary_relations_are_always_visible() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int); CREATE TEMP TABLE tt (a serial); CREATE TEMP TABLE t (a int)").expect("setup");
    let temp = one(&mut c, "SELECT nspname FROM pg_namespace WHERE oid = pg_my_temp_schema()");
    let shown = |c: &mut Client, sql: &str| one(c, sql).replace(&temp, "TMP");
    for path in ["DEFAULT", "pg_catalog", "public", "pg_temp, public", "pg_catalog, pg_temp"] {
        c.batch_execute(&format!("SET search_path = {path}")).expect("set");
        assert_eq!(one(&mut c, "SELECT 'tt'::regclass::text"), "tt", "path {path}");
        assert_eq!(one(&mut c, "SELECT 'tt_a_seq'::regclass::text"), "tt_a_seq", "path {path}");
        assert_eq!(shown(&mut c, "SELECT 'pg_temp.t'::regclass::text"), "t", "path {path}");
        // The temporary t hides public.t.
        assert_eq!(one(&mut c, "SELECT 'public.t'::regclass::text"), "public.t", "path {path}");
        assert_eq!(defaults(&mut c, "tt"), ["nextval('tt_a_seq'::regclass)"], "path {path}");
    }
}

#[test]
fn deparsed_expressions_print_regclass_constants_by_visibility() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    c.batch_execute("CREATE VIEW vv AS SELECT 'app.u'::regclass AS r, 'public.t'::regclass AS s").expect("view");
    c.batch_execute("CREATE TABLE ck (id int, CONSTRAINT ck_c CHECK (id::regclass::text <> 'x' AND 'app.u'::regclass::text <> 'y'))")
        .expect("ck");
    let all = |c: &mut Client| {
        (
            defaults(c, "public.d"),
            column(c, "SELECT column_default FROM information_schema.columns WHERE table_name = 'd' ORDER BY ordinal_position"),
            one(c, "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'ck_c'"),
            one(c, "SELECT pg_get_viewdef('public.vv'::regclass)"),
        )
    };
    let strs = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let d_default = strs(&[
        "nextval('app.t_id_seq'::regclass)",
        "nextval('t_id_seq'::regclass)",
        "'app.u'::regclass",
        "'\"Odd Schema\".\"Odd Table\"'::regclass",
    ]);
    let (exprs, info, ck, view) = all(&mut c);
    assert_eq!(exprs, d_default);
    assert_eq!(info, d_default);
    assert_eq!(ck, "CHECK (((((id)::regclass)::text <> 'x'::text) AND (('app.u'::regclass)::text <> 'y'::text)))");
    assert_eq!(view, " SELECT 'app.u'::regclass AS r,\n    't'::regclass AS s;");

    c.batch_execute("SET search_path = pg_catalog").expect("set");
    let (exprs, info, ck, view) = all(&mut c);
    let off = strs(&[
        "nextval('app.t_id_seq'::regclass)",
        "nextval('public.t_id_seq'::regclass)",
        "'app.u'::regclass",
        "'\"Odd Schema\".\"Odd Table\"'::regclass",
    ]);
    assert_eq!(exprs, off);
    assert_eq!(info, off);
    assert_eq!(ck, "CHECK (((((id)::regclass)::text <> 'x'::text) AND (('app.u'::regclass)::text <> 'y'::text)))");
    assert_eq!(view, " SELECT 'app.u'::regclass AS r,\n    'public.t'::regclass AS s;");

    // app first: its t hides public.t, so the default drawing on public's sequence qualifies.
    c.batch_execute("SET search_path = app, public").expect("set");
    let (exprs, info, _, view) = all(&mut c);
    let shadowed = strs(&[
        "nextval('t_id_seq'::regclass)",
        "nextval('public.t_id_seq'::regclass)",
        "'u'::regclass",
        "'\"Odd Schema\".\"Odd Table\"'::regclass",
    ]);
    assert_eq!(exprs, shadowed);
    assert_eq!(info, shadowed);
    assert_eq!(view, " SELECT 'u'::regclass AS r,\n    'public.t'::regclass AS s;");
}

#[test]
fn an_unqualified_default_keeps_the_relation_it_named_when_it_was_stored() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA app; CREATE SEQUENCE s; CREATE TABLE app.s (a int);
         CREATE TABLE w (a int DEFAULT nextval('s'), b int DEFAULT 0, r regclass DEFAULT 's');
         ALTER TABLE w ALTER COLUMN b SET DEFAULT nextval('s'::regclass)",
    )
    .expect("setup");
    // Postgres stores the sequence's OID: a later path does not change which one it is.
    c.batch_execute("SET search_path = app, public").expect("set");
    assert_eq!(defaults(&mut c, "public.w"), ["nextval('public.s'::regclass)", "nextval('public.s'::regclass)", "'public.s'::regclass"]);
    c.batch_execute("SET search_path = pg_catalog").expect("set");
    assert_eq!(defaults(&mut c, "public.w"), ["nextval('public.s'::regclass)", "nextval('public.s'::regclass)", "'public.s'::regclass"]);
    c.batch_execute("SET search_path = public").expect("set");
    assert_eq!(defaults(&mut c, "w"), ["nextval('s'::regclass)", "nextval('s'::regclass)", "'s'::regclass"]);
}

#[test]
fn a_view_prints_its_regclass_constants_by_the_current_path_not_the_creation_path() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE SCHEMA app; CREATE TABLE app.u (id int); CREATE TABLE u (id int); SET search_path = app, public")
        .expect("setup");
    c.batch_execute("CREATE VIEW public.v AS SELECT 'u'::regclass AS a, 'public.u'::regclass AS b").expect("view");
    assert_eq!(one(&mut c, "SELECT pg_get_viewdef('public.v'::regclass)"), " SELECT 'u'::regclass AS a,\n    'public.u'::regclass AS b;");
    c.batch_execute("SET search_path = public").expect("set");
    assert_eq!(one(&mut c, "SELECT pg_get_viewdef('public.v'::regclass)"), " SELECT 'app.u'::regclass AS a,\n    'u'::regclass AS b;");
    c.batch_execute("SET search_path = pg_catalog").expect("set");
    assert_eq!(
        one(&mut c, "SELECT pg_get_viewdef('public.v'::regclass)"),
        " SELECT 'app.u'::regclass AS a,\n    'public.u'::regclass AS b;"
    );
}

#[test]
fn pg_get_serial_sequence_always_qualifies() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    for path in ["public", "app, public", "pg_catalog"] {
        c.batch_execute(&format!("SET search_path = {path}")).expect("set");
        assert_eq!(one(&mut c, "SELECT pg_get_serial_sequence('public.t', 'id')"), "public.t_id_seq", "path {path}");
        assert_eq!(one(&mut c, "SELECT pg_get_serial_sequence('app.t', 'id')"), "app.t_id_seq", "path {path}");
    }
}

#[test]
fn regtype_follows_format_type() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA app; CREATE SCHEMA other;
         CREATE TYPE mood AS ENUM ('a'); CREATE TYPE app.mood2 AS ENUM ('a'); CREATE TYPE other.mood AS ENUM ('b')",
    )
    .expect("setup");
    let sql = "SELECT 'public.mood'::regtype, 'app.mood2'::regtype, 'other.mood'::regtype, 'public.mood[]'::regtype, 'int4'::regtype, 'text'::regtype, 'uuid'::regtype";
    assert_eq!(texts(&mut c, sql)[0], ["mood", "app.mood2", "other.mood", "mood[]", "integer", "text", "uuid"].map(|s| Some(s.to_owned())));
    c.batch_execute("SET search_path = pg_catalog").expect("set");
    assert_eq!(
        texts(&mut c, sql)[0],
        ["public.mood", "app.mood2", "other.mood", "public.mood[]", "integer", "text", "uuid"].map(|s| Some(s.to_owned()))
    );
    c.batch_execute("SET search_path = other, public, app").expect("set");
    assert_eq!(
        texts(&mut c, sql)[0],
        ["public.mood", "mood2", "mood", "public.mood[]", "integer", "text", "uuid"].map(|s| Some(s.to_owned()))
    );
    // From an OID, as catalog queries print them.
    c.batch_execute("SET search_path = public").expect("set");
    assert_eq!(column(&mut c, "SELECT oid::regtype::text FROM pg_type WHERE typname = 'mood' ORDER BY 1"), ["mood", "other.mood"]);
}

#[test]
fn regproc_and_regprocedure_qualify_functions_the_path_does_not_find() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA app; CREATE SCHEMA other;
         CREATE FUNCTION public.f(int) RETURNS int LANGUAGE sql AS 'select $1';
         CREATE FUNCTION app.f(int) RETURNS int LANGUAGE sql AS 'select $1 + 1';
         CREATE FUNCTION other.h(int) RETURNS int LANGUAGE sql AS 'select $1'",
    )
    .expect("setup");
    let procs = "SELECT oid::regproc::text FROM pg_proc WHERE proname IN ('f', 'h') ORDER BY 1";
    let procedures = "SELECT oid::regprocedure::text FROM pg_proc WHERE proname IN ('f', 'h') ORDER BY 1";
    // Default path: app.f is not on it, and the bare name finds public.f only.
    assert_eq!(column(&mut c, procs), ["app.f", "f", "other.h"]);
    assert_eq!(column(&mut c, procedures), ["app.f(integer)", "f(integer)", "other.h(integer)"]);
    c.batch_execute("SET search_path = pg_catalog").expect("set");
    assert_eq!(column(&mut c, procs), ["app.f", "other.h", "public.f"]);
    assert_eq!(column(&mut c, procedures), ["app.f(integer)", "other.h(integer)", "public.f(integer)"]);
    c.batch_execute("SET search_path = other, public, app").expect("set");
    assert_eq!(column(&mut c, procs), ["app.f", "f", "h"]);
    assert_eq!(column(&mut c, procedures), ["app.f(integer)", "f(integer)", "h(integer)"]);
    // A user function with a builtin's signature hides it only when it comes first.
    c.batch_execute("CREATE FUNCTION public.lower(text) RETURNS text LANGUAGE sql AS 'select $1'; SET search_path = public, pg_catalog")
        .expect("shadow");
    assert_eq!(
        texts(
            &mut c,
            "SELECT 'public.lower(text)'::regprocedure, 'pg_catalog.lower(text)'::regprocedure, 'pg_catalog.upper(text)'::regprocedure"
        )[0],
        ["lower(text)", "pg_catalog.lower(text)", "upper(text)"].map(|s| Some(s.to_owned()))
    );
    c.batch_execute("SET search_path = public").expect("set");
    assert_eq!(
        texts(&mut c, "SELECT 'public.lower(text)'::regprocedure, 'pg_catalog.lower(text)'::regprocedure")[0],
        ["public.lower(text)", "lower(text)"].map(|s| Some(s.to_owned()))
    );
}

#[test]
fn regnamespace_and_regrole_print_bare_names() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE SCHEMA app; CREATE SCHEMA \"Odd Schema\"").expect("setup");
    for path in ["public", "pg_catalog"] {
        c.batch_execute(&format!("SET search_path = {path}")).expect("set");
        assert_eq!(
            texts(
                &mut c,
                "SELECT 'public'::regnamespace, 'app'::regnamespace, '\"Odd Schema\"'::regnamespace, 'pg_catalog'::regnamespace, 'pg_read_all_data'::regrole, current_user::text::regrole::text = current_user::text"
            )[0],
            ["public", "app", "\"Odd Schema\"", "pg_catalog", "pg_read_all_data", "t"].map(|s| Some(s.to_owned())),
            "path {path}"
        );
    }
}
