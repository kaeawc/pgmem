use crate::support::{TestDb, setup};
use postgres::Client;

fn index_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE t (id int PRIMARY KEY, name text NOT NULL); INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b');")
        .expect("setup");
    (db, c)
}

#[test]
fn create_index_plain() {
    let (_db, mut c) = index_setup();
    c.batch_execute("CREATE INDEX t_name_idx ON t (name)").expect("CREATE INDEX");
    let n: i64 = c.query_one("SELECT count(*) FROM t", &[]).expect("count").get(0);
    assert_eq!(n, 2);
}

#[test]
fn create_index_all_options() {
    let (_db, mut c) = index_setup();
    for sql in [
        "CREATE UNIQUE INDEX t_uniq ON t (id)",
        "CREATE INDEX CONCURRENTLY t_conc ON t USING btree (name)",
        "CREATE INDEX IF NOT EXISTS t_partial ON t (id) WHERE id > 0",
        "CREATE INDEX ON t (name, id)", // anonymous index name
    ] {
        c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

#[test]
fn drop_index() {
    let (_db, mut c) = index_setup();
    c.batch_execute("CREATE INDEX t_name_idx ON t (name)").expect("CREATE INDEX");
    c.batch_execute("DROP INDEX t_name_idx").expect("DROP INDEX");
    c.batch_execute("DROP INDEX IF EXISTS missing").expect("DROP INDEX IF EXISTS");
    c.batch_execute("DROP INDEX CONCURRENTLY IF EXISTS missing").expect("DROP INDEX CONCURRENTLY IF EXISTS");
}
