//! Range and multirange types (input, canonical output, operators, functions, range_agg),
//! exclusion constraints and `WITHOUT OVERLAPS` keys.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

#[test]
fn range_input_output_and_canonical_forms() {
    let (_db, mut c) = setup();
    assert_eq!(
        rows(
            &mut c,
            "SELECT int4range(1, 10, '(]'), int4range(NULL, NULL), int4range(3, 3), int4range(3, 3, '[]'), '( 1 , 5 ]'::int4range, numrange(1.5, 2.5, '[]')"
        ),
        [["[2,11)", "(,)", "empty", "[3,4)", "[2,6)", "[1.5,2.5]"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT tsrange('2024-01-01 00:00', '2024-01-02 00:00'), daterange('2024-01-01', '2024-01-31', '[]')"),
        [["[\"2024-01-01 00:00:00\",\"2024-01-02 00:00:00\")", "[2024-01-01,2024-02-01)"]]
    );
    let e = expect_code(c.batch_execute("SELECT '[1,5'::int4range"), "22P02");
    assert_eq!(message(&e), "malformed range literal: \"[1,5\"");
    let e = expect_code(c.batch_execute("SELECT int4range(5, 1)"), "22000");
    assert_eq!(message(&e), "range lower bound must be less than or equal to range upper bound");
    let e = expect_code(c.batch_execute("SELECT int4range(1, 2, 'x')"), "42601");
    assert_eq!(message(&e), "invalid range bound flags");
    assert_eq!(
        rows(&mut c, "SELECT pg_typeof(int4range(1, 2))::text, pg_typeof('{}'::datemultirange)::text"),
        [["int4range", "datemultirange"]]
    );
    // Ordering: empty first, then by lower bound (unbounded lowest), then upper.
    assert_eq!(
        rows(&mut c, "SELECT r FROM (VALUES ('[5,6)'::int4range), ('empty'), ('[1,9)'), ('(,3)')) v(r) ORDER BY r"),
        [["empty"], ["(,3)"], ["[1,9)"], ["[5,6)"]]
    );
    assert_eq!(rows(&mut c, "SELECT count(DISTINCT r) FROM (VALUES (int4range(1, 3)), ('[1,2]'::int4range)) v(r)"), [["1"]]);
}

#[test]
fn range_operators_and_functions() {
    let (_db, mut c) = setup();
    assert_eq!(
        rows(
            &mut c,
            "SELECT int4range(1, 10) @> 5, int4range(1, 10) @> int4range(2, 3), int4range(1, 5) && int4range(4, 8), 3 <@ int4range(1, 3)"
        ),
        [["t", "t", "t", "f"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT int4range(1, 5) << int4range(5, 8), int4range(1, 5) >> int4range(-3, 0), int4range(1, 5) &< int4range(3, 8), int4range(1, 5) &> int4range(0, 3)"
        ),
        [["t", "t", "t", "t"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT numrange(1, 3, '[]') -|- numrange(3, 5, '(]'), numrange(1, 3) -|- numrange(3, 5), int4range(1, 3, '[]') -|- int4range(4, 6)"
        ),
        [["t", "t", "t"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT int4range(1, 5) + int4range(3, 8), int4range(1, 10) * int4range(5, 15), int4range(1, 10) - int4range(5, 15), range_merge(int4range(1, 3), int4range(5, 8))"
        ),
        [["[1,8)", "[5,10)", "[1,5)", "[1,8)"]]
    );
    let e = expect_code(c.batch_execute("SELECT int4range(1, 3) + int4range(5, 8)"), "22000");
    assert_eq!(message(&e), "result of range union would not be contiguous");
    let e = expect_code(c.batch_execute("SELECT int4range(1, 10) - int4range(3, 5)"), "22000");
    assert_eq!(message(&e), "result of range difference would not be contiguous");
    assert_eq!(
        rows(
            &mut c,
            "SELECT lower(int4range(1, 10)), upper(int4range(1, NULL)), isempty(int4range(5, 5)), lower_inc('(1,5]'::int4range), upper_inf(int4range(1, NULL)), lower('empty'::int4range)"
        ),
        [["1", "NULL", "t", "t", "t", "NULL"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT '{[1,3),[2,6),empty,[8,9)}'::int4multirange, int4multirange(), '{[1,3),[5,7)}'::int4multirange @> 6"),
        [["{[1,6),[8,9)}", "{}", "t"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT range_agg(r), range_intersect_agg(r) FROM (VALUES (int4range(1, 3)), (int4range(2, 5)), (NULL), (int4range(4, 11))) v(r)"
        ),
        [["{[1,11)}", "empty"]]
    );
}

#[test]
fn exclusion_constraints_and_temporal_keys() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE bookings (room int, during tsrange, EXCLUDE USING gist (during WITH &&));
         INSERT INTO bookings VALUES (1, '[2024-01-01 10:00, 2024-01-01 11:00)'), (1, '[2024-01-01 11:00, 2024-01-01 12:00)')",
    )
    .expect("setup");
    let e = expect_code(c.batch_execute("INSERT INTO bookings VALUES (2, '[2024-01-01 10:30, 2024-01-01 10:45)')"), "23P01");
    assert_eq!(message(&e), "conflicting key value violates exclusion constraint \"bookings_during_excl\"");
    c.batch_execute("CREATE TABLE eq (a int, EXCLUDE (a WITH =)); INSERT INTO eq VALUES (1), (2), (NULL), (NULL)").expect("btree equality");
    expect_code(c.batch_execute("UPDATE eq SET a = 1 WHERE a = 2"), "23P01");
    let e = expect_code(
        c.batch_execute("CREATE TABLE rooms (room int, during tsrange, EXCLUDE USING gist (room WITH =, during WITH &&))"),
        "42704",
    );
    assert_eq!(message(&e), "data type integer has no default operator class for access method \"gist\"");
    c.batch_execute(
        "CREATE TABLE shifts (worker int, during int4range, active boolean, CONSTRAINT no_overlap EXCLUDE USING gist (during WITH &&) WHERE (active));
         INSERT INTO shifts VALUES (1, '[1,5)', true), (2, '[3,8)', false)",
    )
    .expect("partial exclusion");
    expect_code(c.batch_execute("INSERT INTO shifts VALUES (3, '[4,6)', true)"), "23P01");
    c.batch_execute("CREATE TABLE later (r int4range); INSERT INTO later VALUES ('[1,3)'), ('[2,4)')").expect("rows");
    let e = expect_code(c.batch_execute("ALTER TABLE later ADD EXCLUDE USING gist (r WITH &&)"), "23P01");
    assert_eq!(message(&e), "could not create exclusion constraint \"later_r_excl\"");
    c.batch_execute("DELETE FROM later WHERE r = '[2,4)'; ALTER TABLE later ADD CONSTRAINT later_excl EXCLUDE USING gist (r WITH &&)")
        .expect("add");
    assert_eq!(
        rows(&mut c, "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'later'::regclass"),
        [["later_excl", "x", "EXCLUDE USING gist (r WITH &&)"]]
    );
    // Deferred: checked at commit.
    c.batch_execute(
        "CREATE TABLE dex (r int4range, EXCLUDE USING gist (r WITH &&) DEFERRABLE INITIALLY DEFERRED);
         BEGIN; INSERT INTO dex VALUES ('[1,5)'), ('[3,7)'); UPDATE dex SET r = '[5,7)' WHERE r = '[3,7)'; COMMIT",
    )
    .expect("deferred exclusion");
    c.batch_execute("BEGIN; INSERT INTO dex VALUES ('[6,9)')").expect("pending");
    let e = expect_code(c.batch_execute("COMMIT"), "23P01");
    assert_eq!(message(&e), "conflicting key value violates exclusion constraint \"dex_r_excl\"");
    // Postgres 18 temporal keys.
    c.batch_execute(
        "CREATE TABLE temporal_r (id int4range, valid daterange, PRIMARY KEY (id, valid WITHOUT OVERLAPS));
         INSERT INTO temporal_r VALUES ('[1,2)', '[2024-01-01,2024-02-01)'), ('[1,2)', '[2024-02-01,2024-03-01)')",
    )
    .expect("temporal key");
    let e = expect_code(c.batch_execute("INSERT INTO temporal_r VALUES ('[1,2)', '[2024-01-15,2024-01-20)')"), "23P01");
    assert_eq!(message(&e), "conflicting key value violates exclusion constraint \"temporal_r_pkey\"");
    assert_eq!(
        rows(&mut c, "SELECT contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'temporal_r_pkey'"),
        [["p", "PRIMARY KEY (id, valid WITHOUT OVERLAPS)"]]
    );
}

#[test]
fn multirange_operators_unnest_and_casts() {
    let (_db, mut c) = setup();
    assert_eq!(
        rows(
            &mut c,
            "SELECT '{[1,3),[5,8)}'::int4multirange + '{[3,5)}', '{[1,10)}'::int4multirange - '{[2,3),[5,6),[9,12)}', \
             '{[1,10),[20,30)}'::int4multirange * '{[5,25)}', '{(,)}'::int4multirange - '{[1,2)}'"
        ),
        [["{[1,8)}", "{[1,2),[3,5),[6,9)}", "{[5,10),[20,25)}", "{(,1),[2,)}"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT '{[1,3),[5,8)}'::int4multirange @> '{[1,2),[6,7)}', '{[1,3),[5,8)}'::int4multirange @> '{[1,6)}'::int4multirange, \
             int4range(1, 9) @> '{[2,3),[5,6)}'::int4multirange, 4 <@ '{[1,3),[5,8)}'::int4multirange, \
             '{[1,3),[5,8)}'::int4multirange << int4range(8, 9), int4range(-1, 1) -|- '{[1,3),[7,9)}'::int4multirange, \
             '{}'::int4multirange && int4range(1, 2)"
        ),
        [["t", "f", "t", "f", "t", "t", "f"]]
    );
    let e = expect_code(c.batch_execute("SELECT '{[1,3)}'::int4multirange + int4range(3, 4)"), "42883");
    assert_eq!(message(&e), "operator does not exist: int4multirange + int4range");
    assert_eq!(rows(&mut c, "SELECT x FROM unnest('{[1,3),[5,8)}'::int4multirange) AS x"), [["[1,3)"], ["[5,8)"]]);
    assert_eq!(
        rows(&mut c, "SELECT '[1,5)'::int4range::int4multirange, range_cmp(int4range(1, 2), int4range(1, 3)), int4range(1, 5) || 'x'"),
        [["{[1,5)}", "-1", "[1,5)x"]]
    );
}

#[test]
fn range_bounds_resolution_and_catalog_rows() {
    let (_db, mut c) = setup();
    // daterange_canonical leaves infinite bounds inclusive.
    assert_eq!(
        rows(&mut c, "SELECT '[2024-01-01,infinity]'::daterange, '[-infinity,infinity]'::daterange, '[infinity,infinity]'::daterange"),
        [["[2024-01-01,infinity]", "[-infinity,infinity]", "[infinity,infinity]"]]
    );
    // anyelement takes the subtype exactly; bounds take implicit casts only.
    let e = expect_code(c.batch_execute("SELECT numrange(1, 2) @> 2"), "42883");
    assert_eq!(message(&e), "operator does not exist: numrange @> integer");
    let e = expect_code(c.batch_execute("SELECT int4range(1.5, 3)"), "42883");
    assert_eq!(message(&e), "function int4range(numeric, integer) does not exist");
    let e = expect_code(c.batch_execute("SELECT max(r) FROM (VALUES (int4range(1, 2))) v(r)"), "42883");
    assert_eq!(message(&e), "function max(int4range) does not exist");
    assert_eq!(
        rows(&mut c, "SELECT rngsubopc, rngsubdiff::text FROM pg_range WHERE rngtypid = 'int8range'::regtype"),
        [["3124", "int8range_subdiff"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT typalign::text, typanalyze::text FROM pg_type WHERE typname IN ('tstzrange', '_int4range') ORDER BY oid"),
        [["i", "array_typanalyze"], ["d", "range_typanalyze"]]
    );
}
