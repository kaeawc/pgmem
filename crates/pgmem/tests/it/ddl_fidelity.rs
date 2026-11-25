//! DDL fidelity: updatable views and WITH CHECK OPTION, CREATE OR REPLACE VIEW rules,
//! materialized views, dependency checks on DROP / ALTER, CREATE TABLE AS / SELECT INTO /
//! LIKE, temporary tables' ON COMMIT, UNLOGGED, constraint and index renames.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

#[test]
fn simple_views_are_updatable() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE orders (id int PRIMARY KEY, amount int, status text);
         INSERT INTO orders VALUES (1, 10, 'open'), (2, 20, 'paid');
         CREATE VIEW paid AS SELECT id, amount, status FROM orders WHERE status = 'paid';
         CREATE VIEW paid_checked AS SELECT * FROM orders WHERE status = 'paid' WITH CHECK OPTION;
         CREATE VIEW totals AS SELECT status, sum(amount) AS total FROM orders GROUP BY status",
    )
    .expect("setup");
    c.batch_execute("INSERT INTO paid VALUES (3, 30, 'open'); UPDATE paid SET amount = amount + 1; DELETE FROM paid WHERE id = 99")
        .expect("DML through a view");
    // The view's WHERE restricts UPDATE / DELETE; the inserted 'open' row is not visible through it.
    assert_eq!(rows(&mut c, "SELECT id, amount FROM orders ORDER BY id"), [["1", "10"], ["2", "21"], ["3", "30"]]);
    let e = expect_code(c.batch_execute("INSERT INTO paid_checked VALUES (4, 40, 'open')"), "44000");
    assert_eq!(message(&e), "new row violates check option for view \"paid_checked\"");
    expect_code(c.batch_execute("UPDATE paid_checked SET status = 'open'"), "44000");
    c.batch_execute("INSERT INTO paid_checked VALUES (4, 40, 'paid')").expect("row satisfying the view");
    let e = expect_code(c.batch_execute("INSERT INTO totals VALUES ('x', 1)"), "55000");
    assert_eq!(message(&e), "cannot insert into view \"totals\"");
    assert_eq!(
        rows(&mut c, "SELECT table_name, is_updatable FROM information_schema.views WHERE table_schema = 'public' ORDER BY 1"),
        [["paid", "YES"], ["paid_checked", "YES"], ["totals", "NO"]]
    );
    // CREATE OR REPLACE may add columns at the end, nothing else.
    let e = expect_code(c.batch_execute("CREATE OR REPLACE VIEW paid AS SELECT id, amount FROM orders"), "42P16");
    assert_eq!(message(&e), "cannot drop columns from view");
    let e = expect_code(c.batch_execute("CREATE OR REPLACE VIEW paid AS SELECT id, amount::text, status FROM orders"), "42P16");
    assert_eq!(message(&e), "cannot change data type of view column \"amount\" from integer to text");
    c.batch_execute("CREATE OR REPLACE VIEW paid AS SELECT id, amount, status, 1 AS extra FROM orders; ALTER VIEW paid RENAME TO paid2")
        .expect("add a column, rename");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM paid2"), [["4"]]);
}

