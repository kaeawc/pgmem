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

/// `check_safe_enum_use`: a label `ALTER TYPE ... ADD VALUE` added cannot be used until the
/// transaction that added it commits (55P04), wherever text becomes the enum (`enum_in`) and
/// in the enum functions that return it.
#[test]
fn new_enum_values_are_unusable_until_commit() {
    const HINT: &str = "New enum values must be committed before they can be used.";
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE mood AS ENUM ('a', 'b'); CREATE TABLE t (m mood)").expect("setup");
    let unsafe_use = |c: &mut postgres::Client, setup: &str, sql: &str| {
        c.batch_execute(&format!("BEGIN; {setup}")).expect(setup);
        let e = expect_code(c.simple_query(sql), "55P04");
        assert_eq!(message(&e), "unsafe use of new value \"zz\" of enum type mood", "{sql}");
        assert_eq!(e.as_db_error().and_then(|d| d.hint()), Some(HINT), "{sql}");
        c.batch_execute("ROLLBACK").expect("rollback");
    };
    let add = "ALTER TYPE mood ADD VALUE 'zz'";
    for sql in [
        "INSERT INTO t VALUES ('zz')",
        "ALTER TABLE t ALTER COLUMN m SET DEFAULT 'zz'",
        "CREATE TABLE t2 (v mood DEFAULT 'zz')",
        "ALTER TABLE t ADD CONSTRAINT c CHECK (m <> 'zz')",
        "SELECT 'zz'::mood",
        "SELECT 'zz'::text::mood",
        "SELECT '{a,zz}'::mood[]",
        "SELECT m FROM t WHERE m = 'zz'",
        "SELECT enum_range(NULL::mood)",
        "SELECT enum_last(NULL::mood)",
    ] {
        unsafe_use(&mut c, add, sql);
    }
    // The enum functions check only the labels they return.
    unsafe_use(&mut c, "ALTER TYPE mood ADD VALUE 'zz' BEFORE 'a'", "SELECT enum_first(NULL::mood)");
    unsafe_use(&mut c, "ALTER TYPE mood ADD VALUE 'zz' BEFORE 'a'", "SELECT enum_range(NULL, 'a'::mood)");
    unsafe_use(&mut c, "ALTER TYPE mood ADD VALUE 'zz' AFTER 'a'", "SELECT enum_range('a'::mood, 'b'::mood)");
    c.batch_execute("BEGIN; ALTER TYPE mood ADD VALUE 'zz' BEFORE 'a'").expect("before");
    assert_eq!(one(&mut c, "SELECT enum_last(NULL::mood)"), "b");
    assert_eq!(one(&mut c, "SELECT enum_range('a'::mood, NULL)"), "{a,b}");
    assert_eq!(one(&mut c, "SELECT 'a'::mood < 'b'::mood"), "t");
    // The catalog row is there; only its use as a value is refused.
    let labels = texts(&mut c, "SELECT enumlabel FROM pg_enum WHERE enumtypid = 'mood'::regtype ORDER BY enumsortorder");
    let labels: Vec<String> = labels.into_iter().filter_map(|r| r[0].clone()).collect();
    assert_eq!(labels, ["zz", "a", "b"]);
    c.batch_execute("ROLLBACK").expect("rollback");
    // Renaming keeps the label's identity, so it stays unsafe.
    c.batch_execute("BEGIN; ALTER TYPE mood ADD VALUE 'zz'; ALTER TYPE mood RENAME VALUE 'zz' TO 'yy'").expect("rename");
    let e = expect_code(c.simple_query("SELECT 'yy'::mood"), "55P04");
    assert_eq!(message(&e), "unsafe use of new value \"yy\" of enum type mood");
    c.batch_execute("ROLLBACK").expect("rollback");
    // IF NOT EXISTS of an existing label adds nothing.
    c.batch_execute("BEGIN; ALTER TYPE mood ADD VALUE IF NOT EXISTS 'a'").expect("if not exists");
    assert_eq!(one(&mut c, "SELECT 'a'::mood"), "a");
    c.batch_execute("ROLLBACK").expect("rollback");
    // A label added in a savepoint that rolled back no longer exists.
    c.batch_execute("BEGIN; SAVEPOINT s; ALTER TYPE mood ADD VALUE 'zz'; ROLLBACK TO s").expect("rollback to");
    expect_code(c.simple_query("SELECT 'zz'::mood"), "22P02");
    c.batch_execute("ROLLBACK").expect("rollback");
    // Releasing the savepoint does not commit it.
    unsafe_use(&mut c, "SAVEPOINT s; ALTER TYPE mood ADD VALUE 'zz'; RELEASE s", "SELECT 'zz'::mood");
    // In one simple-query string the statements share an implicit transaction.
    let e = expect_code(c.batch_execute("ALTER TYPE mood ADD VALUE 'qq'; SELECT 'qq'::mood"), "55P04");
    assert_eq!(message(&e), "unsafe use of new value \"qq\" of enum type mood");
    expect_code(c.simple_query("SELECT 'qq'::mood"), "22P02");
    // After the error is rolled back to a savepoint, the label commits with the transaction.
    c.batch_execute("BEGIN; ALTER TYPE mood ADD VALUE 'zz'; SAVEPOINT s").expect("add");
    expect_code(c.simple_query("SELECT 'zz'::mood"), "55P04");
    c.batch_execute("ROLLBACK TO s; COMMIT").expect("commit");
    c.batch_execute("INSERT INTO t VALUES ('zz'); ALTER TABLE t ALTER COLUMN m SET DEFAULT 'zz'").expect("committed");
    assert_eq!(one(&mut c, "SELECT enum_range(NULL::mood)"), "{a,b,zz}");
    assert_eq!(one(&mut c, "SELECT m FROM t"), "zz");
}

