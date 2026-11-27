//! User-defined types: enums (`CREATE TYPE ... AS ENUM`, `ALTER TYPE ... ADD / RENAME VALUE`),
//! domains (constraints, defaults, `ALTER DOMAIN`), composite types (row values, field access,
//! field assignment) and the catalog rows clients read to decode them.

use crate::support::{expect_code, message, setup, text_value, texts};
use postgres::types::Kind;

fn one(c: &mut postgres::Client, sql: &str) -> String {
    text_value(c, sql).unwrap_or_else(|| "NULL".into())
}

fn detail(e: &postgres::Error) -> Option<String> {
    e.as_db_error().and_then(|d| d.detail().map(str::to_owned))
}

#[test]
fn enums_order_by_declaration_and_reject_unknown_labels() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); \
         CREATE TABLE person (name text, m mood); \
         INSERT INTO person VALUES ('a', 'happy'), ('b', 'sad'), ('c', 'ok')",
    )
    .expect("setup");
    let rows = texts(&mut c, "SELECT name FROM person WHERE m > 'sad' ORDER BY m DESC");
    assert_eq!(rows, vec![vec![Some("a".to_owned())], vec![Some("c".to_owned())]]);
    assert_eq!(one(&mut c, "SELECT enum_range(NULL::mood)"), "{sad,ok,happy}");
    assert_eq!(one(&mut c, "SELECT enum_range('ok'::mood, NULL)"), "{ok,happy}");
    assert_eq!(one(&mut c, "SELECT max(m)::text || '/' || min(m)::text FROM person"), "happy/sad");
    let e = expect_code(c.simple_query("INSERT INTO person VALUES ('d', 'angry')"), "22P02");
    assert_eq!(message(&e), "invalid input value for enum mood: \"angry\"");
    let e = expect_code(c.simple_query("SELECT m = 'happy'::text FROM person"), "42883");
    assert_eq!(message(&e), "operator does not exist: mood = text");
}

#[test]
fn alter_enum_add_and_rename_values() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); \
         CREATE TABLE person (m mood); INSERT INTO person VALUES ('ok')",
    )
    .expect("setup");
    c.batch_execute("ALTER TYPE mood ADD VALUE 'meh' BEFORE 'ok'").expect("before");
    c.batch_execute("ALTER TYPE mood ADD VALUE 'ecstatic' AFTER 'happy'").expect("after");
    c.batch_execute("ALTER TYPE mood ADD VALUE 'blue' BEFORE 'sad'").expect("first");
    c.batch_execute("ALTER TYPE mood RENAME VALUE 'ok' TO 'fine'").expect("rename");
    assert_eq!(one(&mut c, "SELECT enum_range(NULL::mood)"), "{blue,sad,meh,fine,happy,ecstatic}");
    // Stored values follow the rename.
    assert_eq!(one(&mut c, "SELECT m FROM person"), "fine");
    let labels = texts(&mut c, "SELECT enumlabel FROM pg_enum WHERE enumtypid = 'mood'::regtype ORDER BY enumsortorder");
    let labels: Vec<String> = labels.into_iter().filter_map(|r| r[0].clone()).collect();
    assert_eq!(labels, ["blue", "sad", "meh", "fine", "happy", "ecstatic"]);

    let e = expect_code(c.simple_query("ALTER TYPE mood ADD VALUE 'sad'"), "42710");
    assert_eq!(message(&e), "enum label \"sad\" already exists");
    c.batch_execute("ALTER TYPE mood ADD VALUE IF NOT EXISTS 'sad'").expect("if not exists");
    let e = expect_code(c.simple_query("ALTER TYPE mood ADD VALUE 'x' BEFORE 'nope'"), "22023");
    assert_eq!(message(&e), "\"nope\" is not an existing enum label");
    let e = expect_code(c.simple_query("CREATE TYPE mood AS ENUM ('x')"), "42710");
    assert_eq!(message(&e), "type \"mood\" already exists");
    expect_code(c.simple_query("CREATE TYPE dup AS ENUM ('a', 'a')"), "23505");
}

#[test]
fn drop_type_reports_dependents_and_cascades() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE mood AS ENUM ('sad', 'ok'); CREATE TABLE person (n text, m mood, ms mood[])").expect("setup");
    let e = expect_code(c.simple_query("DROP TYPE mood"), "2BP01");
    assert_eq!(message(&e), "cannot drop type mood because other objects depend on it");
    assert_eq!(
        detail(&e).as_deref(),
        Some("column ms of table person depends on type mood[]\ncolumn m of table person depends on type mood")
    );
    c.batch_execute("DROP TYPE mood CASCADE").expect("cascade");
    let cols = texts(&mut c, "SELECT column_name FROM information_schema.columns WHERE table_name = 'person'");
    assert_eq!(cols, vec![vec![Some("n".to_owned())]]);
    let e = expect_code(c.simple_query("DROP TYPE person"), "2BP01");
    assert_eq!(message(&e), "cannot drop type person because table person requires it");
    expect_code(c.simple_query("DROP TYPE nope"), "42704");
    c.batch_execute("DROP TYPE IF EXISTS nope").expect("if exists");
}

