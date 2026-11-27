//! Relation locks: LOCK TABLE targets (views, partitions, ONLY), the locks queries, DML and
//! DDL take implicitly, their `pg_locks` rows, and their conflicts with LOCK ... NOWAIT in
//! another session.

use crate::support::{expect_code, message, setup, texts};

/// This backend's relation locks, as "relation mode", sorted.
fn locks(c: &mut postgres::Client) -> Vec<String> {
    texts(
        c,
        "SELECT l FROM (SELECT relation::regclass::text || ' ' || mode AS l FROM pg_locks \
         WHERE pid = pg_backend_pid() AND locktype = 'relation' AND relation <> 'pg_locks'::regclass) x ORDER BY l COLLATE \"C\"",
    )
    .into_iter()
    .map(|r| r[0].clone().unwrap_or_default())
    .collect()
}

/// Runs `sql` in a fresh transaction and returns the relation locks it holds afterwards.
fn locks_of(c: &mut postgres::Client, sql: &str) -> Vec<String> {
    c.batch_execute("BEGIN").expect("BEGIN");
    c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let out = locks(c);
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    out
}

const SCHEMA: &str = "CREATE TABLE t (id int PRIMARY KEY, v text); CREATE INDEX t_v ON t (v); \
    CREATE TABLE u (id int PRIMARY KEY, t_id int); CREATE VIEW vw AS SELECT * FROM t; CREATE SEQUENCE s; \
    CREATE TABLE p (id int PRIMARY KEY) PARTITION BY RANGE (id); \
    CREATE TABLE p1 PARTITION OF p FOR VALUES FROM (0) TO (10); CREATE TABLE p2 PARTITION OF p FOR VALUES FROM (10) TO (20)";

#[test]
fn pg_locks_lists_the_locks_statements_take() {
    let (_db, mut c) = setup();
    c.batch_execute(SCHEMA).expect("setup");
    // Querying pg_locks locks pg_locks itself.
    c.batch_execute("BEGIN").expect("BEGIN");
    let own = texts(
        &mut c,
        "SELECT mode, granted::text, fastpath::text FROM pg_locks WHERE pid = pg_backend_pid() AND relation = 'pg_locks'::regclass",
    );
    assert_eq!(own, [[Some("AccessShareLock".to_owned()), Some("true".to_owned()), Some("true".to_owned())]]);
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    // A query locks the tables it reads with their indexes, and views with their base tables.
    assert_eq!(locks_of(&mut c, "SELECT * FROM t"), ["t AccessShareLock", "t_pkey AccessShareLock", "t_v AccessShareLock"]);
    assert_eq!(
        locks_of(&mut c, "SELECT * FROM vw"),
        ["t AccessShareLock", "t_pkey AccessShareLock", "t_v AccessShareLock", "vw AccessShareLock"]
    );
    assert_eq!(
        locks_of(&mut c, "SELECT * FROM p"),
        [
            "p AccessShareLock",
            "p1 AccessShareLock",
            "p1_pkey AccessShareLock",
            "p2 AccessShareLock",
            "p2_pkey AccessShareLock",
            "p_pkey AccessShareLock"
        ]
    );
    // FOR UPDATE / SHARE takes ROW SHARE on the tables it applies to.
    assert_eq!(locks_of(&mut c, "SELECT * FROM t FOR SHARE"), ["t RowShareLock", "t_pkey RowShareLock", "t_v RowShareLock"]);
    assert_eq!(
        locks_of(&mut c, "SELECT * FROM t, u FOR UPDATE OF u"),
        ["t AccessShareLock", "t_pkey AccessShareLock", "t_v AccessShareLock", "u RowShareLock", "u_pkey RowShareLock"]
    );
    // INSERT locks its target; UPDATE and DELETE also lock its indexes.
    assert_eq!(locks_of(&mut c, "INSERT INTO t VALUES (1, 'a')"), ["t RowExclusiveLock"]);
    assert_eq!(
        locks_of(&mut c, "INSERT INTO u SELECT id, id FROM t"),
        ["t AccessShareLock", "t_pkey AccessShareLock", "t_v AccessShareLock", "u RowExclusiveLock"]
    );
    assert_eq!(
        locks_of(&mut c, "DELETE FROM u USING t WHERE t.id = u.t_id"),
        ["t AccessShareLock", "t_pkey AccessShareLock", "t_v AccessShareLock", "u RowExclusiveLock", "u_pkey RowExclusiveLock"]
    );
    assert_eq!(locks_of(&mut c, "UPDATE t SET v = 'x'"), ["t RowExclusiveLock", "t_pkey RowExclusiveLock", "t_v RowExclusiveLock"]);
    assert_eq!(locks_of(&mut c, "SELECT nextval('s')"), ["s RowExclusiveLock"]);
    // Locks are held to the end of the transaction, once per mode.
    assert_eq!(
        locks_of(&mut c, "LOCK t IN SHARE MODE; LOCK t IN SHARE MODE; SELECT * FROM u; LOCK t"),
        ["t AccessExclusiveLock", "t ShareLock", "u AccessShareLock", "u_pkey AccessShareLock"]
    );
    // ROLLBACK TO releases the locks taken after the savepoint.
    assert_eq!(locks_of(&mut c, "LOCK u IN SHARE MODE; SAVEPOINT a; LOCK t; SELECT * FROM t; ROLLBACK TO a"), ["u ShareLock"]);
    // Outside a transaction nothing stays locked.
    c.batch_execute("SELECT * FROM t").expect("SELECT");
    assert!(locks(&mut c).is_empty());
}