/// A label added to a type the same transaction created is safe, but only when both the
/// type and the label were made outside any savepoint (`EnumValuesCreate` registers the type
/// only at top level, and `AddEnumLabel` trusts the registration only at top level).
#[test]
fn new_values_of_a_type_created_in_the_transaction() {
    let (_db, mut c) = setup();
    c.batch_execute("BEGIN; CREATE TYPE m2 AS ENUM ('x'); ALTER TYPE m2 ADD VALUE 'y'; SAVEPOINT s").expect("create");
    assert_eq!(one(&mut c, "SELECT 'y'::m2"), "y");
    assert_eq!(one(&mut c, "SELECT enum_range(NULL::m2)"), "{x,y}");
    c.batch_execute("CREATE TABLE t (v m2 DEFAULT 'y'); INSERT INTO t DEFAULT VALUES").expect("default");
    assert_eq!(one(&mut c, "SELECT v FROM t"), "y");
    c.batch_execute("ROLLBACK").expect("rollback");
    // Renaming the type keeps its identity.
    c.batch_execute("BEGIN; CREATE TYPE m2 AS ENUM ('x'); ALTER TYPE m2 RENAME TO m3; ALTER TYPE m3 ADD VALUE 'y'").expect("rename");
    assert_eq!(one(&mut c, "SELECT 'y'::m3"), "y");
    c.batch_execute("ROLLBACK").expect("rollback");
    // The exemption is per type.
    c.batch_execute("CREATE TYPE mood AS ENUM ('a')").expect("mood");
    c.batch_execute("BEGIN; ALTER TYPE mood ADD VALUE 'zz'; CREATE TYPE m2 AS ENUM ('x'); ALTER TYPE m2 ADD VALUE 'y'").expect("two types");
    assert_eq!(one(&mut c, "SELECT 'y'::m2"), "y");
    expect_code(c.simple_query("SELECT 'zz'::mood"), "55P04");
    c.batch_execute("ROLLBACK").expect("rollback");
    // An implicit transaction counts too.
    c.batch_execute("CREATE TYPE m4 AS ENUM ('x'); ALTER TYPE m4 ADD VALUE 'y'; SELECT 'y'::m4").expect("implicit");
    c.batch_execute("DROP TYPE m4").expect("drop");
    for (setup, why) in [
        ("CREATE TYPE m2 AS ENUM ('x'); SAVEPOINT s; ALTER TYPE m2 ADD VALUE 'y'", "label added in a savepoint"),
        ("CREATE TYPE m2 AS ENUM ('x'); SAVEPOINT s; ALTER TYPE m2 ADD VALUE 'y'; RELEASE s", "label added in a released savepoint"),
        ("SAVEPOINT s; CREATE TYPE m2 AS ENUM ('x'); ALTER TYPE m2 ADD VALUE 'y'", "type and label in a savepoint"),
        ("SAVEPOINT s; CREATE TYPE m2 AS ENUM ('x'); RELEASE s; ALTER TYPE m2 ADD VALUE 'y'", "type created in a savepoint"),
        // ROLLBACK TO keeps the savepoint open, so the next label is added inside it.
        (
            "CREATE TYPE m2 AS ENUM ('x'); SAVEPOINT s; ALTER TYPE m2 ADD VALUE 'z'; ROLLBACK TO s; ALTER TYPE m2 ADD VALUE 'y'",
            "label added after ROLLBACK TO",
        ),
    ] {
        c.batch_execute(&format!("BEGIN; {setup}")).expect(why);
        let e = expect_code(c.simple_query("SELECT 'y'::m2"), "55P04");
        assert_eq!(message(&e), "unsafe use of new value \"y\" of enum type m2", "{why}");
        c.batch_execute("ROLLBACK").expect("rollback");
    }
    // Labels the type was created with are always safe.
    c.batch_execute("BEGIN; SAVEPOINT s; CREATE TYPE m2 AS ENUM ('x'); RELEASE s").expect("create in savepoint");
    assert_eq!(one(&mut c, "SELECT 'x'::m2"), "x");
    c.batch_execute("ROLLBACK").expect("rollback");
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

/// Rows of `sql`, each row's columns joined with `|` (NULL as `NULL`).
fn rows(c: &mut postgres::Client, sql: &str) -> Vec<String> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect::<Vec<_>>().join("|")).collect()
}

