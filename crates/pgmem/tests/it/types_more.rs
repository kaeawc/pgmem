//! Array lower bounds, bit strings, money, inet / cidr / macaddr, `_name` array type names,
//! and pretty-mode deparsing (`pg_get_viewdef(v, true)`, `pg_get_constraintdef(c, true)`).

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

#[test]
fn array_lower_bounds() {
    let (_db, mut c) = setup();
    assert_eq!(
        rows(
            &mut c,
            "SELECT '[2:4]={1,2,3}'::int[], ('[2:4]={1,2,3}'::int[])[2], ('[2:4]={1,2,3}'::int[])[1], array_dims('[2:4]={1,2,3}'::int[]), array_lower('[0:2]={a,b,c}'::text[], 1), array_upper('[-2:0]={1,2,3}'::int[], 1)"
        ),
        [["[2:4]={1,2,3}", "1", "NULL", "[2:4]", "0", "0"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT '[2:3]={1,2}'::int[] = '{1,2}'::int[], array_length('[0:2]={a,b,c}'::text[], 1), array_fill(1, ARRAY[2], ARRAY[5]), '[2:3]={1,2}'::int[]::text[]"
        ),
        [["f", "3", "[5:6]={1,1}", "[2:3]={1,2}"]]
    );
    assert_eq!(rows(&mut c, "SELECT generate_subscripts('[3:5]={x,y,z}'::text[], 1)"), [["3"], ["4"], ["5"]]);
    assert_eq!(rows(&mut c, "SELECT unnest('[0:1]={7,8}'::int[])"), [["7"], ["8"]]);
    let e = expect_code(c.batch_execute("SELECT '[1:3]={1,2}'::int[]"), "22P02");
    assert_eq!(message(&e), "malformed array literal: \"[1:3]={1,2}\"");
    assert_eq!(rows(&mut c, "SELECT '_int4'::regtype::text, format_type('_text'::regtype, -1)"), [["integer[]", "text[]"]]);
}

#[test]
fn bit_strings() {
    let (_db, mut c) = setup();
    assert_eq!(
        rows(&mut c, "SELECT B'1010', B'1010'::bit(6), 'x0F'::bit(8), 5::bit(4), B'1010'::bit(4)::int, B'10101'::bit(3), x'1'::bit(8)"),
        [["1010", "101000", "00001111", "0101", "10", "101", "00010000"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT B'1100' & B'1010', B'1100' | B'1010', B'1100' # B'1010', B'1100' || B'01', B'1001' << 1, B'1001' >> 2, length(B'10101'), get_bit(B'1010', 0), set_bit(B'1010', 1, 1)"
        ),
        [["1000", "1110", "0110", "110001", "0010", "0010", "5", "1", "1110"]]
    );
    let e = expect_code(c.batch_execute("SELECT B'102'"), "22P02");
    assert_eq!(message(&e), "\"2\" is not a valid binary digit");
    let e = expect_code(c.batch_execute("SELECT B'1' & B'10'"), "22026");
    assert_eq!(message(&e), "cannot AND bit strings of different sizes");
    c.batch_execute("CREATE TABLE bt (a bit(3), b varbit(4)); INSERT INTO bt VALUES (B'101', B'11')").expect("insert");
    let e = expect_code(c.batch_execute("INSERT INTO bt VALUES (B'1011', B'1')"), "22026");
    assert_eq!(message(&e), "bit string length 4 does not match type bit(3)");
    let e = expect_code(c.batch_execute("INSERT INTO bt VALUES (B'101', B'11111')"), "22001");
    assert_eq!(message(&e), "bit string too long for type bit varying(4)");
    assert_eq!(rows(&mut c, "SELECT a, b, '101'::bit, '101'::varbit FROM bt"), [["101", "11", "1", "101"]]);
}

#[test]
fn money_and_network_types() {
    let (_db, mut c) = setup();
    assert_eq!(
        rows(
            &mut c,
            "SELECT '$1,234.50'::money, '$1.50'::money + '$2.25', '$10.00'::money * 3, '$10.00'::money / 4, '$10'::money / '$4'::money, '-$3.5'::money, '(1.25)'::money, 12.345::money, '$1,234.50'::money::numeric"
        ),
        [["$1,234.50", "$3.75", "$30.00", "$2.50", "2.5", "-$3.50", "-$1.25", "$12.35", "1234.50"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT '192.168.0.1/24'::inet, '10.0.0.0/8'::cidr, '10.1'::cidr, '192.168.1'::cidr, '::1'::inet, '2001:db8::1/64'::inet, '192.168.0.1'::inet::text"
        ),
        [["192.168.0.1/24", "10.0.0.0/8", "10.1.0.0/16", "192.168.1.0/24", "::1", "2001:db8::1/64", "192.168.0.1/32"]]
    );
    let e = expect_code(c.batch_execute("SELECT 'x'::inet"), "22P02");
    assert_eq!(message(&e), "invalid input syntax for type inet: \"x\"");
    let e = expect_code(c.batch_execute("SELECT '192.168.1.5/24'::cidr"), "22P02");
    assert_eq!(message(&e), "invalid cidr value: \"192.168.1.5/24\"");
    assert_eq!(
        rows(
            &mut c,
            "SELECT host('192.168.1.5/24'::inet), masklen('192.168.1.5/24'::inet), network('192.168.1.5/24'::inet), broadcast('192.168.1.5/24'::inet), netmask('192.168.1.5/24'::inet), family('::1'::inet)"
        ),
        [["192.168.1.5", "24", "192.168.1.0/24", "192.168.1.255/24", "255.255.255.0", "6"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT '192.168.1.5'::inet << '192.168.1.0/24'::inet, '192.168.1.0/24'::inet >>= '192.168.1.0/24', '10.0.0.0/8'::inet && '10.1.0.0/16'"
        ),
        [["t", "t", "t"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT i FROM (VALUES ('10.0.0.1'::inet), ('10.0.0.0/8'), ('::1'), ('9.9.9.9')) v(i) ORDER BY i"),
        [["9.9.9.9"], ["10.0.0.0/8"], ["10.0.0.1"], ["::1"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT '08-00-2b-01-02-03'::macaddr, '0800.2b01.0203'::macaddr, '08:00:2b:01:02:03'::macaddr::macaddr8"),
        [["08:00:2b:01:02:03", "08:00:2b:01:02:03", "08:00:2b:ff:fe:01:02:03"]]
    );
    expect_code(c.batch_execute("SELECT 'zz'::macaddr"), "22P02");
}

#[test]
fn pretty_deparse_drops_redundant_parentheses() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE pt (a int, b int, c numeric, e bool, CONSTRAINT c1 CHECK (a > 0 AND (b < 10 OR b IS NULL)), CONSTRAINT c2 CHECK ((a + b) * 2 > c), CONSTRAINT c3 CHECK (a - (b - 1) > 0 AND NOT e));
         CREATE VIEW pv AS SELECT a + b * c AS x, (a + b) * c AS y, a - b - c AS m1, a - (b - c) AS m2 FROM pt WHERE a > 1 AND (b > 2 OR c < 3::numeric)",
    )
    .expect("setup");
    assert_eq!(
        rows(
            &mut c,
            "SELECT conname, pg_get_constraintdef(oid, true), pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'pt'::regclass AND contype = 'c' ORDER BY conname"
        ),
        [
            ["c1", "CHECK (a > 0 AND (b < 10 OR b IS NULL))", "CHECK (((a > 0) AND ((b < 10) OR (b IS NULL))))"],
            ["c2", "CHECK (((a + b) * 2)::numeric > c)", "CHECK (((((a + b) * 2))::numeric > c))"],
            ["c3", "CHECK ((a - (b - 1)) > 0 AND NOT e)", "CHECK ((((a - (b - 1)) > 0) AND (NOT e)))"],
        ]
    );
    assert_eq!(
        rows(&mut c, "SELECT pg_get_viewdef('pv'::regclass, true)"),
        [[
            " SELECT a::numeric + b::numeric * c AS x,\n    (a + b)::numeric * c AS y,\n    (a - b)::numeric - c AS m1,\n    a::numeric - (b::numeric - c) AS m2\n   FROM pt\n  WHERE a > 1 AND (b > 2 OR c < 3::numeric);"
        ]]
    );
}
