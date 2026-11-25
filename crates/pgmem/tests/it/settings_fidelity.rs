//! Configuration parameters: validation and normalization from Postgres' own definitions,
//! units, SHOW / current_setting formatting, set_config, transaction and savepoint undo,
//! pg_settings, and the output styles (DateStyle, IntervalStyle, extra_float_digits).

use crate::support::{expect_code, message, setup, text_value};

fn one(c: &mut postgres::Client, sql: &str) -> String {
    text_value(c, sql).unwrap_or_else(|| "NULL".into())
}

#[test]
fn set_validates_and_normalizes_values() {
    let (_db, mut c) = setup();
    c.batch_execute("SET work_mem = '2MB'").expect("set");
    assert_eq!(one(&mut c, "SHOW work_mem"), "2MB");
    assert_eq!(one(&mut c, "SELECT setting FROM pg_settings WHERE name = 'work_mem'"), "2048");
    c.batch_execute("SET statement_timeout = 1500").expect("set");
    assert_eq!(one(&mut c, "SHOW statement_timeout"), "1500ms");
    c.batch_execute("SET statement_timeout = '2min'").expect("set");
    assert_eq!(one(&mut c, "SHOW statement_timeout"), "2min");
    c.batch_execute("SET enable_seqscan = 'of'").expect("set");
    assert_eq!(one(&mut c, "SHOW enable_seqscan"), "off");
    c.batch_execute("SET client_min_messages = 'WARNING'").expect("set");
    assert_eq!(one(&mut c, "SHOW client_min_messages"), "warning");

    let e = expect_code(c.batch_execute("SET statement_timeout = '-5'"), "22023");
    assert_eq!(message(&e), "-5 ms is outside the valid range for parameter \"statement_timeout\" (0 ms .. 2147483647 ms)");
    let e = expect_code(c.batch_execute("SET work_mem = '1 parsec'"), "22023");
    assert_eq!(message(&e), "invalid value for parameter \"work_mem\": \"1 parsec\"");
    let e = expect_code(c.batch_execute("SET enable_seqscan = 'maybe'"), "22023");
    assert_eq!(message(&e), "parameter \"enable_seqscan\" requires a Boolean value");
    let e = expect_code(c.batch_execute("SET client_min_messages = 'loud'"), "22023");
    assert_eq!(message(&e), "invalid value for parameter \"client_min_messages\": \"loud\"");
    let e = expect_code(c.batch_execute("SET server_version = '1'"), "55P02");
    assert_eq!(message(&e), "parameter \"server_version\" cannot be changed");
    let e = expect_code(c.batch_execute("SET no_such_setting = 1"), "42704");
    assert_eq!(message(&e), "unrecognized configuration parameter \"no_such_setting\"");
    assert!(one(&mut c, "SELECT count(*) > 300 FROM pg_settings") == "t");
}

#[test]
fn set_config_and_custom_placeholders() {
    let (_db, mut c) = setup();
    assert_eq!(one(&mut c, "SELECT set_config('myapp.tenant', 'acme', false)"), "acme");
    assert_eq!(one(&mut c, "SELECT current_setting('myapp.tenant')"), "acme");
    assert_eq!(one(&mut c, "SHOW myapp.tenant"), "acme");
    expect_code(c.simple_query("SELECT current_setting('myapp.missing')"), "42704");
    assert_eq!(one(&mut c, "SELECT current_setting('myapp.missing', true)"), "NULL");
    // A placeholder created inside a rolled-back transaction stays defined, as empty.
    c.batch_execute("BEGIN; SET myapp.user_id = '42'; ROLLBACK").expect("txn");
    assert_eq!(one(&mut c, "SELECT current_setting('myapp.user_id')"), "");
    // set_config(..., true) is SET LOCAL.
    c.batch_execute("BEGIN").expect("begin");
    assert_eq!(one(&mut c, "SELECT set_config('work_mem', '8MB', true)"), "8MB");
    assert_eq!(one(&mut c, "SHOW work_mem"), "8MB");
    c.batch_execute("COMMIT").expect("commit");
    assert_eq!(one(&mut c, "SHOW work_mem"), "4MB");
}

