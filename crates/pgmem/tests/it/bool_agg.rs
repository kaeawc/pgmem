use crate::support::setup;

fn bool_agg_setup(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE flags (id int PRIMARY KEY, kind text NOT NULL, ok bool);
         INSERT INTO flags (id, kind, ok) VALUES (1, 'a', true), (2, 'a', true), (3, 'b', false), (4, 'b', true), (5, 'c', NULL);",
    )
    .expect("setup");
}

fn bool_value(c: &mut postgres::Client, sql: &str) -> Option<bool> {
    c.query_one(sql, &[]).expect(sql).get(0)
}

#[test]
fn bool_and_all_true() {
    let (_db, mut c) = setup();
    bool_agg_setup(&mut c);
    assert_eq!(bool_value(&mut c, "SELECT bool_and(ok) FROM flags WHERE kind = 'a'"), Some(true));
}

#[test]
fn bool_and_one_false() {
    let (_db, mut c) = setup();
    bool_agg_setup(&mut c);
    assert_eq!(bool_value(&mut c, "SELECT bool_and(ok) FROM flags WHERE kind = 'b'"), Some(false));
}

#[test]
fn bool_or_any_true() {
    let (_db, mut c) = setup();
    bool_agg_setup(&mut c);
    assert_eq!(bool_value(&mut c, "SELECT bool_or(ok) FROM flags WHERE kind = 'b'"), Some(true));
}

#[test]
fn bool_agg_all_nulls_is_null() {
    let (_db, mut c) = setup();
    bool_agg_setup(&mut c);
    assert_eq!(bool_value(&mut c, "SELECT bool_and(ok) FROM flags WHERE kind = 'c'"), None);
}

#[test]
fn every_synonym_for_bool_and() {
    let (_db, mut c) = setup();
    bool_agg_setup(&mut c);
    assert_eq!(bool_value(&mut c, "SELECT every(ok) FROM flags WHERE kind = 'a'"), Some(true));
}