#[test]
fn lock_table_targets() {
    let (_db, mut c) = setup();
    c.batch_execute(SCHEMA).expect("setup");
    // A view locks the relations its query reads.
    assert_eq!(locks_of(&mut c, "LOCK vw IN SHARE MODE"), ["t ShareLock", "vw ShareLock"]);
    // A partitioned table locks its partitions, unless ONLY.
    assert_eq!(locks_of(&mut c, "LOCK TABLE p IN ROW SHARE MODE"), ["p RowShareLock", "p1 RowShareLock", "p2 RowShareLock"]);
    assert_eq!(locks_of(&mut c, "LOCK TABLE ONLY p"), ["p AccessExclusiveLock"]);
    assert_eq!(locks_of(&mut c, "LOCK TABLE t, u IN EXCLUSIVE MODE NOWAIT"), ["t ExclusiveLock", "u ExclusiveLock"]);
    // Every mode by name.
    let modes = [
        ("ACCESS SHARE", "AccessShareLock"),
        ("ROW SHARE", "RowShareLock"),
        ("ROW EXCLUSIVE", "RowExclusiveLock"),
        ("SHARE UPDATE EXCLUSIVE", "ShareUpdateExclusiveLock"),
        ("SHARE", "ShareLock"),
        ("SHARE ROW EXCLUSIVE", "ShareRowExclusiveLock"),
        ("EXCLUSIVE", "ExclusiveLock"),
        ("ACCESS EXCLUSIVE", "AccessExclusiveLock"),
    ];
    for (sql, name) in modes {
        assert_eq!(locks_of(&mut c, &format!("LOCK u IN {sql} MODE")), [format!("u {name}")]);
    }
    // Relations that cannot be locked.
    c.batch_execute("BEGIN").expect("BEGIN");
    let e = expect_code(c.batch_execute("LOCK s"), "42809");
    assert_eq!(message(&e), "cannot lock relation \"s\"");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("This operation is not supported for sequences."));
    c.batch_execute("ROLLBACK; BEGIN").expect("BEGIN");
    let e = expect_code(c.batch_execute("LOCK t_pkey"), "42809");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("This operation is not supported for indexes."));
    c.batch_execute("ROLLBACK; BEGIN").expect("BEGIN");
    let e = expect_code(c.batch_execute("LOCK nope"), "42P01");
    assert_eq!(message(&e), "relation \"nope\" does not exist");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
}

