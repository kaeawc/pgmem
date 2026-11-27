//! Every table (and view, materialized view and composite type) has a row type with an array
//! type, named by `makeArrayTypeName` (#172); other tables' columns, domains and composite
//! types using a table's row type block DROP TABLE and some ALTER TABLE actions (#173).

use crate::support::{RawConn, TestDb, expect_code, message, setup, text_value, texts};
use postgres::Client;
use postgres::types::{FromSql, IsNull, ToSql, Type};

const HINT: &str = "Use DROP ... CASCADE to drop the dependent objects too.";

/// Checks a 2BP01 dependency error: message, DETAIL and HINT.
fn dependents<T: std::fmt::Debug>(r: Result<T, postgres::Error>, msg: &str, detail: &str) {
    let e = expect_code(r, "2BP01");
    assert_eq!(message(&e), msg);
    let db = e.as_db_error().expect("db error");
    assert_eq!(db.detail(), Some(detail));
    assert_eq!(db.hint(), Some(HINT));
}

/// Runs `sql` on a raw connection; returns its notices (asserting it succeeded).
fn notices(db: &TestDb, c: &mut Client, sql: &str) -> Vec<String> {
    let mut raw = RawConn::connect(db, c);
    let r = raw.query(sql);
    assert!(r.errors.is_empty(), "{sql}: {:?}", r.errors);
    r.notices
}

fn column(rows: Vec<Vec<Option<String>>>) -> Vec<String> {
    rows.into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect::<Vec<_>>().join("|")).collect()
}

/// `typname -> array typname` of the types whose name contains `pattern`.
fn array_names(c: &mut Client, pattern: &str) -> Vec<String> {
    column(texts(
        c,
        &format!(
            "SELECT t.typname::text, a.typname::text FROM pg_type t JOIN pg_type a ON a.oid = t.typarray \
             WHERE t.typname LIKE '%{pattern}%' AND t.typnamespace = 'public'::regnamespace ORDER BY 1"
        ),
    ))
}

#[test]
fn row_type_has_an_array_type() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE zpoint (x int, y int);
         CREATE VIEW zview AS SELECT 1 AS a;
         CREATE MATERIALIZED VIEW zmat AS SELECT 1 AS a;
         CREATE TYPE zcomp AS (a int);
         CREATE SEQUENCE zseq;",
    )
    .expect("setup");
    let rows = column(texts(
        &mut c,
        "SELECT t.typname::text, a.typname::text, a.typcategory::text, (a.typelem = t.oid)::text, a.typtype::text, \
                a.typinput::text, a.typsubscript::text, a.typalign::text \
         FROM pg_type t JOIN pg_type a ON a.oid = t.typarray WHERE t.typname LIKE 'z%' ORDER BY 1",
    ));
    assert_eq!(
        rows,
        [
            "zcomp|_zcomp|A|true|b|array_in|array_subscript_handler|d",
            "zmat|_zmat|A|true|b|array_in|array_subscript_handler|d",
            "zpoint|_zpoint|A|true|b|array_in|array_subscript_handler|d",
            "zview|_zview|A|true|b|array_in|array_subscript_handler|d",
        ]
    );
    // Sequences have no row type.
    assert_eq!(text_value(&mut c, "SELECT reltype::text FROM pg_class WHERE relname = 'zseq'").as_deref(), Some("0"));
    assert_eq!(
        column(texts(
            &mut c,
            "SELECT '_zpoint'::regtype::text, 'zpoint[]'::regtype::text, \
                    ('zpoint[]'::regtype::oid = (SELECT typarray FROM pg_type WHERE typname = 'zpoint'))::text"
        )),
        ["zpoint[]|zpoint[]|true"]
    );
}

