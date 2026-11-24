use crate::support::setup;

fn limit_all_setup(c: &mut postgres::Client) {
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY); INSERT INTO t (id) VALUES (1), (2), (3), (4), (5);").expect("setup");
}

fn ids(c: &mut postgres::Client, sql: &str) -> Vec<i32> {
    c.query(sql, &[]).expect(sql).iter().map(|r| r.get(0)).collect()
}

#[test]
fn limit_all() {
    let (_db, mut c) = setup();
    limit_all_setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM t ORDER BY id LIMIT ALL"), [1, 2, 3, 4, 5]);
}

#[test]
fn offset_with_rows_noise_word() {
    let (_db, mut c) = setup();
    limit_all_setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM t ORDER BY id OFFSET 2 ROWS"), [3, 4, 5]);
}

#[test]
fn offset_with_row_singular_noise_word() {
    let (_db, mut c) = setup();
    limit_all_setup(&mut c);
    assert_eq!(ids(&mut c, "SELECT id FROM t ORDER BY id OFFSET 1 ROW LIMIT 1"), [2]);
}
