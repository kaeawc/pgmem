//! `pg_depend` / `pg_rewrite` and the object-address functions (`pg_describe_object`,
//! `pg_identify_object`, `pg_identify_object_as_address`) for user objects, as Postgres 18
//! records them. Toast tables and the internal RI triggers of foreign keys are not modelled.

use crate::support::{setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

/// The rows of `sql` in byte order (the database's collation may order text differently).
fn sorted_rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    let mut r = rows(c, sql);
    r.sort();
    r
}

fn sorted(mut want: Vec<Vec<&str>>) -> Vec<Vec<&str>> {
    want.sort();
    want
}

fn schema(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE DOMAIN posint AS integer CHECK (VALUE > 0);
         CREATE TYPE mood AS ENUM ('sad', 'happy');
         CREATE TYPE pair AS (a int, b mood);
         CREATE TABLE parent (id serial PRIMARY KEY, code text UNIQUE, m mood);
         CREATE TABLE child (id integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY, parent_id integer REFERENCES parent (id), qty posint, note text CHECK (note <> ''));
         CREATE INDEX child_lower_note ON child (lower(note));
         CREATE VIEW v AS SELECT p.id, p.code, c.qty FROM parent p JOIN child c ON c.parent_id = p.id;
         CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER t BEFORE INSERT ON child FOR EACH ROW EXECUTE FUNCTION trg();
         CREATE FUNCTION f(x mood) RETURNS posint LANGUAGE sql AS 'SELECT 1';
         CREATE MATERIALIZED VIEW mv AS SELECT id, lower(code) AS lc FROM parent;
         CREATE TABLE gen (a int, b int GENERATED ALWAYS AS (a * 2) STORED, c int DEFAULT 5, d pair);
         CREATE VIEW v3 AS SELECT f('sad'::mood) AS x, count(*) AS n FROM gen;
         CREATE TABLE pt (k int, x int) PARTITION BY RANGE (k);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10);",
    )
    .expect("schema");
}

/// Dependencies of (or on) user objects, minus toast tables and RI triggers.
const DEPENDS: &str = "SELECT pg_describe_object(classid, objid, objsubid), pg_describe_object(refclassid, refobjid, refobjsubid), deptype
    FROM pg_depend WHERE (objid >= 16384 OR refobjid >= 16384)
    AND pg_describe_object(classid, objid, objsubid) !~ 'toast|RI_ConstraintTrigger' ";

