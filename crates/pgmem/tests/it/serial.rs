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

/// When the shortened sequence name is taken, the number joins the `seq` label and the table
/// part is cut one byte more (`..._id_seq1`), as in Postgres' ChooseRelationName.
#[test]
fn long_serial_sequence_names_collide_like_postgres() {
    let (_db, mut c) = setup();
    let t60 = "abcdefghij".repeat(6);
    let t57 = &t60[..57];
    c.batch_execute(&format!("CREATE TABLE {t60} (id serial); CREATE TABLE {t57} (id serial)")).expect("create");
    let seq = |c: &mut postgres::Client, t: &str| -> String {
        c.query_one(&format!("SELECT pg_get_serial_sequence('{t}', 'id')"), &[]).expect("serial sequence").get(0)
    };
    assert_eq!(seq(&mut c, &t60), format!("public.{}_id_seq", &t60[..56]));
    assert_eq!(seq(&mut c, t57), format!("public.{}_id_seq1", &t60[..55]));
    // The sequences work under their shortened names.
    let id: i32 = c.query_one(&format!("INSERT INTO {t57} DEFAULT VALUES RETURNING id"), &[]).expect("insert").get(0);
    assert_eq!(id, 1);
}
