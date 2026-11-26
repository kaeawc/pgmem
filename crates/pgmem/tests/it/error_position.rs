//! The ErrorResponse `position` field (`P`): the 1-based character offset into the query string
//! as sent that Postgres reports for syntax errors and parse-analysis errors (#245).

use crate::support::{expect_code, setup};
use postgres::Client;
use postgres::error::ErrorPosition;

/// The position a statement sent as a simple Query fails at (`None`: no `P` field).
#[track_caller]
fn simple(c: &mut Client, sql: &str, code: &str) -> Option<u32> {
    original(&expect_code(c.batch_execute(sql), code))
}

/// The position a statement sent as an extended-protocol Parse fails at.
#[track_caller]
fn extended(c: &mut Client, sql: &str, code: &str) -> Option<u32> {
    original(&expect_code(c.prepare(sql), code))
}

fn original(e: &postgres::Error) -> Option<u32> {
    match e.as_db_error()?.position()? {
        ErrorPosition::Original(p) => Some(*p),
        ErrorPosition::Internal { .. } => None,
    }
}

#[test]
fn syntax_errors_point_at_the_offending_token() {
    let (_db, mut c) = setup();
    assert_eq!(simple(&mut c, "SELEC 1", "42601"), Some(1));
    assert_eq!(simple(&mut c, "SELECT 1 FROM;", "42601"), Some(14));
    assert_eq!(simple(&mut c, "SELECT 1 ORDER 2", "42601"), Some(16));
    assert_eq!(simple(&mut c, "SELECT 1 +", "42601"), Some(11));
    assert_eq!(simple(&mut c, "SELECT 'abc", "42601"), Some(8));
}

#[test]
fn syntax_error_positions_count_the_whole_query_string() {
    let (_db, mut c) = setup();
    // A later statement of a multi-statement Query: relative to the whole string.
    assert_eq!(simple(&mut c, "SELECT 1; SELEC 2", "42601"), Some(11));
    // Lines: newlines count as one character each.
    assert_eq!(simple(&mut c, "SELECT 1\n  FROM\n  WHERE", "42601"), Some(19));
    // Characters, not bytes.
    assert_eq!(simple(&mut c, "SELECT 'ééé' 'x' FROM", "42601"), Some(14));
    // Comments, tabs and dollar quotes before the error.
    assert_eq!(simple(&mut c, "/* é\n */ SELEC 1", "42601"), Some(10));
    assert_eq!(simple(&mut c, "SELECT\t$$ü$$ FROM;", "42601"), Some(18));
    assert_eq!(simple(&mut c, "-- ü\nSELECT 1; SELECT nope;", "42703"), Some(23));
}

#[test]
fn undefined_column_position() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE pt (a int, b text)").expect("create");
    assert_eq!(simple(&mut c, "SELECT nope", "42703"), Some(8));
    assert_eq!(simple(&mut c, "SELECT t.nope FROM (SELECT 1 AS a) t", "42703"), Some(8));
    assert_eq!(simple(&mut c, "SELECT a FROM pt ORDER BY zz", "42703"), Some(27));
    assert_eq!(simple(&mut c, "UPDATE pt SET a = 1 WHERE zz = 2", "42703"), Some(27));
    assert_eq!(simple(&mut c, "SELECT a FROM pt WHERE a IN (SELECT zz)", "42703"), Some(37));
    // Multi-line, non-ASCII text before the reference, and a later statement.
    assert_eq!(simple(&mut c, "SELECT\n  a,\n  zz\nFROM pt", "42703"), Some(15));
    assert_eq!(simple(&mut c, "SELECT 'ééé', nope", "42703"), Some(15));
    assert_eq!(simple(&mut c, "SELECT 1; SELECT nope", "42703"), Some(18));
    // Target columns of INSERT and UPDATE.
    assert_eq!(simple(&mut c, "INSERT INTO pt (zz) VALUES (1)", "42703"), Some(17));
    assert_eq!(simple(&mut c, "UPDATE pt SET zz = 1", "42703"), Some(15));
}

#[test]
fn missing_from_entry_and_ambiguous_column_positions() {
    let (_db, mut c) = setup();
    assert_eq!(simple(&mut c, "SELECT u.a FROM (SELECT 1 AS a) t", "42P01"), Some(8));
    assert_eq!(simple(&mut c, "SELECT a FROM (SELECT 1 AS a) x, (SELECT 2 AS a) y", "42702"), Some(8));
}

#[test]
fn undefined_relation_position() {
    let (_db, mut c) = setup();
    assert_eq!(simple(&mut c, "SELECT * FROM nope", "42P01"), Some(15));
    assert_eq!(simple(&mut c, "SELECT * FROM public.nope", "42P01"), Some(15));
    assert_eq!(simple(&mut c, "INSERT INTO nope VALUES (1)", "42P01"), Some(13));
    assert_eq!(simple(&mut c, "UPDATE nope SET a = 1", "42P01"), Some(8));
    assert_eq!(simple(&mut c, "DELETE FROM nope", "42P01"), Some(13));
    // Utility statements carry no position, as in Postgres.
    assert_eq!(simple(&mut c, "TRUNCATE nope", "42P01"), None);
    assert_eq!(simple(&mut c, "DROP TABLE nope", "42P01"), None);
}

#[test]
fn undefined_function_and_operator_positions() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE pt (a int, b text)").expect("create");
    assert_eq!(simple(&mut c, "SELECT nofunc(1)", "42883"), Some(8));
    assert_eq!(simple(&mut c, "SELECT public.nofunc(1)", "42883"), Some(8));
    assert_eq!(simple(&mut c, "SELECT sum(b) FROM pt", "42883"), Some(8));
    assert_eq!(simple(&mut c, "SELECT 1 + 'a'::text", "42883"), Some(10));
    assert_eq!(simple(&mut c, "SELECT * FROM pt WHERE b = 1", "42883"), Some(26));
    assert_eq!(simple(&mut c, "SELECT a + b FROM pt", "42883"), Some(10));
}

#[test]
fn invalid_literal_position() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE pt (a int, b text)").expect("create");
    assert_eq!(simple(&mut c, "SELECT 'abc'::int", "22P02"), Some(8));
    assert_eq!(simple(&mut c, "SELECT 1 + 'x'", "22P02"), Some(12));
    assert_eq!(simple(&mut c, "INSERT INTO pt VALUES ('x')", "22P02"), Some(24));
}

#[test]
fn extended_protocol_parse_errors_carry_positions() {
    let (_db, mut c) = setup();
    assert_eq!(extended(&mut c, "SELEC 1", "42601"), Some(1));
    assert_eq!(extended(&mut c, "SELECT nope", "42703"), Some(8));
    assert_eq!(extended(&mut c, "SELECT * FROM nope WHERE true", "42P01"), Some(15));
    assert_eq!(extended(&mut c, "SELECT nofunc($1::int)", "42883"), Some(8));
}

#[test]
fn errors_without_a_position_stay_without() {
    let (_db, mut c) = setup();
    // Run-time errors have no position.
    assert_eq!(simple(&mut c, "SELECT 1 / 0", "22012"), None);
    // Errors inside a PL/pgSQL body point into the body (internal position), never into the
    // DO statement.
    assert_eq!(simple(&mut c, "DO $$BEGIN PERFORM nope; END$$", "42703"), None);
}
