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

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

#[test]
fn indexdef_prints_opclass_order_and_collation() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (a text, b int);
         CREATE INDEX t_k ON t (a text_pattern_ops DESC NULLS LAST, b NULLS FIRST);
         CREATE INDEX t_c ON t (a COLLATE \"C\");
         CREATE INDEX t_defaults ON t (a ASC NULLS LAST, b DESC NULLS FIRST);
         CREATE INDEX t_desc ON t (b DESC, b DESC NULLS LAST, b ASC NULLS FIRST);
         CREATE INDEX t_expr ON t ((b + 1) DESC, lower(a) COLLATE \"C\" NULLS FIRST, (a || 'x') text_pattern_ops) INCLUDE (a)",
    )
    .expect("indexes");
    assert_eq!(
        rows(
            &mut c,
            "SELECT indexrelid::regclass::text, pg_get_indexdef(indexrelid) FROM pg_index WHERE indrelid = 't'::regclass ORDER BY 1"
        ),
        [
            ["t_c", "CREATE INDEX t_c ON public.t USING btree (a COLLATE \"C\")"],
            ["t_defaults", "CREATE INDEX t_defaults ON public.t USING btree (a, b DESC)"],
            ["t_desc", "CREATE INDEX t_desc ON public.t USING btree (b DESC, b DESC NULLS LAST, b NULLS FIRST)"],
            [
                "t_expr",
                "CREATE INDEX t_expr ON public.t USING btree (((b + 1)) DESC, lower(a) COLLATE \"C\" NULLS FIRST, ((a || 'x'::text)) text_pattern_ops) INCLUDE (a)"
            ],
            ["t_k", "CREATE INDEX t_k ON public.t USING btree (a text_pattern_ops DESC NULLS LAST, b NULLS FIRST)"],
        ]
    );
}

