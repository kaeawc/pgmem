use crate::support::{expect_code, message, setup};
use uuid::Uuid;

#[test]
fn uuid_column_gen_and_round_trip() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE rows (id uuid PRIMARY KEY, label text NOT NULL)").expect("create");
    let id: Uuid = c
        .query_one("INSERT INTO rows (id, label) VALUES (gen_random_uuid(), $1) RETURNING id", &[&"alpha"])
        .expect("insert returning")
        .get(0);
    assert!(!id.is_nil(), "returned id was zero");
    assert_eq!(id.get_version_num(), 4, "version nibble");
    let label: String = c.query_one("SELECT label FROM rows WHERE id = $1", &[&id]).expect("readback").get(0);
    assert_eq!(label, "alpha");
}

#[test]
fn uuid_gen_in_select() {
    let (_db, mut c) = setup();
    let u: Uuid = c.query_one("SELECT gen_random_uuid()", &[]).expect("query").get(0);
    assert!(!u.is_nil(), "got zero uuid");
}

#[test]
fn uuid_unknown_function_errors() {
    let (_db, mut c) = setup();
    let e = expect_code(c.execute("SELECT no_such_function()", &[]), "42883");
    assert_eq!(message(&e), "function no_such_function() does not exist");
}