#[test]
fn materialized_views_refresh() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (k text, v int); INSERT INTO t VALUES ('a', 1), ('a', 2), ('b', 5);
         CREATE MATERIALIZED VIEW mv AS SELECT k, sum(v) AS total FROM t GROUP BY k;
         CREATE MATERIALIZED VIEW mv_empty AS SELECT k FROM t WITH NO DATA;
         INSERT INTO t VALUES ('b', 10)",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT k, total FROM mv ORDER BY k"), [["a", "3"], ["b", "5"]]);
    let e = expect_code(c.batch_execute("SELECT * FROM mv_empty"), "55000");
    assert_eq!(message(&e), "materialized view \"mv_empty\" has not been populated");
    expect_code(c.batch_execute("INSERT INTO mv VALUES ('x', 1)"), "42809");
    let e = expect_code(c.batch_execute("REFRESH MATERIALIZED VIEW CONCURRENTLY mv"), "55000");
    assert_eq!(message(&e), "cannot refresh materialized view \"public.mv\" concurrently");
    c.batch_execute("CREATE UNIQUE INDEX mv_k ON mv (k); REFRESH MATERIALIZED VIEW CONCURRENTLY mv; REFRESH MATERIALIZED VIEW mv_empty")
        .expect("refresh");
    assert_eq!(rows(&mut c, "SELECT k, total FROM mv ORDER BY k"), [["a", "3"], ["b", "15"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM mv_empty"), [["4"]]);
    assert_eq!(
        rows(&mut c, "SELECT matviewname, ispopulated FROM pg_matviews WHERE schemaname = 'public' ORDER BY 1"),
        [["mv", "t"], ["mv_empty", "t"]]
    );
    assert_eq!(rows(&mut c, "SELECT relkind FROM pg_class WHERE relname = 'mv'"), [["m"]]);
    let e = expect_code(c.batch_execute("DROP TABLE mv"), "42809");
    assert_eq!(message(&e), "\"mv\" is not a table");
    c.batch_execute("DROP MATERIALIZED VIEW mv").expect("DROP MATERIALIZED VIEW");
}

#[test]
fn dependent_views_block_drop_and_alter() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE base (id int, x int, y int); CREATE VIEW v1 AS SELECT id, x FROM base; CREATE VIEW v2 AS SELECT id FROM v1;
         CREATE SEQUENCE s; CREATE TABLE uses_seq (a int DEFAULT nextval('s'))",
    )
    .expect("setup");
    let e = expect_code(c.batch_execute("DROP TABLE base"), "2BP01");
    assert_eq!(message(&e), "cannot drop table base because other objects depend on it");
    let e = expect_code(c.batch_execute("ALTER TABLE base DROP COLUMN x"), "2BP01");
    assert_eq!(message(&e), "cannot drop column x of table base because other objects depend on it");
    let e = expect_code(c.batch_execute("ALTER TABLE base ALTER COLUMN x TYPE bigint"), "0A000");
    assert_eq!(message(&e), "cannot alter type of a column used by a view or rule");
    // A column no view reads is free to go.
    c.batch_execute("ALTER TABLE base DROP COLUMN y; ALTER TABLE base DROP COLUMN x CASCADE").expect("drop columns");
    expect_code(c.batch_execute("SELECT * FROM v2"), "42P01");
    expect_code(c.batch_execute("DROP SEQUENCE s"), "2BP01");
    c.batch_execute("DROP SEQUENCE s CASCADE").expect("DROP SEQUENCE CASCADE");
    assert_eq!(rows(&mut c, "SELECT column_default IS NULL FROM information_schema.columns WHERE table_name = 'uses_seq'"), [["t"]]);
}

