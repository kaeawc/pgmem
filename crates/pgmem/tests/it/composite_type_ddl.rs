//! `ALTER TYPE ... {ADD | DROP | ALTER | RENAME} ATTRIBUTE` on composite types, comments on the
//! attributes of composite types and on domain constraints, and `regclass` for a composite
//! type's relation. Each assertion holds on PostgreSQL 18 too.

use crate::support::{expect_code, message, setup, texts};
use postgres::Client;

/// Rows of `sql`, each row's columns joined with `|` (NULL as `NULL`).
fn rows(c: &mut Client, sql: &str) -> Vec<String> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect::<Vec<_>>().join("|")).collect()
}

fn detail(e: &postgres::Error) -> String {
    e.as_db_error().and_then(|d| d.detail().map(str::to_owned)).unwrap_or_default()
}

/// The live attributes of composite type `ty` as `name type collation`.
fn attributes(c: &mut Client, ty: &str) -> Vec<String> {
    rows(
        c,
        &format!(
            "SELECT a.attname, format_type(a.atttypid, a.atttypmod), coalesce(co.collname, '-') \
             FROM pg_attribute a LEFT JOIN pg_collation co ON co.oid = a.attcollation \
             WHERE a.attrelid = '{ty}'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
        ),
    )
}

#[test]
fn add_and_alter_attribute_with_collations() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE p AS (x int, z text)").expect("setup");
    c.batch_execute("ALTER TYPE p ADD ATTRIBUTE v text COLLATE \"C\"").expect("add collate");
    c.batch_execute("ALTER TYPE p ALTER ATTRIBUTE x TYPE bigint").expect("alter type");
    c.batch_execute("ALTER TYPE p ALTER ATTRIBUTE z SET DATA TYPE varchar(7) COLLATE \"POSIX\" CASCADE").expect("set data type");
    assert_eq!(attributes(&mut c, "p"), vec!["x|bigint|-", "z|character varying(7)|POSIX", "v|text|C"]);
    // Without COLLATE the attribute gets its type's default collation again.
    c.batch_execute("ALTER TYPE p ALTER ATTRIBUTE v TYPE text, ALTER ATTRIBUTE z TYPE int").expect("reset collation");
    assert_eq!(attributes(&mut c, "p"), vec!["x|bigint|-", "z|integer|-", "v|text|default"]);
    // A composite attribute may change to any type: no values are stored in a bare type.
    c.batch_execute("ALTER TYPE p ALTER ATTRIBUTE x TYPE int[], ALTER ATTRIBUTE z TYPE date").expect("arrays and dates");
    assert_eq!(attributes(&mut c, "p"), vec!["x|integer[]|-", "z|date|-", "v|text|default"]);

    let e = expect_code(c.simple_query("ALTER TYPE p ADD ATTRIBUTE i int COLLATE \"C\""), "42804");
    assert_eq!(message(&e), "collations are not supported by type integer");
    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE v TYPE text COLLATE nosuch"), "42704");
    assert_eq!(message(&e), "collation \"nosuch\" for encoding \"UTF8\" does not exist");
    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE v TYPE nosuch"), "42704");
    assert_eq!(message(&e), "type \"nosuch\" does not exist");
    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE nope TYPE int"), "42703");
    assert_eq!(message(&e), "column \"nope\" of relation \"p\" does not exist");
    let e = expect_code(c.simple_query("ALTER TYPE p ADD ATTRIBUTE v int"), "42701");
    assert_eq!(message(&e), "column \"v\" of relation \"p\" already exists");
    // USING and DEFAULT are not part of the composite type grammar.
    expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE x TYPE int USING x"), "42601");
    expect_code(c.simple_query("ALTER TYPE p ADD ATTRIBUTE d int DEFAULT 1"), "42601");
    // A failed statement changes nothing.
    assert_eq!(attributes(&mut c, "p"), vec!["x|integer[]|-", "z|date|-", "v|text|default"]);
}