#[test]
fn pg_depend_records_user_object_dependencies() {
    let (_db, mut c) = setup();
    schema(&mut c);
    let want: Vec<Vec<&str>> = vec![
        vec!["column b of composite type pair", "type mood", "n"],
        vec!["column d of table gen", "type pair", "n"],
        vec!["column k of table pt", "table pt", "i"],
        vec!["column m of table parent", "type mood", "n"],
        vec!["column qty of table child", "type posint", "n"],
        vec!["column qty of view v", "type posint", "n"],
        vec!["column x of view v3", "type posint", "n"],
        vec!["composite type pair", "type pair", "i"],
        vec!["constraint child_id_not_null on table child", "column id of table child", "a"],
        vec!["constraint child_note_check on table child", "column note of table child", "a"],
        vec!["constraint child_note_check on table child", "column note of table child", "n"],
        vec!["constraint child_parent_id_fkey on table child", "column id of table parent", "n"],
        vec!["constraint child_parent_id_fkey on table child", "column parent_id of table child", "a"],
        vec!["constraint child_parent_id_fkey on table child", "index parent_pkey", "n"],
        vec!["constraint child_pkey on table child", "column id of table child", "a"],
        vec!["constraint parent_code_key on table parent", "column code of table parent", "a"],
        vec!["constraint parent_id_not_null on table parent", "column id of table parent", "a"],
        vec!["constraint parent_pkey on table parent", "column id of table parent", "a"],
        vec!["constraint posint_check", "type posint", "a"],
        vec!["default value for column b of table gen", "column a of table gen", "n"],
        vec!["default value for column b of table gen", "column b of table gen", "i"],
        vec!["default value for column c of table gen", "column c of table gen", "a"],
        vec!["default value for column id of table parent", "column id of table parent", "a"],
        vec!["default value for column id of table parent", "sequence parent_id_seq", "n"],
        vec!["function f(mood)", "schema public", "n"],
        vec!["function f(mood)", "type mood", "n"],
        vec!["function f(mood)", "type posint", "n"],
        vec!["function trg()", "language plpgsql", "n"],
        vec!["function trg()", "schema public", "n"],
        vec!["index child_lower_note", "column note of table child", "a"],
        vec!["index child_lower_note", "table child", "a"],
        vec!["index child_pkey", "constraint child_pkey on table child", "i"],
        vec!["index parent_code_key", "constraint parent_code_key on table parent", "i"],
        vec!["index parent_pkey", "constraint parent_pkey on table parent", "i"],
        vec!["materialized view mv", "schema public", "n"],
        vec!["rule _RETURN on materialized view mv", "column code of table parent", "n"],
        vec!["rule _RETURN on materialized view mv", "column id of table parent", "n"],
        vec!["rule _RETURN on materialized view mv", "materialized view mv", "i"],
        vec!["rule _RETURN on view v", "column code of table parent", "n"],
        vec!["rule _RETURN on view v", "column id of table parent", "n"],
        vec!["rule _RETURN on view v", "column parent_id of table child", "n"],
        vec!["rule _RETURN on view v", "column qty of table child", "n"],
        vec!["rule _RETURN on view v", "view v", "i"],
        vec!["rule _RETURN on view v3", "function f(mood)", "n"],
        vec!["rule _RETURN on view v3", "table gen", "n"],
        vec!["rule _RETURN on view v3", "type mood", "n"],
        vec!["rule _RETURN on view v3", "view v3", "i"],
        vec!["sequence child_id_seq", "column id of table child", "i"],
        vec!["sequence child_id_seq", "schema public", "n"],
        vec!["sequence parent_id_seq", "column id of table parent", "a"],
        vec!["sequence parent_id_seq", "schema public", "n"],
        vec!["table child", "schema public", "n"],
        vec!["table gen", "schema public", "n"],
        vec!["table parent", "schema public", "n"],
        vec!["table pt", "schema public", "n"],
        vec!["table pt1", "schema public", "n"],
        vec!["table pt1", "table pt", "a"],
        vec!["trigger t on table child", "function trg()", "n"],
        vec!["trigger t on table child", "table child", "a"],
        vec!["type child", "table child", "i"],
        vec!["type child[]", "type child", "i"],
        vec!["type gen", "table gen", "i"],
        vec!["type gen[]", "type gen", "i"],
        vec!["type mood", "schema public", "n"],
        vec!["type mood[]", "type mood", "i"],
        vec!["type mv", "materialized view mv", "i"],
        vec!["type mv[]", "type mv", "i"],
        vec!["type pair", "schema public", "n"],
        vec!["type pair[]", "type pair", "i"],
        vec!["type parent", "table parent", "i"],
        vec!["type parent[]", "type parent", "i"],
        vec!["type posint", "schema public", "n"],
        vec!["type posint[]", "type posint", "i"],
        vec!["type pt", "table pt", "i"],
        vec!["type pt[]", "type pt", "i"],
        vec!["type pt1", "table pt1", "i"],
        vec!["type pt1[]", "type pt1", "i"],
        vec!["type v", "view v", "i"],
        vec!["type v[]", "type v", "i"],
        vec!["type v3", "view v3", "i"],
        vec!["type v3[]", "type v3", "i"],
        vec!["view v", "schema public", "n"],
        vec!["view v3", "schema public", "n"],
    ];
    assert_eq!(sorted_rows(&mut c, DEPENDS), sorted(want));
}

#[test]
fn pg_rewrite_has_a_return_rule_per_view() {
    let (_db, mut c) = setup();
    schema(&mut c);
    let got = rows(
        &mut c,
        "SELECT ev_class::regclass::text, rulename, ev_type, ev_enabled, is_instead, ev_qual, ev_action IS NOT NULL
         FROM pg_rewrite WHERE ev_class >= 16384 ORDER BY 1",
    );
    assert_eq!(
        got,
        vec![
            vec!["mv", "_RETURN", "1", "O", "t", "<>", "t"],
            vec!["v", "_RETURN", "1", "O", "t", "<>", "t"],
            vec!["v3", "_RETURN", "1", "O", "t", "<>", "t"],
        ]
    );
    let has_rules = rows(&mut c, "SELECT relname, relhasrules FROM pg_class WHERE relname IN ('v', 'mv', 'parent') ORDER BY 1");
    assert_eq!(has_rules, vec![vec!["mv", "t"], vec!["parent", "f"], vec!["v", "t"]]);
}

