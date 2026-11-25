//! Deferrable constraints and SET CONSTRAINTS, per-row versus deferred unique checks,
//! NOT VALID / VALIDATE CONSTRAINT, foreign key actions and MATCH FULL, SERIALIZABLE write
//! skew.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn fk_setup() -> (crate::support::TestDb, postgres::Client) {
    let (db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE parent (id int PRIMARY KEY);
         CREATE TABLE child (id int PRIMARY KEY, parent_id int REFERENCES parent DEFERRABLE INITIALLY DEFERRED);
         CREATE TABLE strict_child (id int PRIMARY KEY, parent_id int CONSTRAINT strict_fk REFERENCES parent DEFERRABLE INITIALLY IMMEDIATE);
         CREATE TABLE plain (id int, parent_id int CONSTRAINT plain_fk REFERENCES parent)",
    )
    .expect("setup");
    (db, c)
}

#[test]
fn deferred_foreign_keys_are_checked_at_commit() {
    let (_db, mut c) = fk_setup();
    c.batch_execute("BEGIN; INSERT INTO child VALUES (1, 10); INSERT INTO parent VALUES (10); COMMIT").expect("child first");
    c.batch_execute("BEGIN; INSERT INTO parent VALUES (20); INSERT INTO child VALUES (2, 99)").expect("deferred violation");
    let e = expect_code(c.batch_execute("COMMIT"), "23503");
    assert_eq!(message(&e), "insert or update on table \"child\" violates foreign key constraint \"child_parent_id_fkey\"");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM parent WHERE id = 20"), [["0"]], "the failed COMMIT rolled back");
    // Outside a block the statement's own transaction checks it.
    expect_code(c.batch_execute("INSERT INTO child VALUES (3, 98)"), "23503");
    // SET CONSTRAINTS ... IMMEDIATE checks what is pending now.
    c.batch_execute("BEGIN; INSERT INTO child VALUES (4, 97)").expect("pending");
    expect_code(c.batch_execute("SET CONSTRAINTS ALL IMMEDIATE"), "23503");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    // Deferring an INITIALLY IMMEDIATE constraint by name.
    c.batch_execute("BEGIN").expect("BEGIN");
    expect_code(c.batch_execute("INSERT INTO strict_child VALUES (1, 30)"), "23503");
    c.batch_execute("ROLLBACK; BEGIN; SET CONSTRAINTS strict_fk DEFERRED; INSERT INTO strict_child VALUES (1, 30); INSERT INTO parent VALUES (30); COMMIT")
        .expect("deferred by name");
    // Deleting a referenced parent is checked at commit too.
    c.batch_execute("BEGIN; DELETE FROM parent WHERE id = 10; INSERT INTO parent VALUES (10); COMMIT").expect("delete and reinsert");
    let e = expect_code(c.batch_execute("BEGIN; SET CONSTRAINTS plain_fk DEFERRED"), "42809");
    assert_eq!(message(&e), "constraint \"plain_fk\" is not deferrable");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    let e = expect_code(c.batch_execute("BEGIN; SET CONSTRAINTS no_such_constraint DEFERRED"), "42704");
    assert_eq!(message(&e), "constraint \"no_such_constraint\" does not exist");
    c.batch_execute("ROLLBACK").expect("ROLLBACK");
    let deferrable = rows(
        &mut c,
        "SELECT conname, condeferrable, condeferred, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname IN ('child_parent_id_fkey', 'strict_fk') ORDER BY 1",
    );
    assert_eq!(
        deferrable,
        [
            ["child_parent_id_fkey", "t", "t", "FOREIGN KEY (parent_id) REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED"],
            ["strict_fk", "t", "f", "FOREIGN KEY (parent_id) REFERENCES parent(id) DEFERRABLE"]
        ]
    );
}