#[test]
fn attribute_actions_run_in_postgres_passes() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE p AS (x int, z int)").expect("setup");
    // Drops first, so a dropped name can be added again in the same statement.
    c.batch_execute("ALTER TYPE p ADD ATTRIBUTE a int, DROP ATTRIBUTE z, ADD ATTRIBUTE z text").expect("drop then add");
    assert_eq!(attributes(&mut c, "p"), vec!["x|integer|-", "a|integer|-", "z|text|default"]);
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE IF EXISTS nope, ADD ATTRIBUTE k int CASCADE, DROP ATTRIBUTE a RESTRICT")
        .expect("if exists");
    assert_eq!(attributes(&mut c, "p"), vec!["x|integer|-", "z|text|default", "k|integer|-"]);

    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE x TYPE text, ALTER ATTRIBUTE x TYPE int"), "0A000");
    assert_eq!(message(&e), "cannot alter type of column \"x\" twice");
    let e = expect_code(c.simple_query("ALTER TYPE p DROP ATTRIBUTE x, ALTER ATTRIBUTE x TYPE text"), "42703");
    assert_eq!(message(&e), "column \"x\" of relation \"p\" does not exist");
    let e = expect_code(c.simple_query("ALTER TYPE p ADD ATTRIBUTE y int, ALTER ATTRIBUTE y TYPE text"), "42703");
    assert_eq!(message(&e), "column \"y\" of relation \"p\" does not exist");
    let e = expect_code(c.simple_query("ALTER TYPE p ADD ATTRIBUTE y int, ADD ATTRIBUTE y int"), "42701");
    assert_eq!(message(&e), "column \"y\" of relation \"p\" already exists");
    let e = expect_code(c.simple_query("ALTER TYPE p ADD ATTRIBUTE y int, DROP ATTRIBUTE y"), "42703");
    assert_eq!(message(&e), "column \"y\" of relation \"p\" does not exist");
    assert_eq!(attributes(&mut c, "p"), vec!["x|integer|-", "z|text|default", "k|integer|-"]);
}

#[test]
fn attribute_actions_need_a_composite_type() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE p AS (x int); CREATE TYPE e AS ENUM ('a'); CREATE DOMAIN d AS int; CREATE TABLE t (a int); \
         CREATE TYPE q AS (a p)",
    )
    .expect("setup");
    for ty in ["e", "d", "nosuch"] {
        for sql in [
            format!("ALTER TYPE {ty} ADD ATTRIBUTE x int"),
            format!("ALTER TYPE {ty} DROP ATTRIBUTE x"),
            format!("ALTER TYPE {ty} ALTER ATTRIBUTE x TYPE int"),
            format!("ALTER TYPE {ty} RENAME ATTRIBUTE x TO y"),
        ] {
            let e = expect_code(c.simple_query(&sql), "42P01");
            assert_eq!(message(&e), format!("relation \"{ty}\" does not exist"), "{sql}");
        }
    }
    let e = expect_code(c.simple_query("ALTER TYPE t ADD ATTRIBUTE x int"), "42809");
    assert_eq!(message(&e), "\"t\" is not a composite type");
    // A composite type cannot contain itself, directly or through arrays and other types.
    for ty in ["p", "p[]", "q", "q[]"] {
        let e = expect_code(c.simple_query(&format!("ALTER TYPE p ADD ATTRIBUTE m {ty}")), "42P16");
        assert_eq!(message(&e), "composite type p cannot be made a member of itself");
        let e = expect_code(c.simple_query(&format!("ALTER TYPE p ALTER ATTRIBUTE x TYPE {ty}")), "42P16");
        assert_eq!(message(&e), "composite type p cannot be made a member of itself");
    }
}