#[test]
fn pg_identify_object_names_dependent_objects() {
    let (_db, mut c) = setup();
    schema(&mut c);
    let got = sorted_rows(
        &mut c,
        "SELECT DISTINCT i.*, a.* FROM pg_depend d, pg_identify_object(d.classid, d.objid, d.objsubid) i,
           pg_identify_object_as_address(d.classid, d.objid, d.objsubid) a
         WHERE d.objid >= 16384 AND i.identity !~ 'toast|RI_ConstraintTrigger'",
    );
    let want: Vec<Vec<&str>> = vec![
        vec!["composite type", "public", "pair", "public.pair", "composite type", "{public,pair}", "{}"],
        vec!["composite type column", "public", "pair", "public.pair.b", "composite type column", "{public,pair,b}", "{}"],
        vec!["default value", "NULL", "NULL", "for public.gen.b", "default value", "{public,gen,b}", "{}"],
        vec!["default value", "NULL", "NULL", "for public.gen.c", "default value", "{public,gen,c}", "{}"],
        vec!["default value", "NULL", "NULL", "for public.parent.id", "default value", "{public,parent,id}", "{}"],
        vec![
            "domain constraint",
            "public",
            "NULL",
            "posint_check on public.posint",
            "domain constraint",
            "{public.posint}",
            "{posint_check}",
        ],
        vec!["function", "public", "NULL", "public.f(public.mood)", "function", "{public,f}", "{public.mood}"],
        vec!["function", "public", "NULL", "public.trg()", "function", "{public,trg}", "{}"],
        vec!["index", "public", "child_lower_note", "public.child_lower_note", "index", "{public,child_lower_note}", "{}"],
        vec!["index", "public", "child_pkey", "public.child_pkey", "index", "{public,child_pkey}", "{}"],
        vec!["index", "public", "parent_code_key", "public.parent_code_key", "index", "{public,parent_code_key}", "{}"],
        vec!["index", "public", "parent_pkey", "public.parent_pkey", "index", "{public,parent_pkey}", "{}"],
        vec!["materialized view", "public", "mv", "public.mv", "materialized view", "{public,mv}", "{}"],
        vec!["rule", "NULL", "NULL", "\"_RETURN\" on public.mv", "rule", "{public,mv,_RETURN}", "{}"],
        vec!["rule", "NULL", "NULL", "\"_RETURN\" on public.v", "rule", "{public,v,_RETURN}", "{}"],
        vec!["rule", "NULL", "NULL", "\"_RETURN\" on public.v3", "rule", "{public,v3,_RETURN}", "{}"],
        vec!["sequence", "public", "child_id_seq", "public.child_id_seq", "sequence", "{public,child_id_seq}", "{}"],
        vec!["sequence", "public", "parent_id_seq", "public.parent_id_seq", "sequence", "{public,parent_id_seq}", "{}"],
        vec!["table", "public", "child", "public.child", "table", "{public,child}", "{}"],
        vec!["table", "public", "gen", "public.gen", "table", "{public,gen}", "{}"],
        vec!["table", "public", "parent", "public.parent", "table", "{public,parent}", "{}"],
        vec!["table", "public", "pt", "public.pt", "table", "{public,pt}", "{}"],
        vec!["table", "public", "pt1", "public.pt1", "table", "{public,pt1}", "{}"],
        vec!["table column", "public", "child", "public.child.qty", "table column", "{public,child,qty}", "{}"],
        vec!["table column", "public", "gen", "public.gen.d", "table column", "{public,gen,d}", "{}"],
        vec!["table column", "public", "parent", "public.parent.m", "table column", "{public,parent,m}", "{}"],
        vec!["table column", "public", "pt", "public.pt.k", "table column", "{public,pt,k}", "{}"],
        vec![
            "table constraint",
            "public",
            "NULL",
            "child_id_not_null on public.child",
            "table constraint",
            "{public,child,child_id_not_null}",
            "{}",
        ],
        vec![
            "table constraint",
            "public",
            "NULL",
            "child_note_check on public.child",
            "table constraint",
            "{public,child,child_note_check}",
            "{}",
        ],
        vec![
            "table constraint",
            "public",
            "NULL",
            "child_parent_id_fkey on public.child",
            "table constraint",
            "{public,child,child_parent_id_fkey}",
            "{}",
        ],
        vec!["table constraint", "public", "NULL", "child_pkey on public.child", "table constraint", "{public,child,child_pkey}", "{}"],
        vec![
            "table constraint",
            "public",
            "NULL",
            "parent_code_key on public.parent",
            "table constraint",
            "{public,parent,parent_code_key}",
            "{}",
        ],
        vec![
            "table constraint",
            "public",
            "NULL",
            "parent_id_not_null on public.parent",
            "table constraint",
            "{public,parent,parent_id_not_null}",
            "{}",
        ],
        vec!["table constraint", "public", "NULL", "parent_pkey on public.parent", "table constraint", "{public,parent,parent_pkey}", "{}"],
        vec!["trigger", "NULL", "NULL", "t on public.child", "trigger", "{public,child,t}", "{}"],
        vec!["type", "public", "_child", "public.child[]", "type", "{public.child[]}", "{}"],
        vec!["type", "public", "child", "public.child", "type", "{public.child}", "{}"],
        vec!["type", "public", "_gen", "public.gen[]", "type", "{public.gen[]}", "{}"],
        vec!["type", "public", "gen", "public.gen", "type", "{public.gen}", "{}"],
        vec!["type", "public", "_mood", "public.mood[]", "type", "{public.mood[]}", "{}"],
        vec!["type", "public", "mood", "public.mood", "type", "{public.mood}", "{}"],
        vec!["type", "public", "_mv", "public.mv[]", "type", "{public.mv[]}", "{}"],
        vec!["type", "public", "mv", "public.mv", "type", "{public.mv}", "{}"],
        vec!["type", "public", "_pair", "public.pair[]", "type", "{public.pair[]}", "{}"],
        vec!["type", "public", "pair", "public.pair", "type", "{public.pair}", "{}"],
        vec!["type", "public", "_parent", "public.parent[]", "type", "{public.parent[]}", "{}"],
        vec!["type", "public", "parent", "public.parent", "type", "{public.parent}", "{}"],
        vec!["type", "public", "_posint", "public.posint[]", "type", "{public.posint[]}", "{}"],
        vec!["type", "public", "posint", "public.posint", "type", "{public.posint}", "{}"],
        vec!["type", "public", "_pt", "public.pt[]", "type", "{public.pt[]}", "{}"],
        vec!["type", "public", "pt", "public.pt", "type", "{public.pt}", "{}"],
        vec!["type", "public", "_pt1", "public.pt1[]", "type", "{public.pt1[]}", "{}"],
        vec!["type", "public", "pt1", "public.pt1", "type", "{public.pt1}", "{}"],
        vec!["type", "public", "_v", "public.v[]", "type", "{public.v[]}", "{}"],
        vec!["type", "public", "v", "public.v", "type", "{public.v}", "{}"],
        vec!["type", "public", "_v3", "public.v3[]", "type", "{public.v3[]}", "{}"],
        vec!["type", "public", "v3", "public.v3", "type", "{public.v3}", "{}"],
        vec!["view", "public", "v", "public.v", "view", "{public,v}", "{}"],
        vec!["view", "public", "v3", "public.v3", "view", "{public,v3}", "{}"],
        vec!["view column", "public", "v", "public.v.qty", "view column", "{public,v,qty}", "{}"],
        vec!["view column", "public", "v3", "public.v3.x", "view column", "{public,v3,x}", "{}"],
    ];
    assert_eq!(got, sorted(want));
}

#[test]
fn object_descriptions_of_builtin_and_missing_objects() {
    let (_db, mut c) = setup();
    let got = rows(
        &mut c,
        "SELECT pg_describe_object('pg_type'::regclass, 23, 0), pg_describe_object('pg_class'::regclass, 999999, 0),
           pg_describe_object('pg_namespace'::regclass, 2200, 0), pg_describe_object('pg_class'::regclass, 'pg_class'::regclass, 0),
           pg_describe_object('pg_type'::regclass, 'int4[]'::regtype, 0), pg_describe_object('pg_language'::regclass, 14, 0)",
    );
    assert_eq!(got, vec![vec!["type integer", "NULL", "schema public", "table pg_class", "type integer[]", "language sql"]]);
    let got = rows(
        &mut c,
        "SELECT i.*, a.* FROM pg_identify_object('pg_type'::regclass, 25, 0) i, pg_identify_object_as_address('pg_type'::regclass, 23, 0) a",
    );
    assert_eq!(got, vec![vec!["type", "pg_catalog", "text", "pg_catalog.text", "type", "{integer}", "{}"]]);
}