#[test]
fn unique_checks_per_row_statement_or_commit() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE seq_strict (pos int UNIQUE); CREATE TABLE seq_stmt (pos int UNIQUE DEFERRABLE);
         CREATE TABLE seq_commit (pos int CONSTRAINT seq_commit_pos UNIQUE DEFERRABLE INITIALLY DEFERRED);
         INSERT INTO seq_strict VALUES (1), (2), (3); INSERT INTO seq_stmt VALUES (1), (2), (3); INSERT INTO seq_commit VALUES (1), (2), (3)",
    )
    .expect("setup");
    // Non-deferrable: the transient duplicate of row 1 → 2 fails.
    let e = expect_code(c.batch_execute("UPDATE seq_strict SET pos = pos + 1"), "23505");
    assert_eq!(message(&e), "duplicate key value violates unique constraint \"seq_strict_pos_key\"");
    // DEFERRABLE (immediate): checked at the end of the statement.
    c.batch_execute("UPDATE seq_stmt SET pos = pos + 1").expect("statement-level check");
    assert_eq!(rows(&mut c, "SELECT pos FROM seq_stmt ORDER BY pos"), [["2"], ["3"], ["4"]]);
    // INITIALLY DEFERRED: duplicates may exist until COMMIT.
    c.batch_execute("BEGIN; INSERT INTO seq_commit VALUES (2)").expect("duplicate for now");
    let e = expect_code(c.batch_execute("COMMIT"), "23505");
    assert_eq!(message(&e), "duplicate key value violates unique constraint \"seq_commit_pos\"");
    let e = expect_code(c.batch_execute("INSERT INTO seq_commit VALUES (3) ON CONFLICT (pos) DO NOTHING"), "55000");
    assert_eq!(message(&e), "ON CONFLICT does not support deferrable unique constraints/exclusion constraints as arbiters");
    expect_code(c.batch_execute("CREATE TABLE bad (a int UNIQUE NOT DEFERRABLE INITIALLY DEFERRED)"), "42601");
}

#[test]
fn not_valid_and_validate_constraint() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE legacy (v int); INSERT INTO legacy VALUES (-1)").expect("setup");
    expect_code(c.batch_execute("ALTER TABLE legacy ADD CONSTRAINT v_pos CHECK (v > 0)"), "23514");
    c.batch_execute("ALTER TABLE legacy ADD CONSTRAINT v_pos CHECK (v > 0) NOT VALID").expect("NOT VALID");
    expect_code(c.batch_execute("INSERT INTO legacy VALUES (-2)"), "23514");
    assert_eq!(
        rows(&mut c, "SELECT convalidated::text, pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = 'v_pos'"),
        [["false", "CHECK ((v > 0)) NOT VALID"]]
    );
    let e = expect_code(c.batch_execute("ALTER TABLE legacy VALIDATE CONSTRAINT v_pos"), "23514");
    assert_eq!(message(&e), "check constraint \"v_pos\" of relation \"legacy\" is violated by some row");
    c.batch_execute("UPDATE legacy SET v = 1; ALTER TABLE legacy VALIDATE CONSTRAINT v_pos").expect("VALIDATE");
    assert_eq!(rows(&mut c, "SELECT convalidated::text FROM pg_constraint WHERE conname = 'v_pos'"), [["true"]]);
    // Named NOT NULL constraints (Postgres 18).
    c.batch_execute("CREATE TABLE nnc (a int CONSTRAINT a_required NOT NULL, b int NOT NULL)").expect("CREATE");
    assert_eq!(
        rows(&mut c, "SELECT conname FROM pg_constraint WHERE conrelid = 'nnc'::regclass ORDER BY 1"),
        [["a_required"], ["nnc_b_not_null"]]
    );
    c.batch_execute("ALTER TABLE nnc DROP CONSTRAINT a_required; INSERT INTO nnc VALUES (NULL, 1)").expect("drop NOT NULL");
    let e = expect_code(c.batch_execute("CREATE TABLE bad (a int CHECK (a IN (SELECT 1)))"), "0A000");
    assert_eq!(message(&e), "cannot use subquery in check constraint");
}