#[test]
fn create_table_as_select_into_and_like() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE src (id serial PRIMARY KEY, name text NOT NULL DEFAULT 'anon' CHECK (name <> ''), n int GENERATED ALWAYS AS (id * 2) STORED);
         CREATE INDEX ON src (lower(name)); COMMENT ON COLUMN src.name IS 'the name';
         INSERT INTO src (name) VALUES ('a'), ('b'), ('c')",
    )
    .expect("setup");
    c.batch_execute(
        "CREATE TABLE ctas AS SELECT id, upper(name) AS uname FROM src WHERE id < 3;
         CREATE TABLE ctas_cols (a, b) AS VALUES (1, 'one'), (2, 'two');
         CREATE TABLE ctas_empty AS SELECT * FROM src WITH NO DATA;
         SELECT id, name INTO selinto FROM src WHERE id = 3",
    )
    .expect("CREATE TABLE AS");
    assert_eq!(rows(&mut c, "SELECT id, uname FROM ctas ORDER BY id"), [["1", "A"], ["2", "B"]]);
    assert_eq!(rows(&mut c, "SELECT a, b FROM ctas_cols ORDER BY a"), [["1", "one"], ["2", "two"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM ctas_empty"), [["0"]]);
    assert_eq!(rows(&mut c, "SELECT id, name FROM selinto"), [["3", "c"]]);
    let e = expect_code(c.batch_execute("CREATE TABLE ctas_bad (p, q, r) AS SELECT 1, 2"), "42601");
    assert_eq!(message(&e), "too many column names were specified");
    expect_code(c.batch_execute("CREATE TABLE ctas AS SELECT 1"), "42P07");
    c.batch_execute("CREATE TABLE IF NOT EXISTS ctas AS SELECT 1").expect("IF NOT EXISTS");

    c.batch_execute("CREATE TABLE plain (LIKE src); CREATE TABLE copy_all (x int, LIKE src INCLUDING ALL, y int)").expect("LIKE");
    // Plain LIKE copies names, types and NOT NULL only.
    assert_eq!(
        rows(
            &mut c,
            "SELECT column_name, is_nullable, column_default IS NULL FROM information_schema.columns WHERE table_name = 'plain' ORDER BY ordinal_position"
        ),
        [["id", "NO", "t"], ["name", "NO", "t"], ["n", "YES", "t"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT column_name FROM information_schema.columns WHERE table_name = 'copy_all' ORDER BY ordinal_position"),
        [["x"], ["id"], ["name"], ["n"], ["y"]]
    );
    c.batch_execute("INSERT INTO copy_all (x) VALUES (1)").expect("defaults copied");
    assert_eq!(rows(&mut c, "SELECT id, name, n FROM copy_all"), [["4", "anon", "8"]]);
    expect_code(c.batch_execute("INSERT INTO copy_all (id, name) VALUES (9, '')"), "23514");
    assert_eq!(
        rows(&mut c, "SELECT indexname FROM pg_indexes WHERE tablename = 'copy_all' ORDER BY 1"),
        [["copy_all_lower_idx"], ["copy_all_pkey"]]
    );
    assert_eq!(rows(&mut c, "SELECT col_description('copy_all'::regclass, 3)"), [["the name"]]);
    let e = expect_code(c.batch_execute("CREATE TABLE dup (name int, LIKE src)"), "42701");
    assert_eq!(message(&e), "column \"name\" specified more than once");
    expect_code(c.batch_execute("CREATE TABLE missing (LIKE nosuch)"), "42P01");
}

#[test]
fn temporary_on_commit_and_unlogged() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TEMP TABLE scratch (a int) ON COMMIT DELETE ROWS; CREATE UNLOGGED TABLE unl (a int)").expect("setup");
    c.batch_execute("BEGIN; INSERT INTO scratch VALUES (1)").expect("insert");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM scratch"), [["1"]]);
    c.batch_execute("COMMIT").expect("COMMIT");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM scratch"), [["0"]]);
    c.batch_execute("BEGIN; CREATE TEMP TABLE gone (a int) ON COMMIT DROP; INSERT INTO gone VALUES (1); COMMIT").expect("ON COMMIT DROP");
    expect_code(c.batch_execute("SELECT * FROM gone"), "42P01");
    let e = expect_code(c.batch_execute("CREATE TABLE notemp (a int) ON COMMIT DROP"), "42P16");
    assert_eq!(message(&e), "ON COMMIT can only be used on temporary tables");
    assert_eq!(
        rows(&mut c, "SELECT relname, relpersistence FROM pg_class WHERE relname IN ('scratch', 'unl') ORDER BY 1"),
        [["scratch", "t"], ["unl", "u"]]
    );
}

#[test]
fn constraint_and_index_renames() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE t (a int PRIMARY KEY, b text CHECK (b <> ''), c int);
         ALTER TABLE t RENAME CONSTRAINT t_b_check TO b_nonempty; ALTER TABLE t RENAME CONSTRAINT t_pkey TO t_pk",
    )
    .expect("renames");
    let e = expect_code(c.batch_execute("INSERT INTO t VALUES (1, '')"), "23514");
    assert_eq!(message(&e), "new row for relation \"t\" violates check constraint \"b_nonempty\"");
    let e = expect_code(c.batch_execute("ALTER TABLE t RENAME CONSTRAINT nosuch TO x"), "42704");
    assert_eq!(message(&e), "constraint \"nosuch\" for table \"t\" does not exist");
    assert_eq!(rows(&mut c, "SELECT indexname FROM pg_indexes WHERE tablename = 't'"), [["t_pk"]]);
    // Type changes without an assignment cast need USING.
    let e = expect_code(c.batch_execute("ALTER TABLE t ALTER COLUMN b TYPE int"), "42804");
    assert_eq!(message(&e), "column \"b\" cannot be cast automatically to type integer");
    c.batch_execute(
        "CREATE INDEX t_c_idx ON t (c); ALTER INDEX t_c_idx RENAME TO t_c_ix; ALTER INDEX IF EXISTS nosuch RENAME TO other;
         CREATE UNIQUE INDEX t_c_u ON t (c); ALTER TABLE t ADD CONSTRAINT t_c_unique UNIQUE USING INDEX t_c_u",
    )
    .expect("index renames");
    assert_eq!(rows(&mut c, "SELECT indexname FROM pg_indexes WHERE tablename = 't' ORDER BY 1"), [["t_c_ix"], ["t_c_unique"], ["t_pk"]]);
    assert_eq!(rows(&mut c, "SELECT conname, contype FROM pg_constraint WHERE conname = 't_c_unique'"), [["t_c_unique", "u"]]);
    let e = expect_code(c.batch_execute("ALTER TABLE t ADD UNIQUE USING INDEX t_c_ix"), "42809");
    assert_eq!(message(&e), "\"t_c_ix\" is not a unique index");
    let e = expect_code(c.batch_execute("CREATE INDEX bad ON t USING nosuchmethod (a)"), "42704");
    assert_eq!(message(&e), "access method \"nosuchmethod\" does not exist");
    c.batch_execute("CREATE TABLE nn (a int); CREATE UNIQUE INDEX nn_a ON nn (a) NULLS NOT DISTINCT; INSERT INTO nn VALUES (NULL)")
        .expect("NULLS NOT DISTINCT");
    expect_code(c.batch_execute("INSERT INTO nn VALUES (NULL)"), "23505");
    c.batch_execute("BEGIN").expect("BEGIN");
    let e = expect_code(c.batch_execute("CREATE INDEX CONCURRENTLY t_a_conc ON t (a)"), "25001");
    assert_eq!(message(&e), "CREATE INDEX CONCURRENTLY cannot run inside a transaction block");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
}
