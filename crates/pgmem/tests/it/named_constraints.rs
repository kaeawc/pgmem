use crate::support::{expect_code, message, setup, texts};

#[test]
fn named_constraint_column_level_pk() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int CONSTRAINT t_pk PRIMARY KEY)").expect("create");
    c.execute("INSERT INTO t (id) VALUES (1)", &[]).expect("insert");
    let e = expect_code(c.execute("INSERT INTO t (id) VALUES (1)", &[]), "23505");
    assert_eq!(message(&e), r#"duplicate key value violates unique constraint "t_pk""#);
}

#[test]
fn named_constraint_column_level_check() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, age int CONSTRAINT age_nonneg CHECK (age >= 0))").expect("create");
    c.execute("INSERT INTO t (id, age) VALUES (1, 5)", &[]).expect("insert");
    let e = expect_code(c.execute("INSERT INTO t (id, age) VALUES (2, -1)", &[]), "23514");
    assert_eq!(message(&e), r#"new row for relation "t" violates check constraint "age_nonneg""#);
}

#[test]
fn named_constraint_table_level_check() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (
           id int PRIMARY KEY,
           lo int NOT NULL,
           hi int NOT NULL,
           CONSTRAINT lo_le_hi CHECK (lo <= hi)
         )",
    )
    .expect("create");
    c.execute("INSERT INTO t (id, lo, hi) VALUES (1, 0, 10)", &[]).expect("insert");
    let e = expect_code(c.execute("INSERT INTO t (id, lo, hi) VALUES (2, 10, 0)", &[]), "23514");
    assert_eq!(message(&e), r#"new row for relation "t" violates check constraint "lo_le_hi""#);
}

#[test]
fn named_constraint_table_level_check_no_name() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, qty int NOT NULL, CHECK (qty > 0))").expect("create");
    let e = expect_code(c.execute("INSERT INTO t (id, qty) VALUES (1, 0)", &[]), "23514");
    assert_eq!(message(&e), r#"new row for relation "t" violates check constraint "t_qty_check""#);
}

/// A named composite PRIMARY KEY (CONSTRAINT name + PRIMARY KEY (col1, col2)) is accepted and enforced.
#[test]
fn named_constraint_table_level_pk_keeps_unique() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a int NOT NULL, b int NOT NULL, CONSTRAINT t_pk PRIMARY KEY (a, b))").expect("create");
    c.execute("INSERT INTO t (a, b) VALUES (1, 2), (1, 3)", &[]).expect("insert");
    let e = expect_code(c.execute("INSERT INTO t (a, b) VALUES (1, 2)", &[]), "23505");
    assert_eq!(message(&e), r#"duplicate key value violates unique constraint "t_pk""#);
}

/// The constraint name is exposed in the error message — sqlc tests often grep for it.
#[test]
fn named_constraint_name_round_trips_in_error() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, qty int NOT NULL, CONSTRAINT qty_positive CHECK (qty > 0))").expect("create");
    let e = expect_code(c.execute("INSERT INTO t (id, qty) VALUES (1, 0)", &[]), "23514");
    assert!(message(&e).contains("qty_positive"), "error {:?} does not mention constraint name", message(&e));
    assert_eq!(e.as_db_error().and_then(|d| d.constraint()), Some("qty_positive"));
}

#[test]
fn key_constraints_print_include() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE u (a int, b int, UNIQUE (a) INCLUDE (b));
         CREATE TABLE p (a int, b int, c int, CONSTRAINT p_pk PRIMARY KEY (a, b) INCLUDE (c));
         CREATE TABLE n (a int, b int, c int, CONSTRAINT n_uq UNIQUE NULLS NOT DISTINCT (a) INCLUDE (b, c) DEFERRABLE);
         CREATE TABLE x (a int, r int4range, b int, CONSTRAINT x_ex EXCLUDE USING gist (r WITH &&) INCLUDE (a, b) WHERE (a > 0));
         CREATE TABLE z (a int, b int, UNIQUE (a), UNIQUE (a) INCLUDE (b), UNIQUE (a));
         ALTER TABLE z ADD CONSTRAINT z_late UNIQUE (b) INCLUDE (a)",
    )
    .expect("tables");
    let defs = texts(
        &mut c,
        "SELECT conname, pg_get_constraintdef(oid), conkey::text FROM pg_constraint \
         WHERE conrelid IN ('u'::regclass, 'p'::regclass, 'n'::regclass, 'x'::regclass, 'z'::regclass) AND contype <> 'n' ORDER BY 1",
    );
    let got: Vec<Vec<String>> = defs.into_iter().map(|r| r.into_iter().map(Option::unwrap_or_default).collect()).collect();
    assert_eq!(
        got,
        [
            ["n_uq", "UNIQUE NULLS NOT DISTINCT (a) INCLUDE (b, c) DEFERRABLE", "{1}"],
            ["p_pk", "PRIMARY KEY (a, b) INCLUDE (c)", "{1,2}"],
            ["u_a_b_key", "UNIQUE (a) INCLUDE (b)", "{1}"],
            ["x_ex", "EXCLUDE USING gist (r WITH &&) INCLUDE (a, b) WHERE ((a > 0))", "{2}"],
            ["z_a_b_key", "UNIQUE (a) INCLUDE (b)", "{1}"],
            ["z_a_key", "UNIQUE (a)", "{1}"],
            ["z_late", "UNIQUE (b) INCLUDE (a)", "{2}"],
        ]
    );
    // The constraint's index covers the INCLUDE columns too.
    assert_eq!(
        texts(
            &mut c,
            "SELECT pg_get_indexdef('u_a_b_key'::regclass), indnatts, indnkeyatts FROM pg_index WHERE indexrelid = 'u_a_b_key'::regclass"
        ),
        [[Some("CREATE UNIQUE INDEX u_a_b_key ON public.u USING btree (a) INCLUDE (b)".into()), Some("2".into()), Some("1".into())]]
    );
}

