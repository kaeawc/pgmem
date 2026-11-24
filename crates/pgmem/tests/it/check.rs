use crate::support::{expect_code, setup};

/// A CHECK in CREATE TABLE rejects a bad INSERT with SQLSTATE 23514 and passes a good one through.
#[test]
fn check_violation_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE inventory (id int PRIMARY KEY, qty int NOT NULL CHECK (qty > 0))").expect("create");
    c.execute("INSERT INTO inventory (id, qty) VALUES ($1, $2)", &[&1i32, &5i32]).expect("good insert");
    expect_code(c.execute("INSERT INTO inventory (id, qty) VALUES ($1, $2)", &[&2i32, &0i32]), "23514");
    let ids: Vec<i32> = c.query("SELECT id FROM inventory ORDER BY id", &[]).expect("select").iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![1], "only the good row survived");
}
