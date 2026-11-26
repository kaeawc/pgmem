//! COPY FROM STDIN / TO STDOUT in text, CSV and binary formats, over both query protocols.

use crate::support::{RawConn, expect_code, message, setup, texts};
use std::io::{Read, Write};

fn copy_out(c: &mut postgres::Client, sql: &str) -> String {
    let mut out = String::new();
    c.copy_out(sql).expect("copy out").read_to_string(&mut out).expect("read copy data");
    out
}

fn copy_in(c: &mut postgres::Client, sql: &str, data: &str) -> Result<u64, postgres::Error> {
    let mut w = c.copy_in(sql)?;
    w.write_all(data.as_bytes()).expect("write copy data");
    w.finish()
}

#[test]
fn text_and_csv() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE items (id int PRIMARY KEY, name text, price numeric(8,2), added date, active bool DEFAULT true, note text)",
    )
    .expect("setup");
    let n = copy_in(
        &mut c,
        "COPY items (id, name, price, added, note) FROM STDIN",
        "1\tapple\t1.50\t2024-01-02\t\\N\n2\ttab\\there\t2\t2024-02-03\tline\\nbreak\n",
    )
    .expect("copy in");
    assert_eq!(n, 2);
    assert_eq!(
        copy_out(&mut c, "COPY items TO STDOUT"),
        "1\tapple\t1.50\t2024-01-02\tt\t\\N\n2\ttab\\there\t2.00\t2024-02-03\tt\tline\\nbreak\n"
    );
    let n = copy_in(
        &mut c,
        "COPY items (id, name, note) FROM STDIN WITH (FORMAT csv, HEADER true)",
        "id,name,note\n3,\"comma, \"\"quoted\"\"\",\n4,,\"\"\n",
    )
    .expect("csv in");
    assert_eq!(n, 2);
    assert_eq!(
        texts(&mut c, "SELECT name, note IS NULL, note FROM items WHERE id >= 3 ORDER BY id"),
        [[Some("comma, \"quoted\"".into()), Some("t".into()), None], [None, Some("f".into()), Some(String::new())]]
    );
    assert_eq!(
        copy_out(&mut c, "COPY items (id, name, note) TO STDOUT WITH (FORMAT csv, HEADER, FORCE_QUOTE (id))"),
        "id,name,note\n\"1\",apple,\n\"2\",tab\there,\"line\nbreak\"\n\"3\",\"comma, \"\"quoted\"\"\",\n\"4\",,\"\"\n"
    );
    assert_eq!(
        copy_out(&mut c, "COPY (SELECT id, upper(name) FROM items WHERE id < 3 ORDER BY id) TO STDOUT (DELIMITER '|', NULL 'NULL')"),
        "1|APPLE\n2|TAB\\tHERE\n"
    );
    // Legacy option syntax.
    assert_eq!(copy_out(&mut c, "COPY (SELECT 1 AS a, NULL AS b) TO STDOUT WITH CSV HEADER"), "a,b\n1,\n");
}

#[test]
fn binary_round_trip() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE src (id int8, label text, amount numeric, at timestamptz, tags text[], blob bytea);
         CREATE TABLE dst (LIKE src);
         INSERT INTO src VALUES (1, 'one', 12.5, '2024-05-06 07:08:09+00', '{a,b}', '\\x0102'), (2, NULL, -1, NULL, '{}', NULL)",
    )
    .expect("setup");
    let mut data = Vec::new();
    c.copy_out("COPY src TO STDOUT (FORMAT binary)").expect("binary out").read_to_end(&mut data).expect("read");
    assert!(data.starts_with(b"PGCOPY\n\xff\r\n\0"));
    let mut w = c.copy_in("COPY dst FROM STDIN (FORMAT binary)").expect("binary in");
    w.write_all(&data).expect("write");
    assert_eq!(w.finish().expect("finish"), 2);
    assert_eq!(texts(&mut c, "SELECT count(*) FROM (SELECT * FROM src EXCEPT SELECT * FROM dst) d"), [[Some("0".into())]]);
    let mut w = c.copy_in("COPY dst FROM STDIN (FORMAT binary)").expect("binary in");
    w.write_all(b"not binary").expect("write");
    let e = expect_code(w.finish(), "22P04");
    assert_eq!(message(&e), "COPY file signature not recognized");
}

