use crate::support::{expect_code, setup};

/// A NOT NULL failure surfaces on the wire as SQLSTATE 23502, which sqlc-generated code inspects.
#[test]
fn not_null_violation_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE widgets (id int NOT NULL, label text NOT NULL)").expect("create");
    expect_code(c.execute("INSERT INTO widgets (id, label) VALUES ($1, $2)", &[&1i32, &None::<&str>]), "23502");
    let row = c.query_opt("SELECT id FROM widgets LIMIT 1", &[]).expect("select");
    assert!(row.is_none(), "table should still be empty");
}
