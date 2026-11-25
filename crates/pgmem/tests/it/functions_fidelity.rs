//! Function sweep: numeric precision of exp / ln / log / power, degree trigonometry,
//! statistical, ordered-set and hypothetical-set aggregates, regular expression functions,
//! string functions and multidimensional arrays.

use crate::support::{expect_code, message, setup, text_value, texts};

fn one(c: &mut postgres::Client, sql: &str) -> String {
    text_value(c, sql).unwrap_or_else(|| "NULL".into())
}

fn row(c: &mut postgres::Client, sql: &str) -> Vec<String> {
    texts(c, sql).into_iter().next().unwrap_or_default().into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()
}

#[test]
fn numeric_transcendental_scales() {
    let (_db, mut c) = setup();
    assert_eq!(
        row(&mut c, "SELECT exp(1.0), exp(-10.5), ln(10::numeric), ln(2.718281828459045)"),
        ["2.7182818284590452", "0.00002753644934974716", "2.3025850929940457", "0.9999999999999999"]
    );
    assert_eq!(
        row(&mut c, "SELECT log(2.0, 64.0), log(0.001::numeric), power(2.0, 3), power(9::numeric, 0.5), power(-8, 3.0)"),
        ["6.0000000000000000", "-3.0000000000000000", "8.0000000000000000", "3.0000000000000000", "-512.00000000000000"]
    );
    assert_eq!(one(&mut c, "SELECT 2.0 ^ 0.5"), "1.4142135623730950");
    let e = expect_code(c.simple_query("SELECT power(-8.0, 0.5)"), "2201F");
    assert_eq!(message(&e), "a negative number raised to a non-integer power yields a complex result");
    expect_code(c.simple_query("SELECT ln(0::numeric)"), "2201E");
    expect_code(c.simple_query("SELECT round(42.4382::float8, 2)"), "42883");
}

#[test]
fn trigonometry_in_degrees() {
    let (_db, mut c) = setup();
    assert_eq!(
        row(&mut c, "SELECT sind(30), cosd(60), tand(45), asind(0.5), acosd(0.5), atand(1), atan2d(1, 0), cotd(45)"),
        ["0.5", "0.5", "1", "30", "60", "45", "90", "1"]
    );
    assert_eq!(row(&mut c, "SELECT sinh(0), cosh(0), tanh(0), asinh(0), acosh(1), atanh(0)"), ["0", "1", "0", "0", "0", "0"]);
    let e = expect_code(c.simple_query("SELECT asin(2)"), "22003");
    assert_eq!(message(&e), "input is out of range");
    assert_eq!(one(&mut c, "SELECT width_bucket(7, ARRAY[1, 5, 10])"), "2");
}

#[test]
fn statistical_and_ordered_set_aggregates() {
    let (_db, mut c) = setup();
    assert_eq!(
        row(&mut c, "SELECT stddev(x::float8), variance(x::float8) FROM (VALUES (1), (2), (4)) v(x)"),
        ["1.5275252316519468", "2.3333333333333335"]
    );
    assert_eq!(
        row(&mut c, "SELECT stddev(x), variance(x) FROM (VALUES (1.5), (2.25), (4.125), (7.0)) v(x)"),
        ["2.4502869729890824", "6.0039062500000000"]
    );
    assert_eq!(
        row(
            &mut c,
            "SELECT corr(x, y), covar_pop(x, y), regr_slope(y, x), regr_intercept(y, x), regr_count(y, x) FROM (VALUES (1, 2), (2, 4), (3, 7), (NULL, 3)) v(x, y)"
        ),
        ["0.9933992677987828", "1.6666666666666667", "2.5", "-0.6666666666666666", "3"]
    );
    assert_eq!(row(&mut c, "SELECT bit_and(x), bit_or(x), bit_xor(x) FROM (VALUES (12), (10)) v(x)"), ["8", "14", "6"]);
    assert_eq!(
        row(
            &mut c,
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x), percentile_disc(0.5) WITHIN GROUP (ORDER BY x), mode() WITHIN GROUP (ORDER BY x) FROM (VALUES (1), (2), (2), (4)) v(x)"
        ),
        ["2", "2", "2"]
    );
    assert_eq!(
        row(
            &mut c,
            "SELECT percentile_cont(ARRAY[0, 0.25, 0.5, 1]) WITHIN GROUP (ORDER BY x), percentile_disc(ARRAY[0, 0.3, 1]) WITHIN GROUP (ORDER BY x DESC) FROM generate_series(1, 10) x"
        ),
        ["{1,3.25,5.5,10}", "{10,8,1}"]
    );
    assert_eq!(
        one(
            &mut c,
            "SELECT percentile_cont(0.3) WITHIN GROUP (ORDER BY d) FROM (VALUES (interval '1 day'), (interval '3 days'), (interval '10 hours')) v(d)"
        ),
        "18:24:00"
    );
    assert_eq!(
        row(
            &mut c,
            "SELECT rank(5) WITHIN GROUP (ORDER BY x), dense_rank(5) WITHIN GROUP (ORDER BY x), percent_rank(5) WITHIN GROUP (ORDER BY x), cume_dist(5) WITHIN GROUP (ORDER BY x) FROM (VALUES (5), (6), (5), (10), (NULL), (NULL)) v(x)"
        ),
        ["1", "1", "0", "0.42857142857142855"]
    );
    let e = expect_code(c.simple_query("SELECT percentile_cont(1.5) WITHIN GROUP (ORDER BY x) FROM (VALUES (1)) v(x)"), "22003");
    assert_eq!(message(&e), "percentile value 1.5 is not between 0 and 1");
    let e = expect_code(c.simple_query("SELECT count(*) WITHIN GROUP (ORDER BY 1)"), "42809");
    assert_eq!(message(&e), "count is not an ordered-set aggregate, so it cannot have WITHIN GROUP");
    let e = expect_code(c.simple_query("SELECT count(*) FILTER (WHERE sum(x) > 1) FROM (VALUES (1)) v(x)"), "42803");
    assert_eq!(message(&e), "aggregate functions are not allowed in FILTER");
}

