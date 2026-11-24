use crate::support::{expect_code, setup};

#[test]
fn bytea_round_trip() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE blobs (id int PRIMARY KEY, body bytea NOT NULL)").expect("create");
    let want: Vec<u8> = vec![0, 1, 2, 0xfe, 0xff, 0x00, b'h', b'i'];
    c.execute("INSERT INTO blobs (id, body) VALUES ($1, $2)", &[&1i32, &want]).expect("insert");
    let got: Vec<u8> = c.query_one("SELECT body FROM blobs WHERE id = $1", &[&1i32]).expect("readback").get(0);
    assert_eq!(got, want);
}

#[test]
fn bytea_unique_constraint() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE keyed (k bytea PRIMARY KEY)").expect("create");
    let first: Vec<u8> = vec![1, 2, 3];
    c.execute("INSERT INTO keyed (k) VALUES ($1)", &[&first]).expect("first insert");
    // Same bytes: must collide.
    expect_code(c.execute("INSERT INTO keyed (k) VALUES ($1)", &[&first]), "23505");
    // Different bytes: fine.
    c.execute("INSERT INTO keyed (k) VALUES ($1)", &[&vec![4u8, 5]]).expect("distinct insert");
}