#[test]
fn domain_default_is_followed_by_constraints() {
    // DEFAULT takes a b_expr: `DEFAULT 1 NOT NULL` is a default and a NOT NULL constraint.
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE DOMAIN d1 AS integer DEFAULT 1 NOT NULL; \
         CREATE DOMAIN d2 AS integer DEFAULT 1 NULL; \
         CREATE DOMAIN d3 AS integer DEFAULT 1 CHECK (VALUE > 0); \
         CREATE DOMAIN d4 AS text DEFAULT 'x' COLLATE \"C\"; \
         CREATE DOMAIN d5 AS integer DEFAULT 1 + 2 NOT NULL CHECK (VALUE > 0)",
    )
    .expect("create domains");
    assert_eq!(
        rows(
            &mut c,
            "SELECT typname, typnotnull::text, typdefault, pg_get_expr(typdefaultbin, 0), typcollation::int FROM pg_type \
             WHERE typname IN ('d1', 'd2', 'd3', 'd4', 'd5') ORDER BY 1"
        ),
        vec!["d1|true|1|1|0", "d2|false|1|1|0", "d3|false|1|1|0", "d4|false|'x'::text|'x'::text|950", "d5|true|(1 + 2)|(1 + 2)|0"]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE contypid IN ('d1'::regtype, 'd3'::regtype, 'd5'::regtype) ORDER BY 1"
        ),
        vec!["d1_not_null|NOT NULL", "d3_check|CHECK ((VALUE > 0))", "d5_check|CHECK ((VALUE > 0))", "d5_not_null|NOT NULL"]
    );
    let e = expect_code(c.simple_query("CREATE DOMAIN d6 AS integer DEFAULT 1 DEFAULT 2"), "42601");
    assert_eq!(message(&e), "multiple default expressions");
    let e = expect_code(c.simple_query("CREATE DOMAIN d6 AS text COLLATE \"C\" COLLATE \"POSIX\""), "42601");
    assert_eq!(message(&e), "multiple COLLATE clauses not allowed");
}

#[test]
fn domain_default_has_a_node_tree() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE DOMAIN d AS integer DEFAULT 2").expect("create");
    let q = "SELECT coalesce(typdefault, 'NULL'), coalesce(pg_get_expr(typdefaultbin, 0), 'NULL') FROM pg_type WHERE typname = 'd'";
    assert_eq!(rows(&mut c, q), vec!["2|2"]);
    c.batch_execute("ALTER DOMAIN d SET DEFAULT 3 + 4").expect("set default");
    assert_eq!(rows(&mut c, q), vec!["(3 + 4)|(3 + 4)"]);
    c.batch_execute("ALTER DOMAIN d DROP DEFAULT").expect("drop default");
    assert_eq!(rows(&mut c, q), vec!["NULL|NULL"]);
}

#[test]
fn domain_over_a_domain_copies_its_default() {
    // DefineDomain copies the base domain's default when the new domain has none of its own;
    // later changes to the base's default do not reach the copy.
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE DOMAIN a AS integer DEFAULT 1; \
         CREATE DOMAIN b AS a; \
         CREATE DOMAIN c AS a DEFAULT 5; \
         ALTER DOMAIN a SET DEFAULT 9; \
         CREATE TABLE t (id int, x b, y c); \
         INSERT INTO t (id) VALUES (1)",
    )
    .expect("setup");
    assert_eq!(
        rows(&mut c, "SELECT typname, typdefault, pg_get_expr(typdefaultbin, 0) FROM pg_type WHERE typname IN ('a', 'b', 'c') ORDER BY 1"),
        vec!["a|9|9", "b|1|1", "c|5|5"]
    );
    assert_eq!(
        rows(&mut c, "SELECT domain_name, domain_default FROM information_schema.domains WHERE domain_name IN ('a', 'b', 'c') ORDER BY 1"),
        vec!["a|9", "b|1", "c|5"]
    );
    assert_eq!(rows(&mut c, "SELECT x, y FROM t"), vec!["1|5"]);
}