#[test]
fn implicit_locks_conflict_with_lock_nowait() {
    let (db, mut a) = setup();
    let mut b = db.connect();
    a.batch_execute(SCHEMA).expect("setup");
    let nowait = |b: &mut postgres::Client, sql: &str| -> Result<(), postgres::Error> {
        b.batch_execute("BEGIN").expect("BEGIN b");
        let r = b.batch_execute(sql);
        b.batch_execute("ROLLBACK").expect("ROLLBACK b");
        r
    };
    // DML's ROW EXCLUSIVE conflicts with SHARE, not with ROW EXCLUSIVE.
    a.batch_execute("BEGIN; INSERT INTO t VALUES (1, 'a')").expect("INSERT");
    let e = expect_code(nowait(&mut b, "LOCK t IN SHARE MODE NOWAIT"), "55P03");
    assert_eq!(message(&e), "could not obtain lock on relation \"t\"");
    nowait(&mut b, "LOCK t IN ROW EXCLUSIVE MODE NOWAIT").expect("compatible");
    a.batch_execute("COMMIT").expect("COMMIT");
    nowait(&mut b, "LOCK t IN SHARE MODE NOWAIT").expect("released at commit");
    // A read's ACCESS SHARE only conflicts with ACCESS EXCLUSIVE; its indexes are locked too.
    a.batch_execute("BEGIN; SELECT * FROM vw").expect("SELECT");
    nowait(&mut b, "LOCK t IN EXCLUSIVE MODE NOWAIT").expect("compatible");
    expect_code(nowait(&mut b, "LOCK t NOWAIT"), "55P03");
    expect_code(nowait(&mut b, "LOCK vw NOWAIT"), "55P03");
    a.batch_execute("ROLLBACK").expect("ROLLBACK");
    // FOR UPDATE's ROW SHARE conflicts with EXCLUSIVE.
    a.batch_execute("BEGIN; SELECT * FROM u FOR UPDATE").expect("FOR UPDATE");
    expect_code(nowait(&mut b, "LOCK u IN EXCLUSIVE MODE NOWAIT"), "55P03");
    nowait(&mut b, "LOCK u IN SHARE MODE NOWAIT").expect("compatible");
    a.batch_execute("ROLLBACK").expect("ROLLBACK");
    // LOCK on a partitioned table covers its partitions, except with ONLY.
    a.batch_execute("BEGIN; LOCK p IN SHARE MODE").expect("LOCK p");
    let e = expect_code(nowait(&mut b, "LOCK p1 IN ROW EXCLUSIVE MODE NOWAIT"), "55P03");
    assert_eq!(message(&e), "could not obtain lock on relation \"p1\"");
    a.batch_execute("ROLLBACK; BEGIN; LOCK ONLY p IN SHARE MODE").expect("LOCK ONLY p");
    nowait(&mut b, "LOCK p1 IN ROW EXCLUSIVE MODE NOWAIT").expect("partition not locked");
    a.batch_execute("ROLLBACK").expect("ROLLBACK");
    // A DDL statement's lock: ALTER TABLE ... ADD COLUMN holds ACCESS EXCLUSIVE.
    a.batch_execute("BEGIN; ALTER TABLE u ADD COLUMN z int").expect("ALTER");
    expect_code(nowait(&mut b, "LOCK u IN ACCESS SHARE MODE NOWAIT"), "55P03");
    a.batch_execute("ROLLBACK").expect("ROLLBACK");
}

