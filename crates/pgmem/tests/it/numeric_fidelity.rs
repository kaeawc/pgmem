//! numeric infinities and negative scales, float input range, and other value-level details.

use crate::support::{expect_code, message, setup, text_value, texts};

#[test]
fn numeric_infinity() {
    let (_db, mut c) = setup();
    assert_eq!(
        texts(&mut c, "SELECT 'Infinity'::numeric, '-inf'::numeric, 'inf'::numeric + 1, 'inf'::numeric * 0, 1 / 'inf'::numeric"),
        vec![vec![Some("Infinity".into()), Some("-Infinity".into()), Some("Infinity".into()), Some("NaN".into()), Some("0".into())]]
    );
    assert_eq!(text_value(&mut c, "SELECT 'NaN'::numeric > 'Infinity'::numeric AND 'Infinity'::numeric > 1e100"), Some("t".into()));
    let e = expect_code(c.simple_query("SELECT 'Infinity'::numeric(5,2)"), "22003");
    assert_eq!(message(&e), "numeric field overflow");
    let e = expect_code(c.simple_query("SELECT 'Infinity'::numeric::int"), "0A000");
    assert_eq!(message(&e), "cannot convert infinity to integer");
    // Binary round trip through the extended protocol.
    let row = c.query_one("SELECT 'Infinity'::numeric::text, '-Infinity'::numeric::float8", &[]).expect("query");
    assert_eq!(row.get::<_, String>(0), "Infinity");
    assert_eq!(row.get::<_, f64>(1), f64::NEG_INFINITY);
}

#[test]
fn numeric_negative_scale_and_scale_functions() {
    let (_db, mut c) = setup();
    assert_eq!(
        texts(&mut c, "SELECT 12345::numeric(5,-2), 0.0123::numeric(3,4), scale(8.4100), min_scale(8.4100), trim_scale(8.4100)"),
        vec![vec![Some("12300".into()), Some("0.0123".into()), Some("4".into()), Some("2".into()), Some("8.41".into())]]
    );
    c.batch_execute("CREATE TABLE m (x numeric(7,-3))").expect("create");
    assert_eq!(
        text_value(&mut c, "SELECT format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'm'::regclass AND attname = 'x'"),
        Some("numeric(7,-3)".into())
    );
    let e = expect_code(c.simple_query("SELECT 1::numeric(3,1001)"), "22023");
    assert_eq!(message(&e), "NUMERIC scale 1001 must be between -1000 and 1000");
    assert_eq!(
        texts(&mut c, "SELECT factorial(20), gcd(12.5, 7.5), lcm(1.5, 2.25), cbrt(27::float8)"),
        vec![vec![Some("2432902008176640000".into()), Some("2.5".into()), Some("4.50".into()), Some("3.0000000000000004".into())]]
    );
}

#[test]
fn float_input_range() {
    let (_db, mut c) = setup();
    let e = expect_code(c.simple_query("SELECT '1e40'::float4"), "22003");
    assert_eq!(message(&e), "\"1e40\" is out of range for type real");
    let e = expect_code(c.simple_query("SELECT '1e-400'::float8"), "22003");
    assert_eq!(message(&e), "\"1e-400\" is out of range for type double precision");
    assert_eq!(text_value(&mut c, "SELECT '1e-320'::float8"), Some("1e-320".into()));
}

#[test]
fn bytea_escape_output_and_byte_functions() {
    let (_db, mut c) = setup();
    c.batch_execute("SET bytea_output = 'escape'").expect("set");
    assert_eq!(text_value(&mut c, "SELECT '\\x00ff0a615c'::bytea"), Some("\\000\\377\\012a\\\\".into()));
    c.batch_execute("RESET bytea_output").expect("reset");
    assert_eq!(
        texts(
            &mut c,
            "SELECT get_byte('\\x0102'::bytea, 1), set_byte('\\x0102'::bytea, 0, 255), substring('\\x01020304'::bytea from 2 for 2), convert_from(convert_to('héllo', 'UTF8'), 'utf8')"
        ),
        vec![vec![Some("2".into()), Some("\\xff02".into()), Some("\\x0203".into()), Some("héllo".into())]]
    );
    let e = expect_code(c.simple_query("SELECT get_byte('\\x01'::bytea, 1)"), "2202E");
    assert_eq!(message(&e), "index 1 out of valid range, 0..0");
}

#[test]
fn uuid_versions_and_timestamps() {
    let (_db, mut c) = setup();
    assert_eq!(
        texts(
            &mut c,
            "SELECT uuid_extract_version(gen_random_uuid()), uuid_extract_version(uuidv7()), uuid_extract_timestamp('019366f7-2a0e-7c4a-9c6a-3a5f1d2b4c6e'::uuid) AT TIME ZONE 'UTC'"
        ),
        vec![vec![Some("4".into()), Some("7".into()), Some("2024-11-26 05:34:54.478".into())]]
    );
}

#[test]
fn truncate_restart_identity_rolls_back() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE solo (id serial PRIMARY KEY, v text); INSERT INTO solo (v) VALUES ('x'), ('y')").expect("setup");
    c.batch_execute("BEGIN; TRUNCATE solo RESTART IDENTITY; INSERT INTO solo (v) VALUES ('z'); ROLLBACK").expect("rolled back");
    assert_eq!(text_value(&mut c, "INSERT INTO solo (v) VALUES ('w') RETURNING id"), Some("3".into()));
    c.batch_execute("BEGIN; SAVEPOINT s; TRUNCATE solo RESTART IDENTITY; ROLLBACK TO s; COMMIT").expect("savepoint");
    assert_eq!(text_value(&mut c, "INSERT INTO solo (v) VALUES ('u') RETURNING id"), Some("4".into()));
    c.batch_execute("TRUNCATE solo RESTART IDENTITY").expect("truncate");
    assert_eq!(text_value(&mut c, "INSERT INTO solo (v) VALUES ('t') RETURNING id"), Some("1".into()));
}