#[test]
fn not_valid_domain_check_until_validated() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE DOMAIN d AS integer; ALTER DOMAIN d ADD CONSTRAINT c1 CHECK (VALUE > 0) NOT VALID").expect("setup");
    let q = "SELECT convalidated::text, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'c1'";
    assert_eq!(rows(&mut c, q), vec!["false|CHECK ((VALUE > 0)) NOT VALID"]);
    c.batch_execute("ALTER DOMAIN d VALIDATE CONSTRAINT c1").expect("validate");
    assert_eq!(rows(&mut c, q), vec!["true|CHECK ((VALUE > 0))"]);
}

#[test]
fn array_domains_keep_the_element_typmod() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE DOMAIN na AS numeric(10,2)[]; \
         CREATE DOMAIN va AS varchar(5)[] DEFAULT ARRAY['ab']; \
         CREATE DOMAIN n1 AS numeric(10,2); \
         CREATE DOMAIN i2 AS integer[][]",
    )
    .expect("setup");
    assert_eq!(
        rows(
            &mut c,
            "SELECT typname, typtypmod, format_type(typbasetype, typtypmod), typndims FROM pg_type \
             WHERE typname IN ('na', 'va', 'n1', 'i2') ORDER BY 1"
        ),
        vec!["i2|-1|integer[]|2", "n1|655366|numeric(10,2)|0", "na|655366|numeric(10,2)[]|1", "va|9|character varying(5)[]|1"]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT domain_name, data_type, coalesce(numeric_precision::text, 'NULL'), udt_name, coalesce(domain_default, 'NULL') \
             FROM information_schema.domains WHERE domain_name IN ('na', 'va', 'n1') ORDER BY 1"
        ),
        vec!["n1|numeric|10|numeric|NULL", "na|numeric[]|NULL|_numeric|NULL", "va|character varying[]|NULL|_varchar|ARRAY['ab'::text]"]
    );
}

#[test]
fn domain_and_composite_attribute_collations() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE DOMAIN tc AS text COLLATE \"C\"; \
         CREATE DOMAIN tc2 AS tc; \
         CREATE DOMAIN tc3 AS tc COLLATE \"POSIX\"; \
         CREATE DOMAIN tu AS text COLLATE ucs_basic; \
         CREATE DOMAIN td AS text; \
         CREATE DOMAIN ti AS integer; \
         CREATE TYPE p AS (y text COLLATE \"C\", z text, w int, v varchar(3) COLLATE \"POSIX\", u tc); \
         CREATE TABLE t (v tc, w text, x text COLLATE \"POSIX\")",
    )
    .expect("setup");
    assert_eq!(
        rows(&mut c, "SELECT typname, typcollation::int FROM pg_type WHERE typname IN ('tc', 'tc2', 'tc3', 'tu', 'td', 'ti') ORDER BY 1"),
        vec!["tc|950", "tc2|950", "tc3|951", "td|100", "ti|0", "tu|962"]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT attname, attcollation::int FROM pg_attribute WHERE attrelid = (SELECT typrelid FROM pg_type WHERE typname = 'p') ORDER BY attnum"
        ),
        vec!["y|950", "z|100", "w|0", "v|951", "u|950"]
    );
    assert_eq!(
        rows(&mut c, "SELECT attname, attcollation::int FROM pg_attribute WHERE attrelid = 't'::regclass AND attnum > 0 ORDER BY attnum"),
        vec!["v|950", "w|100", "x|951"]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT domain_name, coalesce(collation_name, 'NULL') FROM information_schema.domains \
             WHERE domain_name IN ('tc', 'tc3', 'td') ORDER BY 1"
        ),
        vec!["tc|C", "tc3|POSIX", "td|NULL"]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT column_name, coalesce(collation_name, 'NULL') FROM information_schema.columns WHERE table_name = 't' ORDER BY ordinal_position"
        ),
        vec!["v|C", "w|NULL", "x|POSIX"]
    );
    c.batch_execute("INSERT INTO t (v) VALUES ('b'), ('B'), ('a'), ('A')").expect("insert");
    assert_eq!(rows(&mut c, "SELECT string_agg(v, ',' ORDER BY v) FROM t"), vec!["A,B,a,b"]);
    let e = expect_code(c.simple_query("CREATE DOMAIN bad AS integer COLLATE \"C\""), "42804");
    assert_eq!(message(&e), "collations are not supported by type integer");
    let e = expect_code(c.simple_query("CREATE TYPE bad AS (i int COLLATE \"C\")"), "42804");
    assert_eq!(message(&e), "collations are not supported by type integer");
    let e = expect_code(c.simple_query("CREATE DOMAIN bad AS text COLLATE nosuch"), "42704");
    assert_eq!(message(&e), "collation \"nosuch\" for encoding \"UTF8\" does not exist");
}