#[test]
fn rename_attribute() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE p AS (x int, z int)").expect("setup");
    c.batch_execute("ALTER TYPE p RENAME ATTRIBUTE x TO a").expect("rename");
    c.batch_execute("ALTER TYPE p RENAME ATTRIBUTE z TO b CASCADE").expect("rename cascade");
    assert_eq!(attributes(&mut c, "p"), vec!["a|integer|-", "b|integer|-"]);
    let e = expect_code(c.simple_query("ALTER TYPE p RENAME ATTRIBUTE a TO b"), "42701");
    assert_eq!(message(&e), "column \"b\" of relation \"p\" already exists");
    let e = expect_code(c.simple_query("ALTER TYPE p RENAME ATTRIBUTE nope TO c"), "42703");
    assert_eq!(message(&e), "column \"nope\" does not exist");

    // Expressions reading the attribute follow the rename.
    c.batch_execute(
        "CREATE TABLE t (c p, CHECK ((c).a > 0), g int GENERATED ALWAYS AS ((c).b) STORED); \
         CREATE INDEX ti ON t (((c).a)); \
         INSERT INTO t (c) VALUES ((1, 2))",
    )
    .expect("table");
    c.batch_execute("ALTER TYPE p RENAME ATTRIBUTE a TO a2").expect("rename used");
    c.batch_execute("ALTER TYPE p RENAME ATTRIBUTE b TO b2").expect("rename used");
    expect_code(c.simple_query("INSERT INTO t (c) VALUES ((0, 1))"), "23514");
    c.batch_execute("INSERT INTO t (c) VALUES ((5, 6))").expect("insert");
    assert_eq!(rows(&mut c, "SELECT g, (c).a2, (c).b2 FROM t ORDER BY g"), vec!["2|1|2", "6|5|6"]);
}

#[test]
fn alter_attribute_type_is_refused_while_a_column_uses_the_type() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE p AS (x int, z int); CREATE TABLE t (c p); INSERT INTO t VALUES ((1, 2))").expect("setup");
    for sql in [
        "ALTER TYPE p ALTER ATTRIBUTE x TYPE bigint",
        "ALTER TYPE p ALTER ATTRIBUTE x TYPE bigint CASCADE",
        "ALTER TYPE p ALTER ATTRIBUTE x TYPE int",
    ] {
        let e = expect_code(c.simple_query(sql), "0A000");
        assert_eq!(message(&e), "cannot alter type \"p\" because column \"t.c\" uses it", "{sql}");
    }
    // The type check comes first.
    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE x TYPE nosuch"), "42704");
    assert_eq!(message(&e), "type \"nosuch\" does not exist");
    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE nope TYPE int"), "42703");
    assert_eq!(message(&e), "column \"nope\" of relation \"p\" does not exist");
    // The same through an array column, a domain over the type and another composite type.
    c.batch_execute("DROP TABLE t; CREATE TABLE t (c p[])").expect("array");
    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE x TYPE bigint"), "0A000");
    assert_eq!(message(&e), "cannot alter type \"p\" because column \"t.c\" uses it");
    c.batch_execute("DROP TABLE t; CREATE DOMAIN dp AS p; CREATE TABLE t (c dp)").expect("domain");
    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE x TYPE bigint"), "0A000");
    assert_eq!(message(&e), "cannot alter type \"p\" because column \"t.c\" uses it");
    c.batch_execute("DROP TABLE t; DROP DOMAIN dp; CREATE TYPE q AS (a p); CREATE TABLE t (c q)").expect("nested");
    let e = expect_code(c.simple_query("ALTER TYPE p ALTER ATTRIBUTE x TYPE bigint"), "0A000");
    assert_eq!(message(&e), "cannot alter type \"p\" because column \"t.c\" uses it");
    let e = expect_code(c.simple_query("ALTER TYPE q ALTER ATTRIBUTE a TYPE text"), "0A000");
    assert_eq!(message(&e), "cannot alter type \"q\" because column \"t.c\" uses it");
    // Once nothing stores the type, it changes.
    c.batch_execute("DROP TABLE t").expect("drop");
    c.batch_execute("ALTER TYPE p ALTER ATTRIBUTE x TYPE bigint").expect("unused");
    assert_eq!(attributes(&mut c, "p"), vec!["x|bigint|-", "z|integer|-"]);
}

