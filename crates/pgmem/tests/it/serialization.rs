use crate::support::setup;

#[test]
fn serialization_concurrent_writes_one_fails() {
    // Deviation from pgmem-go: pgmem-go detected write conflicts per table, so B's COMMIT failed
    // with 40001 and only A's row survived; real Postgres (READ COMMITTED) commits both inserts of
    // different rows. (A same-row concurrent UPDATE would block on real Postgres, so it isn't tested.)
    let (db, mut a) = setup();
    let mut b = db.connect();
    a.batch_execute("CREATE TABLE items (id int PRIMARY KEY)").expect("CREATE");
    a.batch_execute("BEGIN").expect("A BEGIN");
    b.batch_execute("BEGIN").expect("B BEGIN");
    a.execute("INSERT INTO items (id) VALUES (1)", &[]).expect("A INSERT");
    b.execute("INSERT INTO items (id) VALUES (2)", &[]).expect("B INSERT");
    a.batch_execute("COMMIT").expect("A COMMIT");
    b.batch_execute("COMMIT").expect("B COMMIT");
    let ids: Vec<i32> = a.query("SELECT id FROM items ORDER BY id", &[]).expect("SELECT").iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn serialization_disjoint_writes_no_conflict() {
    let (db, mut a) = setup();
    let mut b = db.connect();
    a.batch_execute("CREATE TABLE a (n int)").expect("CREATE a");
    a.batch_execute("CREATE TABLE b (n int)").expect("CREATE b");
    a.batch_execute("BEGIN").expect("A BEGIN");
    b.batch_execute("BEGIN").expect("B BEGIN");
    a.execute("INSERT INTO a (n) VALUES (1)", &[]).expect("A INSERT");
    b.execute("INSERT INTO b (n) VALUES (2)", &[]).expect("B INSERT");
    a.batch_execute("COMMIT").expect("A COMMIT");
    b.batch_execute("COMMIT").expect("B COMMIT (disjoint tables shouldn't conflict)");
}
