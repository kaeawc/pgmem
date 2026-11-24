use crate::support::setup;

const SETUP: &str = "CREATE TABLE items (id int PRIMARY KEY, label text);
    INSERT INTO items (id, label) VALUES (1, 'a'), (2, 'b'), (3, 'c')";

/// Delete one row by id; check storage and the CommandComplete row count.
#[test]
fn delete_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    let n = c.execute("DELETE FROM items WHERE id = $1", &[&2i32]).expect("delete");
    assert_eq!(n, 1);
    let mut got: Vec<i32> = c.query("SELECT id FROM items", &[]).expect("select").iter().map(|r| r.get(0)).collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 3]);
}

/// DELETE ... RETURNING surfaces the deleted rows the way INSERT ... RETURNING does.
#[test]
fn delete_returning_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    let rows = c.query("DELETE FROM items WHERE id >= $1 RETURNING id, label", &[&2i32]).expect("delete");
    let mut got: Vec<(i32, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    got.sort();
    assert_eq!(got, vec![(2, "b".to_owned()), (3, "c".to_owned())]);
}
