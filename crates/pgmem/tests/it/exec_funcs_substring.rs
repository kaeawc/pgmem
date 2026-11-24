//! Port of exec/funcs_substring_test.go as a SQL-level test.

use crate::support::{expect_code, setup};

#[test]
fn substring_negative_length_rejects_at_exec() {
    let (_db, mut c) = setup();
    expect_code(c.query("SELECT substring('abc'::text, 1, -1) AS x", &[]), "22011");
}