#[test]
fn ddl_lock_levels() {
    let (_db, mut c) = setup();
    c.batch_execute(SCHEMA).expect("setup");
    c.batch_execute(
        "CREATE TABLE q (id int PRIMARY KEY); \
         ALTER TABLE u ADD CONSTRAINT ckv CHECK (id > 0) NOT VALID; \
         CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; \
         CREATE TRIGGER tr BEFORE INSERT ON u FOR EACH ROW EXECUTE FUNCTION trg()",
    )
    .expect("setup");
    let cases: &[(&str, &[&str])] = &[
        ("ALTER TABLE t ADD COLUMN z int", &["t AccessExclusiveLock"]),
        ("ALTER TABLE t ALTER COLUMN v SET DEFAULT 'x'", &["t AccessExclusiveLock"]),
        ("ALTER TABLE t ALTER COLUMN v SET NOT NULL", &["t AccessExclusiveLock"]),
        ("ALTER TABLE t RENAME COLUMN v TO w", &["t AccessExclusiveLock"]),
        ("ALTER TABLE u ADD CONSTRAINT ck CHECK (id > 0)", &["u AccessExclusiveLock"]),
        ("ALTER TABLE u VALIDATE CONSTRAINT ckv", &["u ShareUpdateExclusiveLock"]),
        ("ALTER TABLE u DISABLE TRIGGER ALL", &["u ShareRowExclusiveLock"]),
        (
            "ALTER TABLE u ADD CONSTRAINT fk FOREIGN KEY (t_id) REFERENCES t NOT VALID",
            &["t AccessShareLock", "t ShareRowExclusiveLock", "t_pkey AccessShareLock", "u AccessShareLock", "u ShareRowExclusiveLock"],
        ),
        (
            "ALTER TABLE p ATTACH PARTITION q FOR VALUES FROM (20) TO (30)",
            &["p ShareUpdateExclusiveLock", "p_pkey ShareUpdateExclusiveLock", "q AccessExclusiveLock"],
        ),
        ("CREATE INDEX u_t_id ON u (t_id)", &["u ShareLock", "u_t_id AccessExclusiveLock"]),
        ("ALTER TABLE u ADD CONSTRAINT uq UNIQUE (t_id)", &["u AccessExclusiveLock", "u ShareLock", "uq AccessExclusiveLock"]),
        ("CREATE TRIGGER tr2 AFTER INSERT ON u FOR EACH ROW EXECUTE FUNCTION trg()", &["u ShareRowExclusiveLock"]),
        ("ALTER TRIGGER tr ON u RENAME TO tr3", &["u AccessExclusiveLock"]),
        ("ALTER INDEX t_v RENAME TO t_w", &["t_w ShareUpdateExclusiveLock"]),
        ("ALTER TABLE t RENAME TO tt", &["tt AccessExclusiveLock"]),
        ("ALTER VIEW vw OWNER TO CURRENT_USER", &["vw AccessExclusiveLock"]),
        ("ALTER SEQUENCE s INCREMENT 2", &["s RowExclusiveLock", "s ShareRowExclusiveLock"]),
        ("CREATE SEQUENCE s2", &["s2 AccessExclusiveLock"]),
        ("ANALYZE u", &["u ShareUpdateExclusiveLock", "u_pkey AccessShareLock"]),
        ("CREATE TABLE n (id int)", &["n AccessExclusiveLock"]),
        (
            "CREATE VIEW v2 AS SELECT t.v, u.t_id FROM t JOIN u ON u.t_id = t.id",
            &["t AccessShareLock", "u AccessShareLock", "v2 AccessExclusiveLock"],
        ),
        ("CREATE OR REPLACE VIEW vw AS SELECT * FROM t", &["t AccessShareLock", "vw AccessExclusiveLock"]),
        ("COMMENT ON COLUMN t.v IS 'x'", &["pg_description RowExclusiveLock", "t ShareUpdateExclusiveLock"]),
        ("DROP TRIGGER tr ON u", &["u AccessExclusiveLock", "u AccessShareLock"]),
    ];
    for (sql, want) in cases {
        assert_eq!(locks_of(&mut c, sql), *want, "{sql}");
    }
}
