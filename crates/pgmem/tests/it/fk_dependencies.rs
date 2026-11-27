//! Foreign keys depend on the referenced key's unique index and on the referenced columns:
//! DROP CONSTRAINT, DROP INDEX and DROP COLUMN refuse to drop them while a foreign key uses
//! them, and CASCADE drops the foreign keys (#216, #254).

use crate::support::{RawConn, TestDb, expect_code, message, setup, texts};
use postgres::Client;

const HINT: &str = "Use DROP ... CASCADE to drop the dependent objects too.";

/// Checks a 2BP01 dependency error: message, DETAIL and HINT.
fn dependents(r: Result<(), postgres::Error>, msg: &str, detail: &str) {
    let e = expect_code(r, "2BP01");
    assert_eq!(message(&e), msg);
    let db = e.as_db_error().expect("db error");
    assert_eq!(db.detail(), Some(detail));
    assert_eq!(db.hint(), Some(HINT));
}

/// The foreign keys of `table`, by name.
fn fks(c: &mut Client, table: &str) -> Vec<String> {
    texts(c, &format!("SELECT conname::text FROM pg_constraint WHERE conrelid = '{table}'::regclass AND contype = 'f' ORDER BY 1"))
        .into_iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect()
}

/// Runs `sql` on a raw connection; returns its notices (asserting it succeeded).
fn notices(db: &TestDb, c: &mut Client, sql: &str) -> Vec<String> {
    let mut raw = RawConn::connect(db, c);
    let r = raw.query(sql);
    assert!(r.errors.is_empty(), "{sql}: {:?}", r.errors);
    r.notices
}

/// p (primary key id, unique u); c references the primary key, c2 both keys.
fn keys_setup() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (id int PRIMARY KEY, u int UNIQUE);
         CREATE TABLE c (p int REFERENCES p);
         CREATE TABLE c2 (p int REFERENCES p, q int REFERENCES p (u));",
    )
    .expect("setup");
    (db, c)
}

#[test]
fn drop_primary_key_used_by_foreign_keys() {
    let (db, mut c) = keys_setup();
    dependents(
        c.batch_execute("ALTER TABLE p DROP CONSTRAINT p_pkey"),
        "cannot drop constraint p_pkey on table p because other objects depend on it",
        "constraint c_p_fkey on table c depends on index p_pkey\nconstraint c2_p_fkey on table c2 depends on index p_pkey",
    );
    // A unique constraint's foreign key only.
    dependents(
        c.batch_execute("ALTER TABLE p DROP CONSTRAINT p_u_key"),
        "cannot drop constraint p_u_key on table p because other objects depend on it",
        "constraint c2_q_fkey on table c2 depends on index p_u_key",
    );
    // The index belongs to its constraint.
    let e = expect_code(c.batch_execute("DROP INDEX p_pkey"), "2BP01");
    assert_eq!(message(&e), "cannot drop index p_pkey because constraint p_pkey on table p requires it");
    assert_eq!(fks(&mut c, "c2"), ["c2_p_fkey", "c2_q_fkey"]);
    assert_eq!(
        notices(&db, &mut c, "ALTER TABLE p DROP CONSTRAINT p_pkey CASCADE"),
        ["drop cascades to 2 other objects\ndrop cascades to constraint c_p_fkey on table c\n\
          drop cascades to constraint c2_p_fkey on table c2"]
    );
    assert_eq!(fks(&mut c, "c"), Vec::<String>::new());
    assert_eq!(fks(&mut c, "c2"), ["c2_q_fkey"]);
    assert_eq!(
        notices(&db, &mut c, "ALTER TABLE p DROP CONSTRAINT p_u_key CASCADE"),
        ["drop cascades to constraint c2_q_fkey on table c2"]
    );
    assert_eq!(fks(&mut c, "c2"), Vec::<String>::new());
    // The foreign keys are gone: the child accepts any value.
    c.batch_execute("INSERT INTO c VALUES (42); INSERT INTO c2 VALUES (1, 2)").expect("no foreign keys left");
}

#[test]
fn drop_unique_index_used_by_foreign_key() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (id int, k int);
         CREATE UNIQUE INDEX p_id_idx ON p (id);
         CREATE TABLE c (p int REFERENCES p (id));",
    )
    .expect("setup");
    dependents(
        c.batch_execute("DROP INDEX p_id_idx"),
        "cannot drop index p_id_idx because other objects depend on it",
        "constraint c_p_fkey on table c depends on index p_id_idx",
    );
    assert_eq!(notices(&db, &mut c, "DROP INDEX p_id_idx CASCADE"), ["drop cascades to constraint c_p_fkey on table c"]);
    assert_eq!(fks(&mut c, "c"), Vec::<String>::new());
    assert_eq!(texts(&mut c, "SELECT to_regclass('p_id_idx')::text"), vec![vec![None]]);
}