#[test]
fn row_type_array_columns() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE zpoint (x int, y int); CREATE TABLE path (id int, pts zpoint[], alt _zpoint)").expect("setup");
    assert_eq!(
        column(texts(
            &mut c,
            "SELECT attname::text, format_type(atttypid, atttypmod), atttypid::regtype::text FROM pg_attribute \
             WHERE attrelid = 'path'::regclass AND attnum > 0 ORDER BY attnum"
        )),
        ["id|integer|integer", "pts|zpoint[]|zpoint[]", "alt|zpoint[]|zpoint[]"]
    );
    c.batch_execute(
        "INSERT INTO path VALUES (1, ARRAY[ROW(1,2)::zpoint, ROW(3,4)::zpoint], '{}');
         INSERT INTO path VALUES (2, '{\"(5,6)\",NULL,\"(7,)\"}', NULL);",
    )
    .expect("insert");
    assert_eq!(
        column(texts(
            &mut c,
            "SELECT pts::text, pts[1].x::text, (pts[2]).y::text, pg_typeof(pts)::text, pg_typeof(pts[1])::text FROM path ORDER BY id"
        )),
        ["{\"(1,2)\",\"(3,4)\"}|1|4|zpoint[]|zpoint", "{\"(5,6)\",NULL,\"(7,)\"}|5|NULL|zpoint[]|zpoint"]
    );
    // unnest yields the row type's columns.
    assert_eq!(
        column(texts(
            &mut c,
            "SELECT p.id::text, u.x::text, u.y::text, u.n::text FROM path p, unnest(p.pts) WITH ORDINALITY u(x, y, n) ORDER BY 1, 4"
        )),
        ["1|1|2|1", "1|3|4|2", "2|5|6|1", "2|NULL|NULL|2", "2|7|NULL|3"]
    );
    assert_eq!(column(texts(&mut c, "SELECT * FROM unnest((SELECT pts FROM path WHERE id = 1))")), ["1|2", "3|4"]);
    let e = expect_code(c.batch_execute("SELECT '{\"(1,x)\"}'::zpoint[]"), "22P02");
    assert_eq!(message(&e), "invalid input syntax for type integer: \"x\"");
    assert_eq!(
        text_value(&mut c, "SELECT array_agg(ROW(x, y)::zpoint ORDER BY x)::text FROM (VALUES (2, 1), (1, 2)) v(x, y)").as_deref(),
        Some("{\"(1,2)\",\"(2,1)\"}")
    );
}

/// A value's binary wire form, as received.
#[derive(Debug)]
struct Raw(Vec<u8>);

