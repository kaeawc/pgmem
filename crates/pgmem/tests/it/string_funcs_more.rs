use crate::support::setup;

#[test]
fn trim() {
    let (_db, mut c) = setup();
    for (input, want) in [("  spaces  ", "spaces"), ("\t\nmixed\r\n", "\t\nmixed\r\n"), ("clean", "clean"), ("", "")] {
        let got: String = c.query_one("SELECT trim($1)", &[&input]).expect("trim").get(0);
        // Deviation from pgmem-go: trim(e'\t\nmixed\r\n') = "mixed" vs Postgres trimming only spaces by default.
        assert_eq!(got, want, "trim({input:?})");
    }
}

#[test]
fn replace() {
    let (_db, mut c) = setup();
    let got: String = c.query_one("SELECT replace('aaa-bbb-ccc', '-', '_')", &[]).expect("replace").get(0);
    assert_eq!(got, "aaa_bbb_ccc");
    // Removal (replace with empty string).
    let got: String = c.query_one("SELECT replace('hello world', ' ', '')", &[]).expect("replace").get(0);
    assert_eq!(got, "helloworld");
}

#[test]
fn substring_two_and_three_arg() {
    let (_db, mut c) = setup();
    for (sql, want) in [
        ("SELECT substring('hello world', 7)", "world"),
        ("SELECT substring('hello world', 1, 5)", "hello"),
        ("SELECT substring('héllo', 2, 3)", "éll"), // code-point indexing
        // PG clamps: from <= 0 starts at 1 (consuming count); over-long count clamps to end.
        ("SELECT substring('abc', 0, 2)", "a"),
        ("SELECT substring('abc', 2, 100)", "bc"),
    ] {
        let got: String = c.query_one(sql, &[]).expect(sql).get(0);
        assert_eq!(got, want, "{sql}");
    }
}

#[test]
fn strpos() {
    let (_db, mut c) = setup();
    for (sql, want) in [
        ("SELECT strpos('hello world', 'world')", 7),
        ("SELECT strpos('hello world', 'xyz')", 0),
        ("SELECT strpos('hello world', '')", 1),      // empty needle -> 1
        ("SELECT strpos('héllo world', 'world')", 7), // code-point indexing
    ] {
        let got: i32 = c.query_one(sql, &[]).expect(sql).get(0);
        assert_eq!(got, want, "{sql}");
    }
}

#[test]
fn string_functions_null_args_return_null() {
    let (_db, mut c) = setup();
    for sql in ["SELECT trim(NULL::text)", "SELECT trim(NULL)", "SELECT replace(NULL, 'a', 'b')", "SELECT substring(NULL, 1, 2)"] {
        let got: Option<String> = c.query_one(sql, &[]).expect(sql).get(0);
        assert_eq!(got, None, "{sql}");
    }
    let n: Option<i32> = c.query_one("SELECT strpos(NULL, 'a')", &[]).expect("strpos").get(0);
    assert_eq!(n, None);
}