/// PostgreSQL sends the binary file header in the first row's CopyData message (the trailer's,
/// without rows); psycopg parses the first message as header plus row.
#[test]
fn binary_copy_out_message_framing() {
    let (db, mut c) = setup();
    let mut raw = RawConn::connect(&db, &mut c);
    let header = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0";
    let r = raw.query("COPY (SELECT 1::int4 UNION ALL SELECT 2) TO STDOUT (FORMAT binary)");
    assert_eq!(r.copy_data.len(), 3);
    assert!(r.copy_data[0].starts_with(header));
    assert_eq!(r.copy_data[0].len(), header.len() + 2 + 4 + 4);
    assert_eq!(r.copy_data[2], (-1i16).to_be_bytes());
    let r = raw.query("COPY (SELECT 1 WHERE false) TO STDOUT (FORMAT binary)");
    assert_eq!(r.copy_data.len(), 1);
    assert_eq!(r.copy_data[0], [header.as_slice(), &(-1i16).to_be_bytes()].concat());
}

#[test]
fn defaults_triggers_and_where() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE log (id int GENERATED ALWAYS AS IDENTITY, msg text NOT NULL, level text DEFAULT 'info', len int GENERATED ALWAYS AS (length(msg)) STORED);
         CREATE TABLE audit (n int);
         CREATE FUNCTION count_rows() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO audit VALUES (1); RETURN NEW; END $$;
         CREATE TRIGGER log_ins AFTER INSERT ON log FOR EACH ROW EXECUTE FUNCTION count_rows()",
    )
    .expect("setup");
    assert_eq!(copy_in(&mut c, "COPY log (msg) FROM STDIN", "hello\nworld\n").expect("copy"), 2);
    // Rows the WHERE condition rejects (here also the NULL level) are not inserted.
    assert_eq!(copy_in(&mut c, "COPY log (msg, level) FROM STDIN WHERE level <> 'debug'", "a\twarn\nb\tdebug\nc\t\\N\n").expect("copy"), 1);
    assert_eq!(copy_in(&mut c, "COPY log (msg, level) FROM STDIN (FORMAT csv, DEFAULT '?')", "d,?\n").expect("copy"), 1);
    // Identity columns take the copied values (COPY does not check GENERATED ALWAYS).
    assert_eq!(copy_in(&mut c, "COPY log (id, msg) FROM STDIN", "100\texplicit\n").expect("copy"), 1);
    assert_eq!(
        texts(&mut c, "SELECT msg, level, len FROM log ORDER BY id"),
        [["hello", "info", "5"], ["world", "info", "5"], ["a", "warn", "1"], ["d", "info", "1"], ["explicit", "info", "8"]]
            .map(|r| r.map(|s| Some(s.to_owned())))
    );
    assert_eq!(texts(&mut c, "SELECT max(id) FROM log"), [[Some("100".into())]]);
    assert_eq!(texts(&mut c, "SELECT count(*) FROM audit"), [[Some("5".into())]]);
    assert_eq!(copy_out(&mut c, "COPY log TO STDOUT WITH (FORMAT csv)").lines().next(), Some("1,hello,info"));
    let e = expect_code(c.batch_execute("COPY log (len) FROM STDIN"), "42P10");
    assert_eq!(message(&e), "column \"len\" is a generated column");
    let e = expect_code(copy_in(&mut c, "COPY log (msg) FROM STDIN", "\\N\n"), "23502");
    assert!(message(&e).starts_with("null value in column \"msg\""));
    // ON_ERROR IGNORE skips rows whose values do not convert.
    assert_eq!(copy_in(&mut c, "COPY log (id, msg) FROM STDIN (ON_ERROR ignore)", "200\tok\nnope\tbad\n").expect("copy"), 1);
}