impl<'a> FromSql<'a> for Raw {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(Raw(raw.to_vec()))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

impl ToSql for Raw {
    fn to_sql(&self, _: &Type, out: &mut postgres::types::private::BytesMut) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(&self.0);
        Ok(IsNull::No)
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    postgres::types::to_sql_checked!();
}

fn be32(b: &[u8], at: usize) -> i32 {
    i32::from_be_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

#[test]
fn row_type_array_binary_io() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE zpoint (x int, y int); CREATE TABLE path (pts zpoint[]); INSERT INTO path VALUES ('{\"(1,2)\"}')")
        .expect("setup");
    let row = c.query_one("SELECT pts, 'zpoint'::regtype::oid FROM path", &[]).expect("select");
    let Raw(b) = row.get(0);
    let elem: u32 = row.get(1);
    // ndim 1, no nulls, element type, then one dimension of length 1 from 1.
    assert_eq!((be32(&b, 0), be32(&b, 4), be32(&b, 8) as u32, be32(&b, 12), be32(&b, 16)), (1, 0, elem, 1, 1));
    // The element: its length, then record_send's 2 columns of int4 (OID 23).
    let rec = &b[24..];
    assert_eq!(be32(&b, 20) as usize, rec.len());
    assert_eq!((be32(rec, 0), be32(rec, 4), be32(rec, 8), be32(rec, 12)), (2, 23, 4, 1));
    assert_eq!((be32(rec, 16), be32(rec, 20), be32(rec, 24)), (23, 4, 2));
    // Binary input of the same bytes.
    c.execute("INSERT INTO path VALUES ($1)", &[&Raw(b.clone())]).expect("binary insert");
    assert_eq!(column(texts(&mut c, "SELECT pts::text FROM path")), ["{\"(1,2)\"}", "{\"(1,2)\"}"]);
}

#[test]
fn array_type_names_avoid_collisions() {
    let (_db, mut c) = setup();
    // `_zz` is taken by a table's row type, so zz's array type is `_zz_1`; a type named like
    // an existing array type pushes the array type out of the way.
    c.batch_execute(
        "CREATE TABLE _zz (a int);
         CREATE TABLE zz (x int);
         CREATE TABLE __zz (b int);
         CREATE TYPE ___zz AS (c int);",
    )
    .expect("setup");
    assert_eq!(array_names(&mut c, "zz"), ["___zz|____zz_1", "__zz|___zz_1", "_zz|____zz", "zz|_zz_1"]);
    c.batch_execute("CREATE TABLE zy (x int); CREATE TABLE _zy (a int); CREATE TABLE zq (v zy[], w _zy, u __zy)").expect("zy");
    assert_eq!(array_names(&mut c, "zy"), ["_zy|__zy_1", "zy|__zy"]);
    assert_eq!(
        column(texts(&mut c, "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'zq'::regclass AND attnum > 0")),
        ["zy[]", "_zy", "zy[]"]
    );
    // Renaming renames the array type too.
    c.batch_execute("ALTER TABLE zy RENAME TO zy2").expect("rename");
    assert_eq!(array_names(&mut c, "zy"), ["_zy|__zy_1", "zy2|_zy2"]);
    assert_eq!(
        column(texts(&mut c, "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'zq'::regclass AND attnum = 1")),
        ["zy2[]"]
    );
    // The old name still counts while the array type is renamed; the type's own array does not.
    c.batch_execute(
        "CREATE TABLE _zx (a int); ALTER TABLE _zx RENAME TO zx;
         CREATE TABLE zw (a int); ALTER TABLE zw RENAME TO _zw;
         CREATE VIEW zv AS SELECT 1 AS a; ALTER VIEW zv RENAME TO _zv;
         CREATE TYPE zt AS (a int); ALTER TYPE zt RENAME TO _zt;",
    )
    .expect("renames");
    assert_eq!(array_names(&mut c, "zx"), ["zx|_zx_1"]);
    assert_eq!(array_names(&mut c, "zw"), ["_zw|__zw"]);
    assert_eq!(array_names(&mut c, "zv"), ["_zv|__zv"]);
    assert_eq!(array_names(&mut c, "zt"), ["_zt|__zt"]);
    // Dropping the table drops its array type.
    c.batch_execute("DROP TABLE zq; DROP TABLE zy2").expect("drop");
    assert_eq!(array_names(&mut c, "zy"), ["_zy|__zy_1"]);
    assert_eq!(text_value(&mut c, "SELECT count(*)::text FROM pg_type WHERE typname = '_zy2'").as_deref(), Some("0"));
    c.batch_execute("CREATE TABLE zy2 (z int)").expect("recreate");
    assert_eq!(array_names(&mut c, "zy2"), ["zy2|_zy2"]);
}

#[test]
fn drop_table_used_as_column_type() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE a (x int); CREATE TABLE b (v a, k int); INSERT INTO b VALUES (ROW(1), 2)").expect("setup");
    dependents(
        c.batch_execute("DROP TABLE a"),
        "cannot drop table a because other objects depend on it",
        "column v of table b depends on type a",
    );
    let e = expect_code(c.batch_execute("DROP TYPE a"), "2BP01");
    assert_eq!(message(&e), "cannot drop type a because table a requires it");
    assert_eq!(e.as_db_error().and_then(|d| d.hint()), Some("You can drop table a instead."));
    let e = expect_code(c.batch_execute("DROP TYPE _a"), "2BP01");
    assert_eq!(message(&e), "cannot drop type a[] because type a requires it");
    assert_eq!(e.as_db_error().and_then(|d| d.hint()), Some("You can drop type a instead."));
    // Dropped together, the column is no dependent.
    c.batch_execute("BEGIN; DROP TABLE b, a; ROLLBACK").expect("drop both");
    assert_eq!(notices(&db, &mut c, "DROP TABLE a CASCADE"), ["drop cascades to column v of table b"]);
    assert_eq!(column(texts(&mut c, "SELECT * FROM b")), ["2"]);
}