#[test]
fn domains_check_constraints_on_casts_and_writes() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE DOMAIN posint AS integer CHECK (VALUE > 0); \
         CREATE DOMAIN email AS text NOT NULL CHECK (VALUE ~ '@'); \
         CREATE DOMAIN short_code AS varchar(3) DEFAULT 'abc'; \
         CREATE TABLE acct (id posint, mail email, code short_code)",
    )
    .expect("setup");
    c.batch_execute("INSERT INTO acct (id, mail) VALUES (1, 'a@b')").expect("insert");
    assert_eq!(texts(&mut c, "SELECT code, pg_typeof(id)::text FROM acct"), vec![vec![Some("abc".to_owned()), Some("posint".to_owned())]]);
    let e = expect_code(c.simple_query("INSERT INTO acct (id, mail) VALUES (0, 'a@b')"), "23514");
    assert_eq!(message(&e), "value for domain posint violates check constraint \"posint_check\"");
    let e = expect_code(c.simple_query("INSERT INTO acct (id, mail) VALUES (2, NULL)"), "23502");
    assert_eq!(message(&e), "domain email does not allow null values");
    let e = expect_code(c.simple_query("INSERT INTO acct (id) VALUES (3)"), "23502");
    assert_eq!(message(&e), "domain email does not allow null values");
    let e = expect_code(c.simple_query("INSERT INTO acct (id, mail, code) VALUES (4, 'x@y', 'toolong')"), "22001");
    assert_eq!(message(&e), "value too long for type character varying(3)");
    expect_code(c.simple_query("UPDATE acct SET id = -5"), "23514");
    expect_code(c.simple_query("SELECT (-1)::posint"), "23514");
    // Operators see the base type.
    assert_eq!(
        texts(&mut c, "SELECT 5::posint + 1, pg_typeof(5::posint + 1)::text"),
        vec![vec![Some("6".to_owned()), Some("integer".to_owned())]]
    );
}

#[test]
fn domain_columns_are_described_by_their_base_type() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE DOMAIN posint AS integer CHECK (VALUE > 0); CREATE TABLE t (id posint)").expect("setup");
    let stmt = c.prepare("SELECT id FROM t").expect("prepare");
    assert_eq!(stmt.columns()[0].type_(), &postgres::types::Type::INT4);
    c.batch_execute("INSERT INTO t VALUES (7)").expect("insert");
    let id: i32 = c.query_one("SELECT id FROM t", &[]).expect("query").get(0);
    assert_eq!(id, 7);
}

#[test]
fn alter_domain_validates_stored_values() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE DOMAIN posint AS integer CHECK (VALUE > 0); CREATE TABLE acct (id posint); INSERT INTO acct VALUES (5), (NULL)",
    )
    .expect("setup");
    let e = expect_code(c.simple_query("ALTER DOMAIN posint ADD CONSTRAINT big CHECK (VALUE > 10)"), "23514");
    assert_eq!(message(&e), "column \"id\" of table \"acct\" contains values that violate the new constraint");
    c.batch_execute("ALTER DOMAIN posint ADD CHECK (VALUE > 10) NOT VALID").expect("not valid");
    let cons = texts(&mut c, "SELECT conname, convalidated::text FROM pg_constraint WHERE contypid = 'posint'::regtype ORDER BY 1");
    assert_eq!(
        cons,
        vec![
            vec![Some("posint_check".to_owned()), Some("true".to_owned())],
            vec![Some("posint_check1".to_owned()), Some("false".to_owned())]
        ]
    );
    expect_code(c.simple_query("INSERT INTO acct VALUES (6)"), "23514");
    let e = expect_code(c.simple_query("ALTER DOMAIN posint SET NOT NULL"), "23502");
    assert_eq!(message(&e), "column \"id\" of table \"acct\" contains null values");
    let e = expect_code(c.simple_query("ALTER DOMAIN posint DROP CONSTRAINT nope"), "42704");
    assert_eq!(message(&e), "constraint \"nope\" of domain \"posint\" does not exist");
    c.batch_execute("ALTER DOMAIN posint DROP CONSTRAINT posint_check1").expect("drop");
    c.batch_execute("INSERT INTO acct VALUES (6)").expect("insert after drop");
}

