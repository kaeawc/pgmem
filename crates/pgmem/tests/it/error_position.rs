//! The ErrorResponse `position` field (`P`): the 1-based character offset into the query string
//! as sent that Postgres reports for syntax errors and parse-analysis errors (#245, #166).

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

/// A table the statements below read, and an assertion helper: `at(sql, code, position)`.
fn with_tables() -> (crate::support::TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE pt (a int, b text); CREATE TABLE pu (a int, d text)").expect("create");
    (db, c)
}

#[track_caller]
fn at(c: &mut Client, sql: &str, code: &str, position: Option<u32>) {
    assert_eq!(simple(c, sql, code), position, "{sql}");
}

#[test]
fn unknown_type_positions() {
    let (_db, mut c) = with_tables();
    // Multi-line text, as in a migration file.
    at(&mut c, "CREATE TABLE d (\n    id nosuchtype\n)", "42704", Some(25));
    at(&mut c, "CREATE TABLE d2 (id int, x public.nosuchtype)", "42704", Some(28));
    at(&mut c, "CREATE TABLE d3 (id int, x nosuch.foo)", "3F000", Some(28));
    at(&mut c, "CREATE TABLE d4 (id nosuchtype[])", "42704", Some(21));
    at(&mut c, "SELECT 1::nosuchtype", "42704", Some(11));
    at(&mut c, "SELECT CAST(1 AS nosuchtype)", "42704", Some(18));
    at(&mut c, "SELECT nosuchtype 'x'", "42704", Some(8));
    at(&mut c, "CREATE DOMAIN dd AS nosuchtype", "42704", Some(21));
    at(&mut c, "CREATE SEQUENCE sq AS nosuchtype", "42704", Some(23));
    at(&mut c, "PREPARE p (nosuchtype) AS SELECT 1", "42704", Some(12));
    at(&mut c, "ALTER TABLE pt ADD COLUMN z nosuchtype", "42704", Some(29));
    at(&mut c, "ALTER TABLE pt ALTER COLUMN a TYPE nosuchtype", "42704", Some(36));
    // A later statement of the query string, after a comment with a multi-byte character.
    at(&mut c, "SELECT 1; -- é\nSELECT 2;\n SELECT nosuchtype 'x'", "42704", Some(34));
    // The message names an array type with its brackets.
    let e = expect_code(c.batch_execute("CREATE TABLE d5 (id nosuchtype[])"), "42704");
    assert_eq!(crate::support::message(&e), "type \"nosuchtype[]\" does not exist");
}

#[test]
fn unknown_types_without_a_position() {
    let (_db, mut c) = with_tables();
    // Postgres locates none of these.
    at(&mut c, "CREATE TYPE cc AS (a nosuchtype)", "42704", None);
    at(&mut c, "CREATE FUNCTION f() RETURNS nosuchtype LANGUAGE sql AS 'select 1'", "42704", None);
    at(&mut c, "DROP FUNCTION nofn(nosuchtype)", "42704", None);
}

#[test]
fn function_parameter_types_are_reported_unquoted() {
    let (_db, mut c) = with_tables();
    let sql = "CREATE FUNCTION f(x nosuchtype) RETURNS int LANGUAGE sql AS 'select 1'";
    let e = expect_code(c.batch_execute(sql), "42704");
    assert_eq!(crate::support::message(&e), "type nosuchtype does not exist");
    assert_eq!(original(&e), Some(21));
}

#[test]
fn grouping_errors_point_at_the_column_or_aggregate() {
    let (_db, mut c) = with_tables();
    at(&mut c, "SELECT a, count(*) FROM pt", "42803", Some(8));
    at(&mut c, "SELECT a, b FROM pt GROUP BY a", "42803", Some(11));
    // Not the first mention: the one outside an aggregate.
    at(&mut c, "SELECT sum(a), a FROM pt", "42803", Some(16));
    at(&mut c, "SELECT pt.a FROM pt GROUP BY pt.b", "42803", Some(8));
    at(&mut c, "SELECT upper(b) FROM pt GROUP BY a", "42803", Some(14));
    at(&mut c, "SELECT sum(count(a)) FROM pt", "42803", Some(12));
}