#[test]
fn key_constraint_include_errors_match_postgres() {
    let (_db, mut c) = setup();
    let e = expect_code(c.batch_execute("CREATE TABLE bad (a int, UNIQUE (a) INCLUDE (nope))"), "42703");
    assert_eq!(message(&e), "column \"nope\" named in key does not exist");
    let e = expect_code(c.batch_execute("CREATE TABLE bad (a int, b int, EXCLUDE USING hash (a WITH =) INCLUDE (b))"), "0A000");
    assert_eq!(message(&e), "access method \"hash\" does not support included columns");
}

/// The sorted first column of a query's rows.
fn sorted_names(c: &mut postgres::Client, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = texts(c, sql).into_iter().map(|mut r| r.remove(0).unwrap_or_default()).collect();
    v.sort();
    v
}

/// Implicit names over 63 bytes are shortened like Postgres' makeObjectName: the label is
/// kept, the table part is cut, and a collision number joins the label (`_n_idx1`).
#[test]
fn long_implicit_names_keep_their_label() {
    let (_db, mut c) = setup();
    let t = "abcdefghij".repeat(6);
    c.batch_execute(&format!(
        "CREATE TABLE {t} (id serial PRIMARY KEY, v int UNIQUE CHECK (v > 0), w int GENERATED ALWAYS AS IDENTITY,
                           n int NOT NULL, r int REFERENCES {t});
         CREATE INDEX ON {t} (n); CREATE INDEX ON {t} (n)"
    ))
    .expect("create");
    let p = |n: usize, rest: &str| format!("{}{rest}", &t[..n]);
    assert_eq!(
        sorted_names(&mut c, &format!("SELECT relname FROM pg_class WHERE relnamespace = 'public'::regnamespace AND relname <> '{t}'")),
        [p(56, "_id_seq"), p(56, "_n_idx1"), p(57, "_n_idx"), p(57, "_v_key"), p(57, "_w_seq"), p(58, "_pkey")]
    );
    assert_eq!(
        sorted_names(&mut c, &format!("SELECT conname FROM pg_constraint WHERE conrelid = '{t}'::regclass")),
        [
            p(51, "_id_not_null"),
            p(52, "_n_not_null"),
            p(52, "_w_not_null"),
            p(55, "_v_check"),
            p(56, "_r_fkey"),
            p(57, "_v_key"),
            p(58, "_pkey")
        ]
    );
    // Both parts long: the longer one gives way first, so they end up about even.
    let (tt, cc) = ("t".repeat(40), "c".repeat(40));
    c.batch_execute(&format!("CREATE TABLE {tt} ({cc} serial)")).expect("create");
    assert_eq!(
        sorted_names(&mut c, &format!("SELECT pg_get_serial_sequence('{tt}', '{cc}')")),
        [format!("public.{}_{}_seq", &tt[..29], &cc[..29])]
    );
    assert_eq!(
        sorted_names(&mut c, &format!("SELECT conname FROM pg_constraint WHERE conrelid = '{tt}'::regclass")),
        [format!("{}_{}_not_null", &tt[..27], &cc[..26])]
    );
}

/// Names are cut by bytes but never inside a character.
#[test]
fn long_implicit_names_cut_on_character_boundaries() {
    let (_db, mut c) = setup();
    let e = |n: usize| "\u{e9}".repeat(n);
    let t = format!("x{}", e(30));
    c.batch_execute(&format!("CREATE TABLE \"{t}\" (id serial, \"{}\" int UNIQUE)", e(5))).expect("create");
    assert_eq!(
        sorted_names(&mut c, &format!("SELECT relname FROM pg_class WHERE relnamespace = 'public'::regnamespace AND relname <> '{t}'")),
        // Sorted by bytes: '_' sorts before 'é'.
        [format!("x{}_{}_key", e(23), e(5)), format!("x{}_id_seq", e(27))]
    );
    assert_eq!(
        sorted_names(&mut c, &format!("SELECT conname FROM pg_constraint WHERE conrelid = '\"{t}\"'::regclass")),
        [format!("x{}_{}_key", e(23), e(5)), format!("x{}_id_not_null", e(25))]
    );
}