/// a's row type in another table directly and as an array, in a domain (used by a table) and
/// in a composite type: depth first, the array type's users first, in creation order.
fn users_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE a (x int, y text);
         CREATE TABLE b (v a, w a[], k int);
         CREATE DOMAIN da AS a;
         CREATE TYPE ca AS (f a, g a[]);
         CREATE TABLE c (z da, q ca);
         INSERT INTO b VALUES (ROW(1, 'p'), ARRAY[ROW(2, 'q')::a, NULL], 1);
         INSERT INTO c VALUES (ROW(3, 'r'), ROW(ROW(4, 's'), ARRAY[ROW(5, 't')::a]));",
    )
    .expect("setup");
    (db, c)
}

const USERS: [&str; 6] = [
    "column w of table b",
    "column g of composite type ca",
    "column v of table b",
    "type da",
    "column z of table c",
    "column f of composite type ca",
];

#[test]
fn drop_table_used_through_arrays_domains_and_composites() {
    let (db, mut c) = users_setup();
    dependents(
        c.batch_execute("DROP TABLE a"),
        "cannot drop table a because other objects depend on it",
        "column w of table b depends on type a[]\ncolumn g of composite type ca depends on type a[]\n\
         column v of table b depends on type a\ntype da depends on type a\ncolumn z of table c depends on type da\n\
         column f of composite type ca depends on type a",
    );
    // Several tables named: the other message.
    dependents(
        c.batch_execute("DROP TABLE b, a"),
        "cannot drop desired object(s) because other objects depend on them",
        "column g of composite type ca depends on type a[]\ntype da depends on type a\ncolumn z of table c depends on type da\n\
         column f of composite type ca depends on type a",
    );
    let cascaded: Vec<String> = USERS.iter().map(|u| format!("drop cascades to {u}")).collect();
    assert_eq!(notices(&db, &mut c, "DROP TABLE a CASCADE"), [format!("drop cascades to 6 other objects\n{}", cascaded.join("\n"))]);
    assert_eq!(column(texts(&mut c, "SELECT * FROM b")), ["1"]);
    // The composite type lost both attributes, and its stored values with them.
    assert_eq!(column(texts(&mut c, "SELECT q::text FROM c")), ["()"]);
    assert_eq!(text_value(&mut c, "SELECT count(*)::text FROM pg_type WHERE typname IN ('a', '_a', 'da')").as_deref(), Some("0"));
}

#[test]
fn drop_type_lists_users_of_domains_depth_first() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE x AS (a int); CREATE DOMAIN dx AS x; CREATE TABLE tx (c dx, e x); CREATE DOMAIN dx2 AS dx")
        .expect("setup");
    dependents(
        c.batch_execute("DROP TYPE x"),
        "cannot drop type x because other objects depend on it",
        "type dx depends on type x\ncolumn c of table tx depends on type dx\ntype dx2 depends on type dx\ncolumn e of table tx depends on type x",
    );
    let e = expect_code(c.batch_execute("DROP TYPE _x"), "2BP01");
    assert_eq!(message(&e), "cannot drop type x[] because type x requires it");
}

#[test]
fn drop_materialized_view_used_as_column_type() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE MATERIALIZED VIEW mv AS SELECT 1 AS m; CREATE TABLE g (s mv, t int)").expect("setup");
    dependents(
        c.batch_execute("DROP MATERIALIZED VIEW mv"),
        "cannot drop materialized view mv because other objects depend on it",
        "column s of table g depends on type mv",
    );
    assert_eq!(notices(&db, &mut c, "DROP MATERIALIZED VIEW mv CASCADE"), ["drop cascades to column s of table g"]);
}