#[test]
fn misplaced_aggregates_and_window_calls_point_at_the_call() {
    let (_db, mut c) = with_tables();
    at(&mut c, "SELECT * FROM pt WHERE count(*) > 1", "42803", Some(24));
    // The one in WHERE, not the one in the select list.
    at(&mut c, "SELECT sum(a) FROM pt WHERE sum(a) > 1", "42803", Some(29));
    at(&mut c, "UPDATE pt SET a = 1 WHERE sum(a) > 1", "42803", Some(27));
    at(&mut c, "SELECT a FROM pt GROUP BY count(*)", "42803", Some(27));
    at(&mut c, "SELECT a FROM pt JOIN pu ON sum(pt.a) > 1", "42803", Some(29));
    at(&mut c, "SELECT a FROM pt GROUP BY a HAVING a > 0 AND row_number() OVER () > 1", "42P20", Some(46));
    at(&mut c, "CREATE TABLE x (a int DEFAULT sum(1))", "42803", Some(31));
    at(&mut c, "CREATE TABLE x (a int CHECK (sum(a) > 1))", "42803", Some(30));
}

#[test]
fn operator_positions_tell_equal_operators_apart() {
    let (_db, mut c) = with_tables();
    at(&mut c, "SELECT -b FROM pt", "42883", Some(8));
    at(&mut c, "SELECT ~b FROM pt", "42883", Some(8));
    // The second `+` is the one that fails.
    at(&mut c, "SELECT a + 1, b + 1 FROM pt", "42883", Some(17));
    at(&mut c, "SELECT a = 1, b = 1 FROM pt", "42883", Some(17));
}

#[test]
fn type_mismatch_positions_point_at_the_expression() {
    let (_db, mut c) = with_tables();
    // The same expression in the select list and in WHERE: the one in WHERE is wrong.
    at(&mut c, "SELECT a FROM pt WHERE a", "42804", Some(24));
    at(&mut c, "SELECT a, NOT a FROM pt", "42804", Some(15));
    at(&mut c, "SELECT CASE WHEN a THEN 1 END FROM pt", "42804", Some(18));
    at(&mut c, "SELECT a FROM pt WHERE a AND b", "42804", Some(24));
    at(&mut c, "SELECT a FROM pt WHERE a > 0 AND a", "42804", Some(34));
    at(&mut c, "SELECT coalesce(a, b) FROM pt", "42804", Some(20));
    at(&mut c, "SELECT greatest(a, b) FROM pt", "42804", Some(20));
    at(&mut c, "SELECT ARRAY[1, 'a'::text]", "42804", Some(17));
}

#[test]
fn dml_and_clause_positions() {
    let (_db, mut c) = with_tables();
    // The repeated column, not the first mention.
    at(&mut c, "INSERT INTO pt (a, a) VALUES (1, 2)", "42701", Some(20));
    // The first expression without a column, in the row that has it.
    at(&mut c, "INSERT INTO pt VALUES (1, 'a', 3), (2, 'b')", "42601", Some(32));
    at(&mut c, "INSERT INTO pt VALUES (1, 'a'), (2, 'b', 3)", "42601", Some(34));
    at(&mut c, "SELECT a FROM pt ORDER BY 5", "42P10", Some(27));
    // Not the `6` of the select list.
    at(&mut c, "SELECT 6 FROM pt GROUP BY 6, 7", "42P10", Some(27));
    at(&mut c, "SELECT x.* FROM pt", "42P01", Some(8));
    at(&mut c, "SELECT a FROM pt FOR UPDATE OF zz", "42P01", Some(32));
    at(&mut c, "SELECT * FROM nofunc(1)", "42883", Some(15));
    at(&mut c, "SELECT * FROM generate_series(1, 2) WHERE nofn(1)", "42883", Some(43));
    at(&mut c, "SELECT t1.a FROM pt t1 JOIN pu t2 ON t1.a = t2.zz", "42703", Some(45));
    at(&mut c, "SELECT 'ééé', pt.zz FROM pt", "42703", Some(15));
}

