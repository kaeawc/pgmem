//! Index access methods and operator classes: `USING hash|gin|gist|brin|spgist` is stored and
//! shown by `pg_get_indexdef` / `pg_class.relam` / `pg_index.indclass`, and PostgreSQL's
//! CREATE INDEX errors apply.

use crate::support::{expect_code, message, setup, text_value, texts};

const SETUP: &str = "CREATE TABLE t (a int, b int, c text, j jsonb, r int4range, arr int[]);";

#[test]
fn access_method_is_stored_and_printed() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    c.batch_execute(
        "CREATE INDEX i_hash ON t USING hash (a);
         CREATE INDEX i_gin ON t USING gin (j);
         CREATE INDEX i_gist ON t USING gist (r);
         CREATE INDEX i_brin ON t USING brin (a, c);
         CREATE INDEX i_spgist ON t USING spgist (c);
         CREATE INDEX i_btree ON t USING btree (a);
         CREATE INDEX i_default ON t (b)",
    )
    .expect("indexes");
    let rows = texts(
        &mut c,
        "SELECT c.relname, c.relam, a.amname, pg_get_indexdef(c.oid) FROM pg_class c JOIN pg_am a ON a.oid = c.relam \
         WHERE c.relkind = 'i' AND c.relnamespace = 'public'::regnamespace ORDER BY 1",
    );
    let got: Vec<(String, String, String)> =
        rows.iter().map(|r| (r[0].clone().expect("value"), r[1].clone().expect("value"), r[2].clone().expect("value"))).collect();
    let want = [
        ("i_brin", "3580", "brin"),
        ("i_btree", "403", "btree"),
        ("i_default", "403", "btree"),
        ("i_gin", "2742", "gin"),
        ("i_gist", "783", "gist"),
        ("i_hash", "405", "hash"),
        ("i_spgist", "4000", "spgist"),
    ];
    assert_eq!(got, want.map(|(a, b, c)| (a.into(), b.into(), c.into())));
    let defs: Vec<String> = rows.iter().map(|r| r[3].clone().expect("value").replace("public.", "")).collect();
    assert_eq!(
        defs,
        [
            "CREATE INDEX i_brin ON t USING brin (a, c)",
            "CREATE INDEX i_btree ON t USING btree (a)",
            "CREATE INDEX i_default ON t USING btree (b)",
            "CREATE INDEX i_gin ON t USING gin (j)",
            "CREATE INDEX i_gist ON t USING gist (r)",
            "CREATE INDEX i_hash ON t USING hash (a)",
            "CREATE INDEX i_spgist ON t USING spgist (c)",
        ]
    );
    assert_eq!(
        text_value(&mut c, "SELECT indexdef FROM pg_indexes WHERE indexname = 'i_hash'").map(|s| s.replace("public.", "")),
        Some("CREATE INDEX i_hash ON t USING hash (a)".into())
    );
    assert_eq!(
        text_value(&mut c, "SELECT count(*) FROM pg_am WHERE amname IN ('btree','hash','gist','gin','spgist','brin')"),
        Some("6".into())
    );
}

#[test]
fn default_and_explicit_operator_classes() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    c.batch_execute(
        "CREATE INDEX i1 ON t USING btree (a);
         CREATE INDEX i2 ON t USING hash (c);
         CREATE INDEX i3 ON t USING gin (j);
         CREATE INDEX i4 ON t USING gin (j jsonb_path_ops);
         CREATE INDEX i5 ON t USING btree (c text_pattern_ops);
         CREATE INDEX i6 ON t USING gist (r, r);
         CREATE INDEX i7 ON t USING gin (arr);
         CREATE INDEX i8 ON t USING btree (c text_ops);
         CREATE INDEX i9 ON t USING btree (c varchar_ops);
         CREATE INDEX i10 ON t USING brin (a, c)",
    )
    .expect("indexes");
    let rows = texts(
        &mut c,
        "SELECT c.relname, array_to_string(i.indclass::oid[], ' '), pg_get_indexdef(c.oid) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname LIKE 'i%' ORDER BY length(c.relname), c.relname",
    );
    let got: Vec<(String, String, String)> = rows
        .iter()
        .map(|r| (r[0].clone().expect("value"), r[1].clone().expect("value"), r[2].clone().expect("value").replace("public.", "")))
        .collect();
    let want = [
        ("i1", "1978", "CREATE INDEX i1 ON t USING btree (a)"),
        ("i2", "10037", "CREATE INDEX i2 ON t USING hash (c)"),
        ("i3", "10090", "CREATE INDEX i3 ON t USING gin (j)"),
        ("i4", "10091", "CREATE INDEX i4 ON t USING gin (j jsonb_path_ops)"),
        ("i5", "4217", "CREATE INDEX i5 ON t USING btree (c text_pattern_ops)"),
        ("i6", "10078 10078", "CREATE INDEX i6 ON t USING gist (r, r)"),
        ("i7", "10064", "CREATE INDEX i7 ON t USING gin (arr)"),
        ("i8", "3126", "CREATE INDEX i8 ON t USING btree (c)"),
        ("i9", "10044", "CREATE INDEX i9 ON t USING btree (c varchar_ops)"),
        ("i10", "10104 10107", "CREATE INDEX i10 ON t USING brin (a, c)"),
    ];
    assert_eq!(got, want.map(|(a, b, c)| (a.into(), b.into(), c.into())));
}