#[test]
fn errors() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int, b text); CREATE VIEW v AS SELECT * FROM t").expect("setup");
    let e = expect_code(copy_in(&mut c, "COPY t FROM STDIN", "1\n"), "22P04");
    assert_eq!(message(&e), "missing data for column \"b\"");
    let e = expect_code(copy_in(&mut c, "COPY t FROM STDIN", "1\tx\ty\n"), "22P04");
    assert_eq!(message(&e), "extra data after last expected column");
    let e = expect_code(copy_in(&mut c, "COPY t FROM STDIN", "x\ty\n"), "22P02");
    assert_eq!(message(&e), "invalid input syntax for type integer: \"x\"");
    let e = expect_code(copy_in(&mut c, "COPY t FROM STDIN (FORMAT csv)", "1,\"open\n"), "22P04");
    assert_eq!(message(&e), "unterminated CSV quoted field");
    let e = expect_code(copy_in(&mut c, "COPY t (a) FROM STDIN (FORMAT csv, HEADER match)", "b\n1\n"), "22P04");
    assert_eq!(message(&e), "column name mismatch in header line field 1: got \"b\", expected \"a\"");
    let cases = [
        ("COPY t FROM STDIN (FORMAT xml)", "22023", "COPY format \"xml\" not recognized"),
        ("COPY t FROM STDIN (bogus)", "42601", "option \"bogus\" not recognized"),
        ("COPY t FROM STDIN (FORMAT csv, FORMAT text)", "42601", "conflicting or redundant options"),
        ("COPY t FROM STDIN (DELIMITER ',,')", "0A000", "COPY delimiter must be a single one-byte character"),
        ("COPY t FROM STDIN (QUOTE '\"')", "0A000", "COPY QUOTE requires CSV mode"),
        ("COPY t FROM STDIN (FORMAT binary, NULL 'x')", "42601", "cannot specify NULL in BINARY mode"),
        ("COPY t TO STDOUT (FORMAT csv, FORCE_NOT_NULL (a))", "22023", "COPY FORCE_NOT_NULL cannot be used with COPY TO"),
        ("COPY t (a) TO STDOUT (FORMAT csv, FORCE_QUOTE (b))", "42P10", "FORCE_QUOTE column \"b\" not referenced by COPY"),
        ("COPY t TO STDOUT (FORMAT csv, FORCE_QUOTE (nope))", "42703", "column \"nope\" of relation \"t\" does not exist"),
        ("COPY t (nope) FROM STDIN", "42703", "column \"nope\" of relation \"t\" does not exist"),
        ("COPY t (a, a) FROM STDIN", "42701", "column \"a\" specified more than once"),
        ("COPY missing FROM STDIN", "42P01", "relation \"missing\" does not exist"),
        ("COPY v TO STDOUT", "42809", "cannot copy from view \"v\""),
        ("COPY t TO STDOUT WHERE a > 1", "42601", "WHERE clause not allowed with COPY TO"),
        ("COPY (INSERT INTO t VALUES (1)) TO STDOUT", "0A000", "COPY query must have a RETURNING clause"),
        ("COPY t TO 'relative.txt'", "42602", "relative path not allowed for COPY to file"),
    ];
    for (sql, code, msg) in cases {
        // Over the simple protocol: the error comes before any CopyIn / CopyOut response.
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    }
    // A failed COPY aborts the transaction block.
    c.batch_execute("BEGIN").expect("begin");
    expect_code(copy_in(&mut c, "COPY t FROM STDIN", "bad\tx\n"), "22P02");
    expect_code(c.batch_execute("SELECT 1"), "25P02");
    c.batch_execute("ROLLBACK").expect("rollback");
    assert_eq!(texts(&mut c, "SELECT count(*) FROM t"), [[Some("0".into())]]);
}

#[test]
fn simple_query_protocol() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int, b text)").expect("setup");
    let mut raw = RawConn::connect(&db, &mut c);
    // The statements after the COPY run once its data is in.
    let mut q = b"INSERT INTO t VALUES (0, 'zero'); COPY t FROM STDIN; SELECT count(*) FROM t".to_vec();
    q.push(0);
    raw.send(Some(b'Q'), &q);
    raw.send(Some(b'd'), b"1\tone\n2\t");
    raw.send(Some(b'd'), b"two\n");
    raw.send(Some(b'c'), &[]);
    let r = raw.reply();
    assert_eq!(r.tags, ["INSERT 0 1", "COPY 2", "SELECT 1"]);
    assert_eq!(r.values, [Some("3".to_owned())]);
    assert_eq!(r.status, b'I');
    // CopyFail rolls back the whole query string.
    let mut q = b"INSERT INTO t VALUES (9, 'nine'); COPY t FROM STDIN".to_vec();
    q.push(0);
    raw.send(Some(b'Q'), &q);
    raw.send(Some(b'd'), b"5\tfive\n");
    raw.send(Some(b'f'), b"client gave up\0");
    let r = raw.reply();
    assert_eq!(r.errors, ["57014"]);
    assert_eq!(r.tags, ["INSERT 0 1"]);
    assert_eq!(texts(&mut c, "SELECT count(*) FROM t"), [[Some("3".into())]]);
    // Postgres rejects a view as COPY FROM target once the copy has begun.
    c.batch_execute("CREATE VIEW v AS SELECT * FROM t").expect("view");
    let mut q = b"COPY v FROM STDIN".to_vec();
    q.push(0);
    raw.send(Some(b'Q'), &q);
    raw.send(Some(b'd'), b"1\tx\n");
    raw.send(Some(b'c'), &[]);
    assert_eq!(raw.reply().errors, ["42809"]);
    // CopyData after a COPY that failed is ignored.
    let mut q = b"COPY missing FROM STDIN".to_vec();
    q.push(0);
    raw.send(Some(b'Q'), &q);
    let r = raw.reply();
    assert_eq!(r.errors, ["42P01"]);
    raw.send(Some(b'd'), b"x\n");
    raw.send(Some(b'c'), &[]);
    assert_eq!(raw.simple_query("SELECT 1").0, ["SELECT 1"]);
    let mut q = b"COPY (SELECT 1) TO STDOUT".to_vec();
    q.push(0);
    raw.send(Some(b'Q'), &q);
    assert_eq!(raw.reply().tags, ["COPY 1"]);
}
