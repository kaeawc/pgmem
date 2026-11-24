use crate::support::setup;

/// Regression: pgmem-go bound every Bind against the connection's last Parse result, regardless of the
/// statement name. With a client-side statement cache, a Bind referencing a previously prepared SELECT
/// after a more recent UPDATE Parse bound against the wrong plan and silently returned no rows.
#[test]
fn named_statement_cache_bind_resolves_by_name() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, flag bool NOT NULL); INSERT INTO t (id, flag) VALUES (1, false)").expect("setup");

    let select = c.prepare("SELECT flag FROM t WHERE id = $1").expect("prepare select");
    let query_flag = |c: &mut postgres::Client| -> bool { c.query_one(&select, &[&1i32]).expect("query").get(0) };
    assert!(!query_flag(&mut c), "flag should start false");

    // A different named statement, Parse'd after the SELECT.
    let update = c.prepare("UPDATE t SET flag = true WHERE id = $1").expect("prepare update");
    c.execute(&update, &[&1i32]).expect("update");

    assert!(query_flag(&mut c), "expected flag=true after UPDATE");
}