#[test]
fn syntax_errors_in_unterminated_constructs_and_unlocated_parser_errors() {
    let (_db, mut c) = setup();
    assert_eq!(simple(&mut c, "SELECT 1 AS \"abc", "42601"), Some(13));
    assert_eq!(simple(&mut c, "SELECT /* abc", "42601"), Some(8));
    assert_eq!(simple(&mut c, "SELECT 1; SELECT $tag$abc", "42601"), Some(18));
    assert_eq!(simple(&mut c, "SELECT 'é', FROM", "42601"), Some(13));
    assert_eq!(simple(&mut c, "SELECT $", "42601"), Some(8));
    let e = expect_code(c.batch_execute("SELECT 1 AS \"abc"), "42601");
    assert_eq!(crate::support::message(&e), "unterminated quoted identifier at or near \"\"abc\"");
}

#[test]
fn rewritten_statements_keep_positions_into_the_text_as_sent() {
    let (_db, mut c) = with_tables();
    // Each of these is rewritten before parsing; the error comes after the rewritten part.
    at(&mut c, "SELECT concat(VARIADIC ARRAY['a']), nope FROM pt", "42703", Some(37));
    at(&mut c, "SELECT substring('abc' SIMILAR 'a' ESCAPE '#'), nope", "42703", Some(49));
    at(&mut c, "SELECT a FROM pt FOR NO KEY UPDATE OF zz", "42P01", Some(39));
}

#[test]
fn alter_table_expressions_and_domain_checks_carry_no_position() {
    let (_db, mut c) = with_tables();
    at(&mut c, "ALTER TABLE pt ALTER COLUMN a SET DEFAULT 'x'", "22P02", None);
    at(&mut c, "ALTER TABLE pt ADD CONSTRAINT ck CHECK (zz = 1)", "42703", None);
    at(&mut c, "ALTER TABLE pt ADD COLUMN z int DEFAULT 'x'", "22P02", None);
    // CREATE TABLE's do.
    at(&mut c, "CREATE TABLE x (a int DEFAULT 'x')", "22P02", Some(31));
}

#[test]
fn constraint_and_domain_grammar_errors_have_positions() {
    let (_db, mut c) = with_tables();
    // A key naming an unknown column is located at its constraint.
    at(&mut c, "CREATE TABLE k1 (a int, PRIMARY KEY (zz))", "42703", Some(25));
    at(&mut c, "CREATE TABLE k2 (a int, UNIQUE (a, zz))", "42703", Some(25));
    at(&mut c, "CREATE TABLE k3 (a int, CONSTRAINT u UNIQUE (zz))", "42703", Some(25));
    at(&mut c, "CREATE TABLE k4 (a int) PARTITION BY RANGE (zz)", "42703", Some(45));
    // Conflicting domain clauses: at the one that conflicts.
    at(&mut c, "CREATE DOMAIN dm AS int NOT NULL NULL", "42601", Some(34));
    at(&mut c, "CREATE DOMAIN dm AS int DEFAULT 1 DEFAULT 2", "42601", Some(35));
    at(&mut c, "CREATE DOMAIN dm AS int COLLATE \"C\" COLLATE \"C\"", "42601", Some(37));
    at(&mut c, "CREATE DOMAIN dm AS int COLLATE \"C\"", "42804", Some(21));
}

#[test]
fn extended_protocol_parse_reports_the_new_positions() {
    let (_db, mut c) = with_tables();
    assert_eq!(extended(&mut c, "SELECT 1::nosuchtype", "42704"), Some(11));
    assert_eq!(extended(&mut c, "SELECT a, count(*) FROM pt", "42803"), Some(8));
    assert_eq!(extended(&mut c, "SELECT a FROM pt WHERE a", "42804"), Some(24));
}