#[test]
fn add_attribute_pads_stored_values_with_null() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE p AS (x int, z int); CREATE TYPE q AS (a p); CREATE DOMAIN dp AS p; \
         CREATE TABLE t (c p, arr p[], d dp, n q); \
         INSERT INTO t VALUES ((1, 2), ARRAY[(3, 4)::p], (5, 6), ROW((7, 8)::p))",
    )
    .expect("setup");
    c.batch_execute("ALTER TYPE p ADD ATTRIBUTE w text").expect("add");
    assert_eq!(rows(&mut c, "SELECT c, arr, d, n, (c).w IS NULL FROM t"), vec!["(1,2,)|{\"(3,4,)\"}|(5,6,)|(\"(7,8,)\")|t"]);
    c.batch_execute("INSERT INTO t (c) VALUES ((9, 9, 'w'))").expect("insert");
    assert_eq!(rows(&mut c, "SELECT (c).w FROM t ORDER BY (c).x"), vec!["NULL", "w"]);
}

#[test]
fn drop_attribute_of_a_type_a_table_uses() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE p AS (x int, z int); CREATE TABLE t (c p); INSERT INTO t VALUES ((1, 2)), (NULL)").expect("setup");
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE z").expect("drop");
    assert_eq!(rows(&mut c, "SELECT c FROM t ORDER BY (c).x"), vec!["(1)", "NULL"]);
    assert_eq!(attributes(&mut c, "p"), vec!["x|integer|-"]);
    // New values have the new shape; the old shape is refused.
    c.batch_execute("INSERT INTO t VALUES (ROW(3))").expect("insert");
    let e = expect_code(c.simple_query("INSERT INTO t VALUES (ROW(4, 5))"), "42846");
    assert_eq!(message(&e), "cannot cast type record to p");
    assert_eq!(detail(&e), "Input has too many columns.");
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE x").expect("drop the last");
    assert_eq!(rows(&mut c, "SELECT c FROM t ORDER BY 1 NULLS LAST"), vec!["()", "()", "NULL"]);
    // Re-adding an attribute starts it out NULL in the stored values.
    c.batch_execute("ALTER TYPE p ADD ATTRIBUTE y text").expect("add");
    assert_eq!(rows(&mut c, "SELECT c FROM t ORDER BY 1 NULLS LAST"), vec!["()", "()", "NULL"]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM t WHERE (c).y IS NULL"), vec!["3"]);
}