#[test]
fn index_catalog_stores_options_collations_and_classes() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (a text, b int);
         CREATE INDEX t_k ON t (a text_pattern_ops DESC NULLS LAST, b NULLS FIRST);
         CREATE INDEX t_c ON t (a COLLATE \"C\");
         CREATE INDEX t_desc ON t (b DESC, b DESC NULLS LAST, b ASC NULLS FIRST);
         CREATE INDEX t_expr ON t ((b + 1) DESC, lower(a) COLLATE \"C\" NULLS FIRST, (a || 'x') text_pattern_ops) INCLUDE (a);
         CREATE TABLE v (a text COLLATE \"C\", b text COLLATE \"POSIX\", c int);
         CREATE INDEX v_a ON v (a);
         CREATE INDEX v_ac ON v (a COLLATE \"C\");
         CREATE INDEX v_ap ON v (a COLLATE \"POSIX\");
         CREATE INDEX v_b ON v (b);
         CREATE INDEX v_ad ON v (a COLLATE \"default\");
         CREATE INDEX v_h ON v USING hash (a);
         CREATE INDEX v_ex ON v ((a || 'x'), lower(a), (c + 1))",
    )
    .expect("indexes");
    assert_eq!(
        rows(
            &mut c,
            "SELECT indexrelid::regclass::text, indoption::text, indcollation::text, indclass::text FROM pg_index \
             WHERE indrelid = 't'::regclass ORDER BY 1"
        ),
        [
            ["t_c", "0", "950", "3126"],
            ["t_desc", "3 1 2", "0 0 0", "1978 1978 1978"],
            ["t_expr", "3 2 0", "0 950 100", "1978 3126 4217"],
            ["t_k", "1 2", "100 0", "4217 1978"],
        ]
    );
    // A key's own `COLLATE` is printed only when it differs from the collation of its column.
    assert_eq!(
        rows(
            &mut c,
            "SELECT indexrelid::regclass::text, pg_get_indexdef(indexrelid), indcollation::text FROM pg_index \
             WHERE indrelid = 'v'::regclass ORDER BY 1"
        ),
        [
            ["v_a", "CREATE INDEX v_a ON public.v USING btree (a)", "950"],
            ["v_ac", "CREATE INDEX v_ac ON public.v USING btree (a)", "950"],
            ["v_ad", "CREATE INDEX v_ad ON public.v USING btree (a COLLATE \"default\")", "100"],
            ["v_ap", "CREATE INDEX v_ap ON public.v USING btree (a COLLATE \"POSIX\")", "951"],
            ["v_b", "CREATE INDEX v_b ON public.v USING btree (b)", "951"],
            ["v_ex", "CREATE INDEX v_ex ON public.v USING btree (((a || 'x'::text)), lower(a), ((c + 1)))", "950 950 0"],
            ["v_h", "CREATE INDEX v_h ON public.v USING hash (a)", "950"],
        ]
    );
}

#[test]
fn indexdef_per_column_and_pretty_forms() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (a text, b int);
         CREATE INDEX t_k ON t (a text_pattern_ops DESC NULLS LAST, b NULLS FIRST);
         CREATE INDEX t_expr ON t ((b + 1) DESC, lower(a) COLLATE \"C\" NULLS FIRST) INCLUDE (a);
         CREATE INDEX t_part ON t (b, (b * 2), lower(a)) WHERE b > 0 AND b < 5",
    )
    .expect("indexes");
    // One key alone: no collation, operator class or order; an INCLUDE column counts after the keys.
    assert_eq!(
        rows(
            &mut c,
            "SELECT pg_get_indexdef('t_k'::regclass, 1, false), pg_get_indexdef('t_k'::regclass, 2, true), \
                    pg_get_indexdef('t_expr'::regclass, 1, false), pg_get_indexdef('t_expr'::regclass, 1, true), \
                    pg_get_indexdef('t_expr'::regclass, 2, false), pg_get_indexdef('t_expr'::regclass, 3, false), \
                    pg_get_indexdef('t_expr'::regclass, 4, false)"
        ),
        [["a", "b", "((b + 1))", "(b + 1)", "lower(a)", "a", ""]]
    );
    assert_eq!(
        rows(&mut c, "SELECT pg_get_indexdef('t_part'::regclass, 0, false), pg_get_indexdef('t_part'::regclass, 0, true)"),
        [[
            "CREATE INDEX t_part ON public.t USING btree (b, ((b * 2)), lower(a)) WHERE ((b > 0) AND (b < 5))",
            "CREATE INDEX t_part ON t USING btree (b, (b * 2), lower(a)) WHERE b > 0 AND b < 5"
        ]]
    );
}

#[test]
fn sort_options_and_collations_are_checked_like_postgres() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (a text, b int)").expect("setup");
    let cases = [
        ("CREATE INDEX x ON t USING hash (b DESC)", "0A000", "access method \"hash\" does not support ASC/DESC options"),
        (
            "CREATE INDEX x ON t USING gist (a NULLS FIRST)",
            "42704",
            "data type text has no default operator class for access method \"gist\"",
        ),
        ("CREATE INDEX x ON t (b COLLATE \"C\")", "42804", "collations are not supported by type integer"),
        ("CREATE INDEX x ON t (a COLLATE \"nosuch\")", "42704", "collation \"nosuch\" for encoding \"UTF8\" does not exist"),
    ];
    for (sql, code, msg) in cases {
        let e = expect_code(c.batch_execute(sql), code);
        assert_eq!(message(&e), msg, "{sql}");
    }
}

#[test]
fn default_index_names_list_include_columns_and_number_repeats() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE k (a int, b int);
         CREATE INDEX ON k (a, a);
         CREATE INDEX ON k (a, b, a, a);
         CREATE INDEX ON k (a) INCLUDE (a);
         CREATE INDEX ON k (b) INCLUDE (a)",
    )
    .expect("indexes");
    assert_eq!(
        rows(&mut c, "SELECT indexrelid::regclass::text FROM pg_index WHERE indrelid = 'k'::regclass ORDER BY 1"),
        [["k_a_a1_idx"], ["k_a_a1_idx1"], ["k_a_b_a1_a2_idx"], ["k_b_a_idx"]]
    );
}