#[test]
fn alter_table_refused_while_row_type_is_stored() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE a (x int, s varchar(5)); CREATE TABLE b (k int, v a); CREATE DOMAIN pos AS int CHECK (VALUE > 0)")
        .expect("setup");
    let refused = [
        "ALTER TABLE a ALTER COLUMN x TYPE text",
        "ALTER TABLE a ALTER COLUMN s TYPE varchar(10)",
        "ALTER TABLE a ALTER COLUMN x TYPE int",
        "ALTER TABLE a ADD COLUMN y int DEFAULT 5",
        "ALTER TABLE a ADD COLUMN y serial",
        "ALTER TABLE a ADD COLUMN y int GENERATED ALWAYS AS IDENTITY",
        "ALTER TABLE a ADD COLUMN y int GENERATED ALWAYS AS (x + 1) STORED",
        "ALTER TABLE a ADD COLUMN y pos",
        "ALTER TABLE a ADD COLUMN y int, ADD COLUMN z int DEFAULT 1",
    ];
    for sql in refused {
        let e = expect_code(c.batch_execute(sql), "0A000");
        assert_eq!(message(&e), "cannot alter table \"a\" because column \"b.v\" uses its row type", "{sql}");
    }
    for sql in [
        "ALTER TABLE a ADD COLUMN y int DEFAULT NULL",
        "ALTER TABLE a ADD COLUMN z int NOT NULL",
        "ALTER TABLE a ADD COLUMN w int CHECK (w > 0)",
        "ALTER TABLE a ALTER COLUMN x SET DEFAULT 3",
        "ALTER TABLE a RENAME COLUMN x TO xx",
    ] {
        c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // An array of the row type, or a domain over it, counts too: the array type's users first.
    c.batch_execute("CREATE TABLE a2 (q int); CREATE TABLE b0 (k int); CREATE DOMAIN da AS a2; CREATE TABLE b1 (z da); ALTER TABLE b0 ADD COLUMN w a2[]")
        .expect("users");
    let e = expect_code(c.batch_execute("ALTER TABLE a2 ALTER COLUMN q TYPE text"), "0A000");
    assert_eq!(message(&e), "cannot alter table \"a2\" because column \"b0.w\" uses its row type");
    c.batch_execute("ALTER TABLE b0 DROP COLUMN w").expect("drop w");
    let e = expect_code(c.batch_execute("ALTER TABLE a2 ALTER COLUMN q TYPE text"), "0A000");
    assert_eq!(message(&e), "cannot alter table \"a2\" because column \"b1.z\" uses its row type");
    // A composite type nobody stores does not.
    c.batch_execute("CREATE TABLE a3 (q int); CREATE TYPE ca AS (f a3); ALTER TABLE a3 ALTER COLUMN q TYPE text").expect("composite only");
}

#[test]
fn add_and_drop_column_change_stored_row_values() {
    let (_db, mut c) = users_setup();
    c.batch_execute("ALTER TABLE a ADD COLUMN z int").expect("add");
    assert_eq!(column(texts(&mut c, "SELECT v::text, w::text FROM b")), ["(1,p,)|{\"(2,q,)\",NULL}"]);
    assert_eq!(column(texts(&mut c, "SELECT z::text, q::text FROM c")), ["(3,r,)|(\"(4,s,)\",\"{\"\"(5,t,)\"\"}\")"]);
    c.batch_execute("ALTER TABLE a DROP COLUMN x").expect("drop x");
    assert_eq!(column(texts(&mut c, "SELECT v::text, w::text, (v).y, (v).z::text, w[1].y FROM b")), ["(p,)|{\"(q,)\",NULL}|p|NULL|q"]);
    assert_eq!(column(texts(&mut c, "SELECT z::text, q::text FROM c")), ["(r,)|(\"(s,)\",\"{\"\"(t,)\"\"}\")"]);
    c.batch_execute("INSERT INTO b (v, k) VALUES (ROW('n', 7), 2)").expect("insert");
    assert_eq!(column(texts(&mut c, "SELECT v::text FROM b ORDER BY k")), ["(p,)", "(n,7)"]);
    c.batch_execute("ALTER TABLE a DROP COLUMN y; ALTER TABLE a DROP COLUMN z").expect("drop all");
    assert_eq!(column(texts(&mut c, "SELECT v::text, w::text FROM b ORDER BY k")), ["()|{(),NULL}", "()|NULL"]);
}

#[test]
fn row_type_array_parameters_are_described() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TYPE zc AS (x int); CREATE TYPE zmood AS ENUM ('ok'); CREATE TABLE zp (x int); CREATE TABLE t (a zp[])")
        .expect("setup");
    for (sql, name) in
        [("INSERT INTO t VALUES ($1)", "_zp"), ("SELECT $1::zp[]", "_zp"), ("SELECT $1::zc[]", "_zc"), ("SELECT $1::zmood[]", "_zmood")]
    {
        let st = c.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(st.params()[0].name(), name, "{sql}");
    }
}