#[test]
fn composite_values_fields_and_assignment() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE address AS (street text, zip int); \
         CREATE TABLE customer (id int, addr address); \
         INSERT INTO customer VALUES (1, ROW('Main St', 12345)), (2, '(\"Elm, Rd\",)'), (3, NULL)",
    )
    .expect("setup");
    assert_eq!(
        texts(&mut c, "SELECT addr, (addr).street, (addr).zip FROM customer ORDER BY id"),
        vec![
            vec![Some("(\"Main St\",12345)".to_owned()), Some("Main St".to_owned()), Some("12345".to_owned())],
            vec![Some("(\"Elm, Rd\",)".to_owned()), Some("Elm, Rd".to_owned()), None],
            vec![None, None, None],
        ]
    );
    c.batch_execute("UPDATE customer SET addr.zip = 99999 WHERE id IN (1, 3)").expect("field assignment");
    assert_eq!(
        texts(&mut c, "SELECT (addr).* FROM customer WHERE id = 1"),
        vec![vec![Some("Main St".to_owned()), Some("99999".to_owned())]]
    );
    assert_eq!(one(&mut c, "SELECT addr FROM customer WHERE id = 3"), "(,99999)");
    assert_eq!(one(&mut c, "SELECT c FROM customer c WHERE id = 1"), "(1,\"(\"\"Main St\"\",99999)\")");
    assert_eq!(one(&mut c, "SELECT ROW(1, 'a', NULL)"), "(1,a,)");
    let e = expect_code(c.simple_query("SELECT ROW('a', 2, 3)::address"), "42846");
    assert_eq!(detail(&e).as_deref(), Some("Input has too many columns."));
    let e = expect_code(c.simple_query("SELECT '(a,b)'::address"), "22P02");
    assert_eq!(message(&e), "invalid input syntax for type integer: \"b\"");
    let e = expect_code(c.simple_query("SELECT (ROW('a', 1)::address).nope"), "42703");
    assert_eq!(message(&e), "column \"nope\" not found in data type address");
}

#[test]
fn clients_can_load_user_types_from_the_catalog() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TYPE mood AS ENUM ('sad', 'happy'); \
         CREATE TYPE address AS (street text, zip int); \
         CREATE DOMAIN posint AS integer CHECK (VALUE > 0)",
    )
    .expect("setup");
    // tokio-postgres resolves unknown OIDs through pg_type, pg_enum and pg_attribute.
    let stmt = c.prepare("SELECT 'sad'::mood, ARRAY['happy']::mood[], ROW('x', 1)::address").expect("prepare");
    let types: Vec<_> = stmt.columns().iter().map(|col| col.type_().clone()).collect();
    assert_eq!(types[0].name(), "mood");
    assert_eq!(types[0].kind(), &Kind::Enum(vec!["sad".to_owned(), "happy".to_owned()]));
    assert_eq!(types[1].name(), "_mood");
    assert!(matches!(types[1].kind(), Kind::Array(e) if e.name() == "mood"));
    assert_eq!(types[2].name(), "address");
    match types[2].kind() {
        Kind::Composite(fields) => {
            let names: Vec<&str> = fields.iter().map(postgres::types::Field::name).collect();
            assert_eq!(names, ["street", "zip"]);
        }
        other => panic!("expected a composite, got {other:?}"),
    }
    let rows =
        texts(&mut c, "SELECT typname, typtype::text FROM pg_type WHERE typname IN ('mood', '_mood', 'address', 'posint') ORDER BY 1");
    let rows: Vec<(String, String)> =
        rows.into_iter().map(|r| (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default())).collect();
    assert_eq!(
        rows,
        [("_mood".into(), "b".into()), ("address".into(), "c".into()), ("mood".into(), "e".into()), ("posint".into(), "d".into())]
    );
    c.batch_execute("CREATE TABLE p (m mood)").expect("table");
    assert_eq!(
        texts(&mut c, "SELECT data_type, udt_schema, udt_name FROM information_schema.columns WHERE table_name = 'p'"),
        vec![vec![Some("USER-DEFINED".to_owned()), Some("public".to_owned()), Some("mood".to_owned())]]
    );
}

/// A long domain's implicit constraint names keep their labels (`_check`, `_check1`,
/// `_not_null`), as Postgres' ChooseConstraintName does.
#[test]
fn long_domain_constraint_names() {
    let (_db, mut c) = setup();
    let d = "d".repeat(63);
    c.batch_execute(&format!("CREATE DOMAIN {d} AS int NOT NULL CHECK (VALUE > 0) CHECK (VALUE < 100)")).expect("create");
    let mut got: Vec<String> = texts(&mut c, &format!("SELECT conname FROM pg_constraint WHERE contypid = '{d}'::regtype"))
        .into_iter()
        .map(|mut r| r.remove(0).unwrap_or_default())
        .collect();
    got.sort();
    assert_eq!(got, [format!("{}_not_null", &d[..54]), format!("{}_check1", &d[..56]), format!("{}_check", &d[..57])]);
    // The NOT NULL constraint can be dropped by that name.
    c.batch_execute(&format!("ALTER DOMAIN {d} DROP CONSTRAINT {}_not_null", &d[..54])).expect("drop not null");
}
