use crate::support::setup;

/// A SERIAL primary key is auto-filled, returned via RETURNING, and successive inserts get sequential ids.
#[test]
fn serial_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE notes (id SERIAL PRIMARY KEY, body text NOT NULL)").expect("create");
    for (want, body) in [(1, "alpha"), (2, "beta"), (3, "gamma")] {
        let id: i32 = c.query_one("INSERT INTO notes (body) VALUES ($1) RETURNING id", &[&body]).expect("insert").get(0);
        assert_eq!(id, want, "insert {body}");
    }
    let mut got: Vec<i32> = c.query("SELECT id FROM notes", &[]).expect("select").iter().map(|r| r.get(0)).collect();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2, 3]);
}

/// BIGSERIAL returns int8.
#[test]
fn big_serial_over_wire() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE big (id BIGSERIAL PRIMARY KEY, label text NOT NULL)").expect("create");
    let id: i64 = c.query_one("INSERT INTO big (label) VALUES ($1) RETURNING id", &[&"x"]).expect("insert").get(0);
    assert_eq!(id, 1);
}