#[test]
fn drop_attribute_rewrites_values_nested_in_arrays_domains_and_composites() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE p AS (x int, z int); CREATE TYPE q AS (a p, b int); CREATE DOMAIN dp AS p; \
         CREATE TABLE t (arr p[], d dp, n q, nn q[]); \
         INSERT INTO t VALUES (ARRAY[(1, 2)::p, (3, 4)::p], (5, 6), ROW((7, 8)::p, 9), ARRAY[ROW((10, 11)::p, 12)::q])",
    )
    .expect("setup");
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE x").expect("drop");
    assert_eq!(rows(&mut c, "SELECT arr, d, n, nn FROM t"), vec![r#"{(2),(4)}|(6)|("(8)",9)|{"(\"(11)\",12)"}"#]);
    c.batch_execute("ALTER TYPE q DROP ATTRIBUTE b").expect("drop outer");
    assert_eq!(rows(&mut c, "SELECT n, nn FROM t"), vec![r#"("(8)")|{"(\"(11)\")"}"#]);
}

#[test]
fn drop_attribute_errors() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE p AS (x int)").expect("setup");
    let e = expect_code(c.simple_query("ALTER TYPE p DROP ATTRIBUTE nope"), "42703");
    assert_eq!(message(&e), "column \"nope\" of relation \"p\" does not exist");
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE IF EXISTS nope").expect("if exists");
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE x").expect("drop");
    let e = expect_code(c.simple_query("ALTER TYPE p DROP ATTRIBUTE x, DROP ATTRIBUTE x"), "42703");
    assert_eq!(message(&e), "column \"x\" of relation \"p\" does not exist");
}

/// What the objects reading an attribute say when it is dropped without CASCADE.
fn dependents_of_drop(c: &mut Client, ty: &str, attr: &str) -> (String, String) {
    let e = expect_code(c.simple_query(&format!("ALTER TYPE {ty} DROP ATTRIBUTE {attr}")), "2BP01");
    assert_eq!(message(&e), format!("cannot drop column {attr} of composite type {ty} because other objects depend on it"));
    assert_eq!(e.as_db_error().and_then(|d| d.hint()), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    (message(&e), detail(&e))
}

#[test]
fn drop_attribute_needs_cascade_for_expressions_reading_it() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE p AS (x int, z int); \
         CREATE TABLE t (c p, CHECK ((c).z > 0)); \
         INSERT INTO t VALUES ((1, 2))",
    )
    .expect("setup");
    assert_eq!(dependents_of_drop(&mut c, "p", "z").1, "constraint t_c_check on table t depends on column z of composite type p");
    // Other attributes are not read.
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE x").expect("unrelated attribute");
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE z CASCADE").expect("cascade");
    assert_eq!(rows(&mut c, "SELECT conname FROM pg_constraint WHERE conrelid = 't'::regclass"), Vec::<String>::new());
    assert_eq!(rows(&mut c, "SELECT c FROM t"), vec!["()"]);

    c.batch_execute("CREATE TYPE p2 AS (x int, z int); CREATE TABLE t2 (c p2); CREATE VIEW v AS SELECT (c).z AS zz, (c).x AS xx FROM t2")
        .expect("view");
    assert_eq!(dependents_of_drop(&mut c, "p2", "z").1, "view v depends on column z of composite type p2");
    c.batch_execute("ALTER TYPE p2 DROP ATTRIBUTE z CASCADE").expect("cascade view");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_class WHERE relname = 'v'"), vec!["0"]);

    c.batch_execute("CREATE TYPE p3 AS (x int, z int); CREATE TABLE t3 (c p3); CREATE INDEX t3i ON t3 (((c).z))").expect("index");
    assert_eq!(dependents_of_drop(&mut c, "p3", "z").1, "index t3i depends on column z of composite type p3");
    c.batch_execute("ALTER TYPE p3 DROP ATTRIBUTE z CASCADE").expect("cascade index");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_class WHERE relname = 't3i'"), vec!["0"]);

    c.batch_execute("CREATE TYPE p4 AS (x int, z int); CREATE TABLE t4 (c p4, g int GENERATED ALWAYS AS ((c).z) STORED)")
        .expect("generated");
    assert_eq!(dependents_of_drop(&mut c, "p4", "z").1, "column g of table t4 depends on column z of composite type p4");
    c.batch_execute("ALTER TYPE p4 DROP ATTRIBUTE z CASCADE").expect("cascade generated");
    assert_eq!(
        rows(&mut c, "SELECT attname FROM pg_attribute WHERE attrelid = 't4'::regclass AND attnum > 0 AND NOT attisdropped"),
        vec!["c"]
    );

    c.batch_execute("CREATE TYPE p5 AS (x int, z int); CREATE DOMAIN d5 AS p5 CHECK ((VALUE).z > 0)").expect("domain");
    assert_eq!(dependents_of_drop(&mut c, "p5", "z").1, "constraint d5_check depends on column z of composite type p5");
    c.batch_execute("ALTER TYPE p5 DROP ATTRIBUTE z CASCADE").expect("cascade domain check");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_constraint WHERE contypid = 'd5'::regtype"), vec!["0"]);
}

#[test]
fn comments_on_composite_attributes() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE p AS (x int, z text)").expect("setup");
    c.batch_execute("COMMENT ON COLUMN p.x IS 'the x'").expect("comment");
    c.batch_execute("COMMENT ON COLUMN public.p.z IS 'the z'").expect("qualified");
    assert_eq!(rows(&mut c, "SELECT col_description('p'::regclass, 1), col_description('p'::regclass, 2)"), vec!["the x|the z"]);
    assert_eq!(rows(&mut c, "SELECT obj_description('p'::regclass, 'pg_class')"), vec!["NULL"]);
    assert_eq!(
        rows(
            &mut c,
            "SELECT objoid = 'p'::regclass, classoid::regclass, objsubid, description FROM pg_description \
             WHERE objoid = 'p'::regclass ORDER BY objsubid"
        ),
        vec!["t|pg_class|1|the x", "t|pg_class|2|the z"]
    );
    // The type's own comment is separate.
    c.batch_execute("COMMENT ON TYPE p IS 'the type'").expect("type comment");
    assert_eq!(
        rows(&mut c, "SELECT obj_description('p'::regtype, 'pg_type'), obj_description('p'::regclass, 'pg_class')"),
        vec!["the type|NULL"]
    );

    c.batch_execute("COMMENT ON COLUMN p.x IS NULL").expect("remove");
    assert_eq!(rows(&mut c, "SELECT col_description('p'::regclass, 1)"), vec!["NULL"]);
    // Renaming keeps the comment; dropping the attribute removes it.
    c.batch_execute("ALTER TYPE p RENAME ATTRIBUTE z TO zz").expect("rename");
    assert_eq!(rows(&mut c, "SELECT col_description('p'::regclass, 2)"), vec!["the z"]);
    c.batch_execute("ALTER TYPE p DROP ATTRIBUTE zz").expect("drop");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_description WHERE description = 'the z'"), vec!["0"]);
    // Dropping the type removes the comments with it.
    c.batch_execute("COMMENT ON COLUMN p.x IS 'again'; DROP TYPE p").expect("drop type");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_description WHERE description IN ('again', 'the type')"), vec!["0"]);

    c.batch_execute("CREATE TYPE p AS (x int); CREATE TYPE e AS ENUM ('a'); CREATE DOMAIN d AS int; CREATE TABLE t (a int)")
        .expect("types");
    let e = expect_code(c.simple_query("COMMENT ON COLUMN p.nope IS 'x'"), "42703");
    assert_eq!(message(&e), "column \"nope\" of relation \"p\" does not exist");
    let e = expect_code(c.simple_query("COMMENT ON COLUMN nosuch.x IS 'x'"), "42P01");
    assert_eq!(message(&e), "relation \"nosuch\" does not exist");
    for ty in ["e", "d"] {
        let e = expect_code(c.simple_query(&format!("COMMENT ON COLUMN {ty}.x IS 'x'")), "42P01");
        assert_eq!(message(&e), format!("relation \"{ty}\" does not exist"));
    }
    // A composite type's relation is none of the relation kinds COMMENT ON names.
    for (kind, what) in [("TABLE", "a table"), ("VIEW", "a view"), ("SEQUENCE", "a sequence"), ("INDEX", "an index")] {
        let e = expect_code(c.simple_query(&format!("COMMENT ON {kind} p IS 'x'")), "42809");
        assert_eq!(message(&e), format!("\"p\" is not {what}"));
    }
}