#[test]
fn foreign_key_actions() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE p (id int PRIMARY KEY); INSERT INTO p VALUES (1), (2), (3);
         CREATE TABLE c_cascade (id int, pid int REFERENCES p ON UPDATE CASCADE);
         CREATE TABLE c_restrict (id int, pid int REFERENCES p ON DELETE RESTRICT);
         CREATE TABLE c_setdef (id int, pid int DEFAULT 3 REFERENCES p ON DELETE SET DEFAULT);
         INSERT INTO c_cascade VALUES (1, 1), (2, 2); INSERT INTO c_restrict VALUES (1, 2); INSERT INTO c_setdef VALUES (1, 3)",
    )
    .expect("setup");
    // Each child follows its own parent's new key.
    c.batch_execute("UPDATE p SET id = id * 10 WHERE id IN (1, 2)")
        .expect_err("RESTRICT is on delete only; NO ACTION on update blocks id 2");
    c.batch_execute("DELETE FROM c_restrict; UPDATE p SET id = id * 10 WHERE id IN (1, 2)").expect("cascade");
    assert_eq!(rows(&mut c, "SELECT id, pid FROM c_cascade ORDER BY id"), [["1", "10"], ["2", "20"]]);
    c.batch_execute("DELETE FROM c_cascade; INSERT INTO c_restrict VALUES (2, 20)").expect("restrict child");
    let e = expect_code(c.batch_execute("DELETE FROM p WHERE id = 20"), "23001");
    assert_eq!(
        message(&e),
        "update or delete on table \"p\" violates RESTRICT setting of foreign key constraint \"c_restrict_pid_fkey\" on table \"c_restrict\""
    );
    // SET DEFAULT to a parent that is gone fails as the parent's delete.
    let e = expect_code(c.batch_execute("DELETE FROM p WHERE id = 3"), "23503");
    assert_eq!(message(&e), "update or delete on table \"p\" violates foreign key constraint \"c_setdef_pid_fkey\" on table \"c_setdef\"");
    // ON DELETE SET NULL (column list) and MATCH FULL.
    c.batch_execute(
        "CREATE TABLE tenants (tid int, id int, PRIMARY KEY (tid, id));
         CREATE TABLE items (tid int NOT NULL, parent_id int, FOREIGN KEY (tid, parent_id) REFERENCES tenants ON DELETE SET NULL (parent_id));
         INSERT INTO tenants VALUES (1, 1); INSERT INTO items VALUES (1, 1); DELETE FROM tenants",
    )
    .expect("SET NULL (cols)");
    assert_eq!(rows(&mut c, "SELECT tid, parent_id FROM items"), [["1", "NULL"]]);
    c.batch_execute("CREATE TABLE ck (a int, b int, PRIMARY KEY (a, b)); CREATE TABLE fk_full (a int, b int, FOREIGN KEY (a, b) REFERENCES ck MATCH FULL)")
        .expect("MATCH FULL");
    c.batch_execute("INSERT INTO fk_full VALUES (NULL, NULL)").expect("all NULL");
    expect_code(c.batch_execute("INSERT INTO fk_full VALUES (9, NULL)"), "23503");
    let e = expect_code(c.batch_execute("CREATE TABLE bad (x text REFERENCES p (id))"), "42804");
    assert_eq!(message(&e), "foreign key constraint \"bad_x_fkey\" cannot be implemented");
}

#[test]
fn serializable_write_skew_fails_the_second_commit() {
    let (db, mut a) = setup();
    let mut b = db.connect();
    a.batch_execute(
        "CREATE TABLE oncall (doctor text PRIMARY KEY, on_duty boolean NOT NULL); INSERT INTO oncall VALUES ('alice', true), ('bob', true)",
    )
    .expect("setup");
    a.batch_execute("BEGIN ISOLATION LEVEL SERIALIZABLE").expect("BEGIN a");
    b.batch_execute("BEGIN ISOLATION LEVEL SERIALIZABLE").expect("BEGIN b");
    assert_eq!(rows(&mut a, "SELECT count(*) FROM oncall WHERE on_duty"), [["2"]]);
    assert_eq!(rows(&mut b, "SELECT count(*) FROM oncall WHERE on_duty"), [["2"]]);
    a.batch_execute("UPDATE oncall SET on_duty = false WHERE doctor = 'alice'").expect("a writes");
    b.batch_execute("UPDATE oncall SET on_duty = false WHERE doctor = 'bob'").expect("b writes");
    a.batch_execute("COMMIT").expect("first commit");
    let e = expect_code(b.batch_execute("COMMIT"), "40001");
    assert_eq!(message(&e), "could not serialize access due to read/write dependencies among transactions");
    assert_eq!(rows(&mut a, "SELECT doctor, on_duty FROM oncall ORDER BY doctor"), [["alice", "f"], ["bob", "t"]]);
    // REPEATABLE READ allows the same interleaving.
    a.batch_execute("UPDATE oncall SET on_duty = true").expect("reset");
    a.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").expect("BEGIN a");
    b.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ").expect("BEGIN b");
    rows(&mut a, "SELECT count(*) FROM oncall WHERE on_duty");
    rows(&mut b, "SELECT count(*) FROM oncall WHERE on_duty");
    a.batch_execute("UPDATE oncall SET on_duty = false WHERE doctor = 'alice'; COMMIT").expect("a");
    b.batch_execute("UPDATE oncall SET on_duty = false WHERE doctor = 'bob'; COMMIT").expect("b");
}