#[test]
fn drop_referenced_column() {
    let (db, mut c) = keys_setup();
    dependents(
        c.batch_execute("ALTER TABLE p DROP COLUMN id"),
        "cannot drop column id of table p because other objects depend on it",
        "constraint c_p_fkey on table c depends on column id of table p\nconstraint c2_p_fkey on table c2 depends on column id of table p",
    );
    dependents(
        c.batch_execute("ALTER TABLE p DROP COLUMN u"),
        "cannot drop column u of table p because other objects depend on it",
        "constraint c2_q_fkey on table c2 depends on column u of table p",
    );
    assert_eq!(
        notices(&db, &mut c, "ALTER TABLE p DROP COLUMN id CASCADE"),
        ["drop cascades to 2 other objects\ndrop cascades to constraint c_p_fkey on table c\n\
          drop cascades to constraint c2_p_fkey on table c2"]
    );
    assert_eq!(fks(&mut c, "c"), Vec::<String>::new());
    assert_eq!(fks(&mut c, "c2"), ["c2_q_fkey"]);
    assert_eq!(
        texts(&mut c, "SELECT attname::text FROM pg_attribute WHERE attrelid = 'p'::regclass AND attnum > 0 AND NOT attisdropped"),
        vec![vec![Some("u".to_owned())]]
    );
}

// Dropping the referencing column drops that table's own foreign key, nothing else.
#[test]
fn drop_referencing_column() {
    let (_db, mut c) = keys_setup();
    c.batch_execute("ALTER TABLE c2 DROP COLUMN p").expect("drop referencing column");
    assert_eq!(fks(&mut c, "c2"), ["c2_q_fkey"]);
    assert_eq!(fks(&mut c, "c"), ["c_p_fkey"]);
}

#[test]
fn self_referencing_foreign_key() {
    let (db, mut c) = setup();
    c.batch_execute("CREATE TABLE s (id int PRIMARY KEY, parent int REFERENCES s)").expect("setup");
    dependents(
        c.batch_execute("ALTER TABLE s DROP CONSTRAINT s_pkey"),
        "cannot drop constraint s_pkey on table s because other objects depend on it",
        "constraint s_parent_fkey on table s depends on index s_pkey",
    );
    dependents(
        c.batch_execute("ALTER TABLE s DROP COLUMN id"),
        "cannot drop column id of table s because other objects depend on it",
        "constraint s_parent_fkey on table s depends on column id of table s",
    );
    c.batch_execute(
        "CREATE TABLE s2 (id int PRIMARY KEY, parent int REFERENCES s2);
         CREATE TABLE s3 (id int PRIMARY KEY, parent int REFERENCES s3);",
    )
    .expect("more tables");
    assert_eq!(
        notices(&db, &mut c, "ALTER TABLE s2 DROP CONSTRAINT s2_pkey CASCADE"),
        ["drop cascades to constraint s2_parent_fkey on table s2"]
    );
    assert_eq!(
        texts(&mut c, "SELECT conname::text FROM pg_constraint WHERE conrelid = 's2'::regclass AND contype IN ('p', 'f')"),
        Vec::<Vec<Option<String>>>::new()
    );
    assert_eq!(notices(&db, &mut c, "ALTER TABLE s3 DROP COLUMN id CASCADE"), ["drop cascades to constraint s3_parent_fkey on table s3"]);
    assert_eq!(fks(&mut c, "s3"), Vec::<String>::new());
    assert_eq!(
        texts(&mut c, "SELECT attname::text FROM pg_attribute WHERE attrelid = 's3'::regclass AND attnum > 0 AND NOT attisdropped"),
        vec![vec![Some("parent".to_owned())]]
    );
    // Dropping the referencing column first frees the key column.
    c.batch_execute("ALTER TABLE s DROP COLUMN parent, DROP COLUMN id").expect("drop both");
    // A key referencing its own column goes with that column.
    c.batch_execute("CREATE TABLE r (id int PRIMARY KEY REFERENCES r); ALTER TABLE r DROP COLUMN id").expect("own column");
}