#[test]
fn composite_type_relation_resolves_as_regclass() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE p AS (x int); CREATE SCHEMA s; CREATE TYPE s.sp AS (y int); CREATE TYPE e AS ENUM ('a')").expect("setup");
    assert_eq!(
        rows(
            &mut c,
            "SELECT 'p'::regclass::text, 'public.p'::regclass::text, to_regclass('p')::text, 's.sp'::regclass::text, to_regclass('e')::text"
        ),
        vec!["p|p|p|s.sp|NULL"]
    );
    assert_eq!(rows(&mut c, "SELECT 'p'::regclass::oid = typrelid FROM pg_type WHERE oid = 'p'::regtype"), vec!["t"]);
    assert_eq!(rows(&mut c, "SELECT relkind, pg_table_is_visible(oid) FROM pg_class WHERE oid = 'p'::regclass"), vec!["c|t"]);
    assert_eq!(rows(&mut c, "SELECT relname FROM pg_class WHERE oid = 'p'::regclass::oid::regclass::oid"), vec!["p"]);
    let e = expect_code(c.simple_query("SELECT 'e'::regclass"), "42P01");
    assert_eq!(message(&e), "relation \"e\" does not exist");
    let e = expect_code(c.simple_query("SELECT 'nosuch'::regclass"), "42P01");
    assert_eq!(message(&e), "relation \"nosuch\" does not exist");
}