#[test]
fn rollback_and_savepoints_undo_settings() {
    let (_db, mut c) = setup();
    c.batch_execute("BEGIN; SET work_mem = '16MB'; ROLLBACK").expect("txn");
    assert_eq!(one(&mut c, "SHOW work_mem"), "4MB");
    c.batch_execute("BEGIN; SET work_mem = '16MB'; SAVEPOINT s; SET work_mem = '32MB'; ROLLBACK TO SAVEPOINT s").expect("txn");
    assert_eq!(one(&mut c, "SHOW work_mem"), "16MB");
    c.batch_execute("COMMIT").expect("commit");
    assert_eq!(one(&mut c, "SHOW work_mem"), "16MB");
    // SET LOCAL lasts until the end of the transaction, even a committed one.
    c.batch_execute("BEGIN; SET LOCAL work_mem = '64MB'; COMMIT").expect("txn");
    assert_eq!(one(&mut c, "SHOW work_mem"), "16MB");
    c.batch_execute("RESET work_mem").expect("reset");
    assert_eq!(one(&mut c, "SHOW work_mem"), "4MB");
    c.batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ").expect("set");
    assert_eq!(one(&mut c, "SHOW default_transaction_isolation"), "repeatable read");
}

#[test]
fn date_and_interval_styles() {
    let (_db, mut c) = setup();
    c.batch_execute("SET TimeZone = 'America/New_York'; SET DateStyle = 'German'").expect("set");
    assert_eq!(one(&mut c, "SHOW DateStyle"), "German, DMY");
    assert_eq!(one(&mut c, "SELECT '2024-03-04'::date::text"), "04.03.2024");
    c.batch_execute("SET DateStyle = 'SQL, DMY'").expect("set");
    assert_eq!(one(&mut c, "SELECT '05/06/2024'::date::text"), "05/06/2024");
    assert_eq!(one(&mut c, "SELECT '2024-03-04 05:06:07+00'::timestamptz::text"), "04/03/2024 00:06:07 EST");
    c.batch_execute("SET DateStyle = 'Postgres, MDY'").expect("set");
    assert_eq!(one(&mut c, "SELECT '2024-03-04 05:06:07.5'::timestamp::text"), "Mon Mar 04 05:06:07.5 2024");
    let e = expect_code(c.batch_execute("SET DateStyle = 'Klingon'"), "22023");
    assert_eq!(message(&e), "invalid value for parameter \"DateStyle\": \"Klingon\"");
    c.batch_execute("SET DateStyle = 'ISO'").expect("set");

    let iv = "SELECT interval '-1 year -2 mons +3 days -04:05:06'::text";
    c.batch_execute("SET IntervalStyle = 'sql_standard'").expect("set");
    assert_eq!(one(&mut c, iv), "-1-2 +3 -4:05:06");
    c.batch_execute("SET IntervalStyle = 'iso_8601'").expect("set");
    assert_eq!(one(&mut c, iv), "P-1Y-2M3DT-4H-5M-6S");
    c.batch_execute("SET IntervalStyle = 'postgres_verbose'").expect("set");
    assert_eq!(one(&mut c, iv), "@ 1 year 2 mons -3 days 4 hours 5 mins 6 secs ago");
    c.batch_execute("RESET IntervalStyle").expect("reset");

    c.batch_execute("SET extra_float_digits = 0").expect("set");
    assert_eq!(one(&mut c, "SELECT (0.1::float8 + 0.2)::text"), "0.3");
    c.batch_execute("SET extra_float_digits = -3").expect("set");
    assert_eq!(one(&mut c, "SELECT pi()::text"), "3.14159265359");
}