#[test]
fn multi_column_foreign_key() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE m (a int, b int, PRIMARY KEY (a, b));
         CREATE TABLE mc (x int, y int, FOREIGN KEY (x, y) REFERENCES m (b, a));",
    )
    .expect("setup");
    dependents(
        c.batch_execute("ALTER TABLE m DROP CONSTRAINT m_pkey"),
        "cannot drop constraint m_pkey on table m because other objects depend on it",
        "constraint mc_x_y_fkey on table mc depends on index m_pkey",
    );
    dependents(
        c.batch_execute("ALTER TABLE m DROP COLUMN b"),
        "cannot drop column b of table m because other objects depend on it",
        "constraint mc_x_y_fkey on table mc depends on column b of table m",
    );
    assert_eq!(notices(&db, &mut c, "ALTER TABLE m DROP COLUMN a CASCADE"), ["drop cascades to constraint mc_x_y_fkey on table mc"]);
    assert_eq!(fks(&mut c, "mc"), Vec::<String>::new());
    assert_eq!(texts(&mut c, "SELECT count(*)::text FROM pg_index WHERE indrelid = 'm'::regclass"), vec![vec![Some("0".to_owned())]]);
}

// The dependent sits in another schema: it is named schema-qualified.
#[test]
fn foreign_key_in_other_schema() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE SCHEMA fkdep_other;
         CREATE TABLE p (id int PRIMARY KEY);
         CREATE TABLE fkdep_other.c (r int REFERENCES p);",
    )
    .expect("setup");
    dependents(
        c.batch_execute("ALTER TABLE p DROP CONSTRAINT p_pkey"),
        "cannot drop constraint p_pkey on table p because other objects depend on it",
        "constraint c_r_fkey on table fkdep_other.c depends on index p_pkey",
    );
    assert_eq!(
        notices(&db, &mut c, "ALTER TABLE p DROP COLUMN id CASCADE"),
        ["drop cascades to constraint c_r_fkey on table fkdep_other.c"]
    );
    c.batch_execute("DROP SCHEMA fkdep_other CASCADE").expect("cleanup");
}

#[test]
fn partitioned_primary_key() {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE pp (id int PRIMARY KEY) PARTITION BY RANGE (id);
         CREATE TABLE pp2 PARTITION OF pp FOR VALUES FROM (10) TO (20);
         CREATE TABLE pp1 PARTITION OF pp FOR VALUES FROM (0) TO (10);
         CREATE TABLE ppc (r int REFERENCES pp);",
    )
    .expect("setup");
    // The key's clone on the newest partition index is the one reported.
    dependents(
        c.batch_execute("ALTER TABLE pp DROP CONSTRAINT pp_pkey"),
        "cannot drop constraint pp_pkey on table pp because other objects depend on it",
        "constraint ppc_r_fkey on table ppc depends on index pp1_pkey",
    );
    assert_eq!(
        notices(&db, &mut c, "ALTER TABLE pp DROP CONSTRAINT pp_pkey CASCADE"),
        ["drop cascades to constraint ppc_r_fkey on table ppc"]
    );
    assert_eq!(fks(&mut c, "ppc"), Vec::<String>::new());
    // Without partitions, the key's own index.
    c.batch_execute("CREATE TABLE pq (id int PRIMARY KEY) PARTITION BY RANGE (id); CREATE TABLE pqc (r int REFERENCES pq)").expect("pq");
    dependents(
        c.batch_execute("ALTER TABLE pq DROP CONSTRAINT pq_pkey"),
        "cannot drop constraint pq_pkey on table pq because other objects depend on it",
        "constraint pqc_r_fkey on table pqc depends on index pq_pkey",
    );
}

// A failed drop changes nothing; a dropped foreign key comes back on ROLLBACK.
#[test]
fn rollback_restores_foreign_keys() {
    let (_db, mut c) = keys_setup();
    c.batch_execute("INSERT INTO p VALUES (1, 1)").expect("parent row");
    c.batch_execute("BEGIN; SAVEPOINT s").expect("begin");
    expect_code(c.batch_execute("ALTER TABLE p DROP COLUMN id"), "2BP01");
    c.batch_execute("ROLLBACK TO s; ALTER TABLE p DROP COLUMN id CASCADE; ALTER TABLE p DROP CONSTRAINT p_u_key CASCADE").expect("cascade");
    assert_eq!(fks(&mut c, "c2"), Vec::<String>::new());
    c.batch_execute("ROLLBACK").expect("rollback");
    assert_eq!(fks(&mut c, "c"), ["c_p_fkey"]);
    assert_eq!(fks(&mut c, "c2"), ["c2_p_fkey", "c2_q_fkey"]);
    expect_code(c.batch_execute("INSERT INTO c VALUES (42)"), "23503");
    c.batch_execute("INSERT INTO c VALUES (1)").expect("parent row exists");
}