#[test]
fn comments_on_domain_constraints() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE DOMAIN d AS int CONSTRAINT dc CHECK (VALUE > 0)").expect("setup");
    c.batch_execute("COMMENT ON CONSTRAINT dc ON DOMAIN d IS 'positive'").expect("comment");
    assert_eq!(
        rows(&mut c, "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint WHERE conname = 'dc' AND contypid = 'd'::regtype"),
        vec!["positive"]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT classoid::regclass, objsubid, description FROM pg_description \
             WHERE objoid = (SELECT oid FROM pg_constraint WHERE conname = 'dc')"
        ),
        vec!["pg_constraint|0|positive"]
    );
    // The comment follows a rename and goes away with the constraint, or the domain.
    c.batch_execute("ALTER DOMAIN d RENAME CONSTRAINT dc TO dc2").expect("rename");
    assert_eq!(rows(&mut c, "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint WHERE conname = 'dc2'"), vec!["positive"]);
    c.batch_execute("ALTER DOMAIN d DROP CONSTRAINT dc2").expect("drop constraint");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_description WHERE description = 'positive'"), vec!["0"]);
    c.batch_execute("ALTER DOMAIN d ADD CONSTRAINT dc3 CHECK (VALUE < 100); COMMENT ON CONSTRAINT dc3 ON DOMAIN d IS 'small'")
        .expect("again");
    c.batch_execute("COMMENT ON CONSTRAINT dc3 ON DOMAIN d IS NULL").expect("remove");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_description WHERE description = 'small'"), vec!["0"]);
    c.batch_execute("COMMENT ON CONSTRAINT dc3 ON DOMAIN d IS 'small'; DROP DOMAIN d").expect("drop domain");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_description WHERE description = 'small'"), vec!["0"]);

    c.batch_execute("CREATE DOMAIN d AS int CONSTRAINT dc CHECK (VALUE > 0); CREATE TABLE t (a int CONSTRAINT tc CHECK (a > 0)); CREATE TYPE e AS ENUM ('a')")
        .expect("setup errors");
    let e = expect_code(c.simple_query("COMMENT ON CONSTRAINT nope ON DOMAIN d IS 'x'"), "42704");
    assert_eq!(message(&e), "constraint \"nope\" for domain d does not exist");
    let e = expect_code(c.simple_query("COMMENT ON CONSTRAINT tc ON DOMAIN d IS 'x'"), "42704");
    assert_eq!(message(&e), "constraint \"tc\" for domain d does not exist");
    let e = expect_code(c.simple_query("COMMENT ON CONSTRAINT dc ON DOMAIN nosuch IS 'x'"), "42704");
    assert_eq!(message(&e), "type \"nosuch\" does not exist");
    for ty in ["t", "e"] {
        let e = expect_code(c.simple_query(&format!("COMMENT ON CONSTRAINT dc ON DOMAIN {ty} IS 'x'")), "42809");
        assert_eq!(message(&e), format!("\"{ty}\" is not a domain"));
    }
    // Without DOMAIN the constraint is a table's.
    let e = expect_code(c.simple_query("COMMENT ON CONSTRAINT dc ON d IS 'x'"), "42P01");
    assert_eq!(message(&e), "relation \"d\" does not exist");
    let e = expect_code(c.simple_query("COMMENT ON CONSTRAINT dc ON t IS 'x'"), "42704");
    assert_eq!(message(&e), "constraint \"dc\" for table \"t\" does not exist");
}
