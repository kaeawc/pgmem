use crate::support::{expect_code, message, setup};

const RECT_AREA: &str = "CREATE TABLE rect (
    id     int PRIMARY KEY,
    width  int NOT NULL,
    height int NOT NULL,
    area   int GENERATED ALWAYS AS (width * height) STORED
)";

const RECT_SQ: &str = "CREATE TABLE rect (
    id    int PRIMARY KEY,
    side  int NOT NULL,
    sq    int GENERATED ALWAYS AS (side * side) STORED
)";

#[test]
fn generated_computed_on_insert() {
    let (_db, mut c) = setup();
    c.batch_execute(RECT_AREA).expect("create");
    c.execute("INSERT INTO rect (id, width, height) VALUES (1, 4, 5)", &[]).expect("insert");
    let area: i32 = c.query_one("SELECT area FROM rect WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(area, 20);
}

#[test]
fn generated_recomputed_on_update() {
    let (_db, mut c) = setup();
    c.batch_execute(RECT_AREA).expect("create");
    c.execute("INSERT INTO rect (id, width, height) VALUES (1, 2, 3)", &[]).expect("insert");
    c.execute("UPDATE rect SET width = 7 WHERE id = 1", &[]).expect("update");
    let area: i32 = c.query_one("SELECT area FROM rect WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(area, 21);
}

#[test]
fn generated_rejects_explicit_insert() {
    let (_db, mut c) = setup();
    c.batch_execute(RECT_SQ).expect("create");
    let e = expect_code(c.execute("INSERT INTO rect (id, side, sq) VALUES (1, 4, 99)", &[]), "428C9");
    assert_eq!(message(&e), r#"cannot insert a non-DEFAULT value into column "sq""#);
}

#[test]
fn generated_rejects_explicit_update() {
    let (_db, mut c) = setup();
    c.batch_execute(RECT_SQ).expect("create");
    c.execute("INSERT INTO rect (id, side) VALUES (1, 4)", &[]).expect("insert");
    let e = expect_code(c.execute("UPDATE rect SET sq = 100 WHERE id = 1", &[]), "428C9");
    assert_eq!(message(&e), r#"column "sq" can only be updated to DEFAULT"#);
}

/// Inserting DEFAULT for a generated column (as migration tools may emit) is accepted.
#[test]
fn generated_accepts_default_marker() {
    let (_db, mut c) = setup();
    c.batch_execute(RECT_SQ).expect("create");
    c.execute("INSERT INTO rect (id, side, sq) VALUES (1, 5, DEFAULT)", &[]).expect("insert");
    let sq: i32 = c.query_one("SELECT sq FROM rect WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(sq, 25);
}

#[test]
fn generated_text_concat() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE people (
           id        int PRIMARY KEY,
           first     text NOT NULL,
           last      text NOT NULL,
           full_name text GENERATED ALWAYS AS (first || ' ' || last) STORED
         )",
    )
    .expect("create");
    c.execute("INSERT INTO people (id, first, last) VALUES (1, 'Ada', 'Lovelace')", &[]).expect("insert");
    let name: String = c.query_one("SELECT full_name FROM people WHERE id = 1", &[]).expect("select").get(0);
    assert_eq!(name, "Ada Lovelace");
}