#[test]
fn constraint_indexes_have_default_opclasses() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE k (id int PRIMARY KEY, email text UNIQUE)").expect("setup");
    assert_eq!(
        texts(&mut c, "SELECT array_to_string(indclass::oid[], ' ') FROM pg_index WHERE indrelid = 'k'::regclass ORDER BY 1"),
        [[Some("1978".into())], [Some("3126".into())]]
    );
}

#[test]
fn create_index_errors_match_postgres() {
    let (_db, mut c) = setup();
    c.batch_execute(SETUP).expect("setup");
    let cases = [
        ("CREATE INDEX x ON t USING foo (a)", "42704", "access method \"foo\" does not exist"),
        ("CREATE UNIQUE INDEX x ON t USING hash (a)", "0A000", "access method \"hash\" does not support unique indexes"),
        ("CREATE UNIQUE INDEX x ON t USING gin (j)", "0A000", "access method \"gin\" does not support unique indexes"),
        ("CREATE INDEX x ON t USING hash (a, b)", "0A000", "access method \"hash\" does not support multicolumn indexes"),
        ("CREATE INDEX x ON t USING spgist (c, c)", "0A000", "access method \"spgist\" does not support multicolumn indexes"),
        ("CREATE INDEX x ON t USING hash (a) INCLUDE (b)", "0A000", "access method \"hash\" does not support included columns"),
        ("CREATE INDEX x ON t USING gin (j) INCLUDE (a)", "0A000", "access method \"gin\" does not support included columns"),
        ("CREATE INDEX x ON t USING brin (a) INCLUDE (b)", "0A000", "access method \"brin\" does not support included columns"),
        ("CREATE INDEX x ON t USING gin (a)", "42704", "data type integer has no default operator class for access method \"gin\""),
        ("CREATE INDEX x ON t USING gist (c)", "42704", "data type text has no default operator class for access method \"gist\""),
        ("CREATE INDEX x ON t USING brin (j)", "42704", "data type jsonb has no default operator class for access method \"brin\""),
        (
            "CREATE INDEX x ON t USING btree (a nosuch_ops)",
            "42704",
            "operator class \"nosuch_ops\" does not exist for access method \"btree\"",
        ),
        (
            "CREATE INDEX x ON t USING btree (a text_pattern_ops)",
            "42804",
            "operator class \"text_pattern_ops\" does not accept data type integer",
        ),
    ];
    for (sql, code, msg) in cases {
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    }
    let e = expect_code(c.batch_execute("CREATE INDEX x ON t USING gin (a)"), "42704");
    assert_eq!(
        e.as_db_error().and_then(|d| d.hint()),
        Some("You must specify an operator class for the index or define a default operator class for the data type.")
    );
    // Allowed: hash NULLS NOT DISTINCT, INCLUDE on btree and spgist.
    c.batch_execute(
        "CREATE INDEX ok1 ON t USING hash (a) NULLS NOT DISTINCT;
         CREATE INDEX ok2 ON t USING spgist (c) INCLUDE (a);
         CREATE INDEX ok3 ON t USING btree (a) INCLUDE (b)",
    )
    .expect("allowed forms");
}

#[test]
fn like_and_partitions_carry_the_method() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE src (a int, j jsonb);
         CREATE INDEX src_h ON src USING hash (a);
         CREATE INDEX src_g ON src USING gin (j jsonb_path_ops);
         CREATE TABLE cp (LIKE src INCLUDING INDEXES);
         CREATE TABLE pt (a int, j jsonb) PARTITION BY RANGE (a);
         CREATE INDEX pt_h ON pt USING hash (a);
         CREATE TABLE pt1 PARTITION OF pt FOR VALUES FROM (0) TO (10)",
    )
    .expect("setup");
    let defs = |c: &mut postgres::Client, t: &str| -> Vec<String> {
        texts(c, &format!("SELECT pg_get_indexdef(indexrelid) FROM pg_index WHERE indrelid = '{t}'::regclass ORDER BY 1"))
            .into_iter()
            .map(|r| r[0].clone().expect("value").replace("public.", ""))
            .collect()
    };
    assert_eq!(
        defs(&mut c, "cp"),
        ["CREATE INDEX cp_a_idx ON cp USING hash (a)", "CREATE INDEX cp_j_idx ON cp USING gin (j jsonb_path_ops)"]
    );
    assert_eq!(defs(&mut c, "pt1"), ["CREATE INDEX pt1_a_idx ON pt1 USING hash (a)"]);
    assert_eq!(text_value(&mut c, "SELECT relam::text FROM pg_class WHERE relname = 'pt1_a_idx'"), Some("405".into()));
}

#[test]
fn btree_unique_still_enforced() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE u (a int); CREATE UNIQUE INDEX u_a ON u USING btree (a); INSERT INTO u VALUES (1)").expect("setup");
    expect_code(c.batch_execute("INSERT INTO u VALUES (1)"), "23505");
}