#[test]
fn regular_expression_functions() {
    let (_db, mut c) = setup();
    assert_eq!(
        texts(&mut c, "SELECT regexp_matches('a1b2c3', '([a-z])(\\d)', 'g')"),
        vec![vec![Some("{a,1}".into())], vec![Some("{b,2}".into())], vec![Some("{c,3}".into())]]
    );
    assert_eq!(one(&mut c, "SELECT regexp_replace('A PostgreSQL function', 'a|e|i|o|u', 'X', 1, 3, 'i')"), "A PostgrXSQL function");
    assert_eq!(row(&mut c, "SELECT regexp_replace('abc', 'b*', '-', 'g'), regexp_replace('a.c', '.', 'x', 'q')"), ["-a--c-", "axc"]);
    assert_eq!(row(&mut c, "SELECT regexp_count('ABCABCAXYaxy', 'A.', 1, 'i'), regexp_count('aaa', 'a*')"), ["4", "2"]);
    assert_eq!(
        row(&mut c, "SELECT regexp_instr('ABCDEFGHI', '(c..)(...)', 1, 1, 0, 'i', 2), regexp_instr('abcabc', 'b', 1, 2, 1)"),
        ["6", "6"]
    );
    assert_eq!(row(&mut c, "SELECT regexp_substr('number of your row', '\\w+', 1, 3), regexp_substr('abc', 'z')"), ["your", "NULL"]);
    assert_eq!(
        texts(&mut c, "SELECT regexp_split_to_table('hello world', '\\s*')").len(),
        10,
        "zero-length matches at the ends and next to other matches are ignored"
    );
    let e = expect_code(c.simple_query("SELECT 'abc' ~ '('"), "2201B");
    assert_eq!(message(&e), "invalid regular expression: parentheses () not balanced");
}

#[test]
fn string_functions() {
    let (_db, mut c) = setup();
    assert_eq!(
        row(&mut c, "SELECT substring('Thomas' SIMILAR '%#\"o_a#\"_' ESCAPE '#'), substring('Thomas' FROM '%#\"o_a#\"_' FOR '#')"),
        ["oma", "oma"]
    );
    assert_eq!(row(&mut c, "SELECT concat(VARIADIC ARRAY['a', 'b', 'c']), num_nulls(VARIADIC ARRAY[1, NULL, 3])"), ["abc", "1"]);
    assert_eq!(
        row(&mut c, "SELECT format('%*s|', 6, 'ab'), format('%-*s|', 6, 'ab'), format('%1$*2$s|', 'x', 4)"),
        ["    ab|", "ab    |", "   x|"]
    );
    assert_eq!(
        row(&mut c, "SELECT quote_literal(42.5), to_bin(10), to_oct(64), to_bin(-1)"),
        ["'42.5'", "1010", "100", "11111111111111111111111111111111"]
    );
    assert_eq!(texts(&mut c, "SELECT * FROM string_to_table('a|b|c', '|')").len(), 3);
    assert_eq!(row(&mut c, "SELECT unistr('d\\0061t\\+000061'), normalize('abc'), 'abc' IS NFC NORMALIZED"), ["data", "abc", "t"]);
}

#[test]
fn multidimensional_arrays() {
    let (_db, mut c) = setup();
    assert_eq!(
        row(&mut c, "SELECT ARRAY[[1, 2], [3, 4]], (ARRAY[[1, 2], [3, 4]])[2][1], (ARRAY[[1, 2], [3, 4]])[1:2][2:2]"),
        ["{{1,2},{3,4}}", "3", "{{2},{4}}"]
    );
    assert_eq!(
        row(&mut c, "SELECT array_dims(ARRAY[[1, 2], [3, 4]]), array_length(ARRAY[[1, 2, 3]], 2), cardinality(ARRAY[[1, 2], [3, 4]])"),
        ["[1:2][1:2]", "3", "4"]
    );
    assert_eq!(texts(&mut c, "SELECT unnest(ARRAY[[1, 2], [3, 4]])").len(), 4);
    assert_eq!(one(&mut c, "SELECT array_fill(0, ARRAY[2, 2])"), "{{0,0},{0,0}}");
    let e = expect_code(c.simple_query("SELECT ARRAY[[1, 2], [3]]"), "2202E");
    assert_eq!(message(&e), "multidimensional arrays must have array expressions with matching dimensions");
    c.batch_execute("CREATE TABLE grid (id int, cells int[]); INSERT INTO grid VALUES (1, '{{1,2},{3,4}}')").expect("ddl");
    c.batch_execute("UPDATE grid SET cells[2][2] = 40").expect("update");
    assert_eq!(one(&mut c, "SELECT cells FROM grid"), "{{1,2},{3,40}}");
    // Set-returning functions in the select list run in lockstep.
    assert_eq!(
        texts(&mut c, "SELECT generate_series(1, 3), unnest(ARRAY['a', 'b'])"),
        vec![vec![Some("1".into()), Some("a".into())], vec![Some("2".into()), Some("b".into())], vec![Some("3".into()), None]]
    );
}
