use crate::support::{expect_code, message, setup};

fn ints(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect()
}

// generate_series over int4 arguments yields int4 (pgx scanned it loosely into int64).

#[test]
fn generate_series_basic() {
    let (_db, mut c) = setup();
    assert_eq!(ints(&mut c, "SELECT * FROM generate_series(1, 5)"), [1, 2, 3, 4, 5]);
}

#[test]
fn generate_series_step() {
    let (_db, mut c) = setup();
    assert_eq!(ints(&mut c, "SELECT * FROM generate_series(0, 10, 2)"), [0, 2, 4, 6, 8, 10]);
}

#[test]
fn generate_series_negative_step() {
    let (_db, mut c) = setup();
    assert_eq!(ints(&mut c, "SELECT * FROM generate_series(5, 1, -1)"), [5, 4, 3, 2, 1]);
}

#[test]
fn generate_series_empty() {
    let (_db, mut c) = setup();
    assert_eq!(ints(&mut c, "SELECT * FROM generate_series(10, 1)"), Vec::<i32>::new());
}

#[test]
fn generate_series_zero_step() {
    let (_db, mut c) = setup();
    let e = expect_code(c.query("SELECT * FROM generate_series(1, 5, 0)", &[]), "22023");
    assert_eq!(message(&e), "step size cannot equal zero");
}

#[test]
fn generate_series_alias() {
    let (_db, mut c) = setup();
    // Deviation from pgmem-go: it kept the column name "generate_series" under a table alias; in Postgres a
    // scalar function's table alias also names its column, so `gs.generate_series` does not exist.
    let e = expect_code(c.query("SELECT gs.generate_series FROM generate_series(1, 3) AS gs", &[]), "42703");
    assert_eq!(message(&e), "column gs.generate_series does not exist");
    assert_eq!(ints(&mut c, "SELECT gs.gs FROM generate_series(1, 3) AS gs"), [1, 2, 3]);
}

#[test]
fn generate_series_joined() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, name text); INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c');")
        .expect("setup");
    // Deviation from pgmem-go: `gs.generate_series` (Go's join column) does not exist in Postgres; the alias names the column.
    let e = expect_code(
        c.query("SELECT t.name FROM t INNER JOIN generate_series(1, 3) gs ON gs.generate_series = t.id ORDER BY t.id", &[]),
        "42703",
    );
    assert_eq!(message(&e), "column gs.generate_series does not exist");
    let rows = c.query("SELECT t.name FROM t INNER JOIN generate_series(1, 3) gs ON gs.gs = t.id ORDER BY t.id", &[]).expect("query");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["a", "b", "c"]);
}

#[test]
fn generate_series_bigint() {
    let (_db, mut c) = setup();
    // A literal beyond int32 range forces int8.
    let rows = c.query("SELECT * FROM generate_series(2147483648, 2147483650)", &[]).expect("query");
    let got: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, [2147483648, 2147483649, 2147483650]);
}
