use crate::support::setup;

/// string||string and string||int both work; the latter follows PG's implicit cast to text.
#[test]
fn string_concat_basic_and_implicit_cast() {
    let (_db, mut c) = setup();
    for (sql, want) in
        [("SELECT 'hello, ' || 'world'", "hello, world"), ("SELECT 'count: ' || 42", "count: 42"), ("SELECT 'a' || 'b' || 'c'", "abc")]
    {
        let got: String = c.query_one(sql, &[]).expect(sql).get(0);
        assert_eq!(got, want, "{sql}");
    }
}

/// NULL on either side produces NULL.
#[test]
fn string_concat_null_propagates() {
    let (_db, mut c) = setup();
    let got: Option<String> = c.query_one("SELECT 'x' || NULL", &[]).expect("select").get(0);
    assert_eq!(got, None);
}

#[test]
fn string_functions_lower_upper_length() {
    let (_db, mut c) = setup();
    for (sql, want) in [("SELECT lower('Hello WORLD')", "hello world"), ("SELECT upper('Hello WORLD')", "HELLO WORLD")] {
        let got: String = c.query_one(sql, &[]).expect(sql).get(0);
        assert_eq!(got, want, "{sql}");
    }
    // length() returns int4 and counts code points (not bytes).
    let n: i32 = c.query_one("SELECT length('héllo')", &[]).expect("length").get(0);
    assert_eq!(n, 5);
}

/// lower/upper/length on NULL -> NULL.
#[test]
fn string_functions_null_is_null() {
    let (_db, mut c) = setup();
    for sql in ["SELECT lower(NULL)", "SELECT upper(NULL)"] {
        let got: Option<String> = c.query_one(sql, &[]).expect(sql).get(0);
        assert_eq!(got, None, "{sql}");
    }
    let n: Option<i32> = c.query_one("SELECT length(NULL)", &[]).expect("length").get(0);
    assert_eq!(n, None);
}

#[test]
fn string_concat_on_columns() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE people (id int PRIMARY KEY, first text NOT NULL, last text NOT NULL)").expect("create");
    c.batch_execute("INSERT INTO people (id, first, last) VALUES (1, 'Ada', 'Lovelace')").expect("insert");
    let got: String = c.query_one("SELECT first || ' ' || last FROM people WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(got, "Ada Lovelace");
}
