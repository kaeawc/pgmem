//! Ports of the Go `storage` package tests. Where the Go engine's table-level conflict
//! detection deviated from Postgres, the port asserts Postgres' row-level behaviour instead
//! (see PARITY.md, "Deviations from pgmem-go").

use super::*;
use pgmem_catalog::{ColumnDef, TableDef};
use pgmem_types::Type;

fn db_with_tables(names: &[&str], seed: &[(i32, &str)]) -> (Arc<Database>, Vec<Oid>) {
    let db = Arc::new(Database::new());
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    let mut oids = Vec::new();
    for name in names {
        let oid = tx.catalog_mut().alloc_oid();
        tx.catalog_mut()
            .create_table(TableDef {
                schema: "public".into(),
                oid,
                name: (*name).into(),
                columns: vec![ColumnDef::new("id", Type::Int4), ColumnDef::new("name", Type::Text)],
                constraints: vec![],
                indexes: vec![],
                matview: None,
                unlogged: false,
                replident: 'd',
                reloptions: Vec::new(),
            })
            .expect("create");
        tx.created_table(oid);
        oids.push(oid);
    }
    for (id, name) in seed {
        tx.insert(oids[0], vec![Value::Int4(*id), Value::Text((*name).into())]);
    }
    tx.commit().expect("commit setup");
    (db, oids)
}

fn ids(tx: &Txn, oid: Oid) -> Vec<i32> {
    tx.scan(oid)
        .into_iter()
        .map(|(_, r)| match r[0] {
            Value::Int4(n) => n,
            _ => panic!("not int4"),
        })
        .collect()
}

fn committed_ids(db: &Arc<Database>, oid: Oid) -> Vec<i32> {
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    ids(&tx, oid)
}

fn row(id: i32) -> Row {
    vec![Value::Int4(id), Value::Null]
}

#[test]
fn txn_writes_not_visible_until_commit() {
    let (db, oids) = db_with_tables(&["items"], &[(1, "alpha"), (2, "beta")]);
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    tx.insert(oids[0], row(3));
    assert_eq!(ids(&tx, oids[0]), [1, 2, 3]);
    assert_eq!(committed_ids(&db, oids[0]), [1, 2]);
    tx.commit().expect("commit");
    assert_eq!(committed_ids(&db, oids[0]), [1, 2, 3]);
}

#[test]
fn txn_rollback_discards() {
    let (db, oids) = db_with_tables(&["items"], &[(1, "alpha"), (2, "beta")]);
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    tx.insert(oids[0], row(99));
    for (id, _) in tx.scan(oids[0]) {
        tx.delete(oids[0], id);
    }
    assert!(tx.scan(oids[0]).is_empty());
    tx.rollback();
    assert_eq!(committed_ids(&db, oids[0]), [1, 2]);
}

/// Go snapshotted each table on first access. In Postgres that is REPEATABLE READ behaviour;
/// READ COMMITTED (the default) sees other transactions' commits at the next statement.
#[test]
fn txn_snapshot_isolation() {
    let (db, oids) = db_with_tables(&["items"], &[(1, "alpha"), (2, "beta")]);
    let mut a = db.begin(Isolation::ReadCommitted);
    a.begin_statement();
    a.insert(oids[0], row(3));

    let mut rr = db.begin(Isolation::RepeatableRead);
    rr.begin_statement();
    let mut rc = db.begin(Isolation::ReadCommitted);
    rc.begin_statement();
    assert_eq!(ids(&rr, oids[0]).len(), 2);
    assert_eq!(ids(&rc, oids[0]).len(), 2);

    a.commit().expect("commit A");
    rr.begin_statement();
    rc.begin_statement();
    assert_eq!(ids(&rr, oids[0]).len(), 2, "repeatable read keeps its snapshot");
    assert_eq!(ids(&rc, oids[0]).len(), 3, "read committed sees the new commit");
    assert_eq!(committed_ids(&db, oids[0]).len(), 3);
}

#[test]
fn txn_rollback_is_idempotent() {
    let (db, _) = db_with_tables(&["items"], &[(1, "alpha")]);
    // Commit and rollback consume the transaction, so "rollback after commit" cannot be
    // expressed; an empty commit followed by a fresh rollback must both succeed.
    db.begin(Isolation::ReadCommitted).commit().expect("empty commit");
    db.begin(Isolation::ReadCommitted).rollback();
}

fn fresh(seed: &[i32]) -> (Arc<Database>, Oid, Txn) {
    let rows: Vec<(i32, &str)> = seed.iter().map(|n| (*n, "")).collect();
    let (db, oids) = db_with_tables(&["items"], &rows);
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    (db, oids[0], tx)
}

#[test]
fn savepoint_rollback_undoes_only_post_savepoint_work() {
    let (_db, t, mut tx) = fresh(&[1, 2]);
    tx.insert(t, row(3));
    tx.savepoint("sp1");
    tx.insert(t, row(4));
    tx.insert(t, row(5));
    assert_eq!(tx.scan(t).len(), 5);
    tx.rollback_to("sp1").expect("rollback to");
    assert_eq!(ids(&tx, t), [1, 2, 3]);
}

#[test]
fn savepoint_nested_rollback_to() {
    let (_db, t, mut tx) = fresh(&[1, 2]);
    tx.savepoint("outer");
    tx.insert(t, row(3));
    tx.savepoint("inner");
    tx.insert(t, row(4));
    tx.rollback_to("outer").expect("rollback to outer");
    assert_eq!(tx.rollback_to("inner").expect_err("inner is gone").code, "3B001");
    assert_eq!(ids(&tx, t), [1, 2]);
}

#[test]
fn savepoint_release_pops() {
    let (_db, t, mut tx) = fresh(&[1, 2]);
    tx.savepoint("sp");
    tx.insert(t, row(99));
    tx.release("sp").expect("release");
    assert_eq!(tx.scan(t).len(), 3);
    let err = tx.rollback_to("sp").expect_err("released");
    assert_eq!(err.code, "3B001");
    assert_eq!(err.message, "savepoint \"sp\" does not exist");
}

#[test]
fn savepoint_unknown_name_errors() {
    let (_db, _t, mut tx) = fresh(&[1]);
    assert_eq!(tx.rollback_to("does-not-exist").expect_err("unknown").code, "3B001");
    assert_eq!(tx.release("does-not-exist").expect_err("unknown").code, "3B001");
}

/// Deviation from pgmem-go: concurrent inserts of different rows into one table both commit
/// in Postgres; Go's table-level check aborted the second.
#[test]
fn commit_conflict_two_concurrent_writes() {
    let (db, oids) = db_with_tables(&["items"], &[]);
    let t = oids[0];
    let mut a = db.begin(Isolation::ReadCommitted);
    a.begin_statement();
    a.insert(t, row(1));
    let mut b = db.begin(Isolation::ReadCommitted);
    b.begin_statement();
    b.insert(t, row(2));
    a.commit().expect("commit A");
    b.commit().expect("commit B: disjoint rows do not conflict");
    assert_eq!(committed_ids(&db, t), [1, 2]);
}

/// Two transactions updating the same row: the second COMMIT fails with 40001.
#[test]
fn commit_conflict_same_row_update() {
    let (db, oids) = db_with_tables(&["items"], &[(1, "x")]);
    let t = oids[0];
    let mut a = db.begin(Isolation::ReadCommitted);
    a.begin_statement();
    let mut b = db.begin(Isolation::ReadCommitted);
    b.begin_statement();
    let id = a.scan(t)[0].0;
    a.update(t, id, row(10));
    b.update(t, id, row(20));
    a.commit().expect("commit A");
    let err = b.commit().expect_err("same row");
    assert_eq!(err.code, "40001");
    assert_eq!(committed_ids(&db, t), [10]);
}

#[test]
fn commit_no_conflict_one_side_read_only() {
    let (db, oids) = db_with_tables(&["items"], &[(0, "")]);
    let t = oids[0];
    let mut reader = db.begin(Isolation::RepeatableRead);
    reader.begin_statement();
    assert_eq!(reader.scan(t).len(), 1);
    let mut writer = db.begin(Isolation::ReadCommitted);
    writer.begin_statement();
    writer.insert(t, row(1));
    writer.commit().expect("writer commit");
    reader.begin_statement();
    assert_eq!(reader.scan(t).len(), 1);
    reader.commit().expect("reader commit");
}

#[test]
fn commit_no_conflict_disjoint_tables() {
    let (db, oids) = db_with_tables(&["a", "b"], &[]);
    let mut a = db.begin(Isolation::ReadCommitted);
    a.begin_statement();
    a.insert(oids[0], row(1));
    let mut b = db.begin(Isolation::ReadCommitted);
    b.begin_statement();
    b.insert(oids[1], row(2));
    a.commit().expect("commit A");
    b.commit().expect("commit B");
}

/// Deviation from pgmem-go: deleting one row while another transaction inserts a different
/// row does not conflict in Postgres.
#[test]
fn commit_conflict_detected_across_delete() {
    let (db, oids) = db_with_tables(&["items"], &[(1, ""), (2, "")]);
    let t = oids[0];
    let mut a = db.begin(Isolation::ReadCommitted);
    a.begin_statement();
    let second = a.scan(t)[1].0;
    a.delete(t, second);
    let mut b = db.begin(Isolation::ReadCommitted);
    b.begin_statement();
    b.insert(t, row(3));
    a.commit().expect("commit A");
    b.commit().expect("commit B");
    assert_eq!(committed_ids(&db, t), [1, 3]);
    // Deleting a row another transaction already deleted does conflict.
    let mut c = db.begin(Isolation::ReadCommitted);
    c.begin_statement();
    let mut d = db.begin(Isolation::ReadCommitted);
    d.begin_statement();
    let first = c.scan(t)[0].0;
    c.delete(t, first);
    d.delete(t, first);
    c.commit().expect("commit C");
    assert_eq!(d.commit().expect_err("double delete").code, "40001");
}

#[test]
fn ddl_is_transactional() {
    let (db, _) = db_with_tables(&["items"], &[]);
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    tx.catalog_mut().drop_table(&pgmem_catalog::QualName::new("public", "items"));
    assert!(tx.catalog().table("public", "items").is_none());
    tx.rollback();
    assert!(db.current().catalog.table("public", "items").is_some());
}

#[test]
fn updated_rows_move_to_end_of_heap() {
    let (db, oids) = db_with_tables(&["items"], &[(1, ""), (2, ""), (3, "")]);
    let t = oids[0];
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    let first = tx.scan(t)[0].0;
    tx.update(t, first, row(1));
    tx.commit().expect("commit");
    assert_eq!(committed_ids(&db, t), [2, 3, 1]);
}

#[test]
fn sequences_are_not_transactional() {
    let db = Database::new();
    let seq = SequenceDef {
        schema: "public".into(),
        oid: 1,
        name: "s".into(),
        start: 1,
        increment: 1,
        min: 1,
        max: i64::MAX,
        cycle: false,
        data_type: pgmem_types::Type::Int8,
        cache: 1,
        owned_by: None,
    };
    assert_eq!(db.nextval(&seq).ok(), Some(1));
    assert_eq!(db.nextval(&seq).ok(), Some(2));
    db.setval(&seq, 10, true).expect("setval");
    assert_eq!(db.nextval(&seq).ok(), Some(11));
    db.setval(&seq, 10, false).expect("setval");
    assert_eq!(db.nextval(&seq).ok(), Some(10));
}

/// `items` with a unique index on `id` (index lookups and the commit-time recheck, #218).
fn db_with_unique_id() -> (Arc<Database>, Oid) {
    let (db, oids) = db_with_tables(&["items"], &[(1, "alpha"), (2, "beta")]);
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    let def = tx.catalog().table_by_oid(oids[0]).cloned().expect("items");
    let mut def = (*def).clone();
    def.indexes.push(pgmem_catalog::IndexDef {
        oid: 0,
        name: "items_id_key".into(),
        columns: vec![0],
        exprs: vec![],
        include: vec![],
        reloptions: vec![],
        clustered: false,
        replica: false,
        method: "btree".into(),
        opclasses: Vec::new(),
        key_options: Vec::new(),
        unique: true,
        primary: false,
        nulls_not_distinct: false,
        predicate: None,
        valid: true,
        ready: true,
    });
    let name = def.qualified();
    tx.catalog_mut().replace_table(&name, def);
    tx.commit().expect("add index");
    (db, oids[0])
}

fn lookup_ids(tx: &Txn, oid: Oid, id: i32) -> Vec<i32> {
    let def = tx.catalog().table_by_oid(oid).cloned().expect("table");
    tx.lookup_latest(&def, &[0], &[Value::Int4(id)])
        .into_iter()
        .map(|(_, r)| match r[0] {
            Value::Int4(n) => n,
            _ => panic!("not int4"),
        })
        .collect()
}

#[test]
fn lookup_sees_committed_pending_deleted_and_savepoint_state() {
    let (db, t) = db_with_unique_id();
    let mut tx = db.begin(Isolation::ReadCommitted);
    tx.begin_statement();
    assert_eq!(lookup_ids(&tx, t, 1), [1]);
    assert_eq!(lookup_ids(&tx, t, 3), Vec::<i32>::new());
    tx.insert(t, row(3));
    assert_eq!(lookup_ids(&tx, t, 3), [3], "a pending insert");
    tx.savepoint("sp");
    let committed_one = tx.scan(t).into_iter().find(|(_, r)| r[0] == Value::Int4(1)).expect("row 1").0;
    tx.delete(t, committed_one);
    let pending_three = tx.scan(t).into_iter().find(|(_, r)| r[0] == Value::Int4(3)).expect("row 3").0;
    tx.update(t, pending_three, row(4));
    assert_eq!(lookup_ids(&tx, t, 1), Vec::<i32>::new(), "a deleted committed row");
    assert_eq!(lookup_ids(&tx, t, 3), Vec::<i32>::new(), "an updated pending row's old key");
    assert_eq!(lookup_ids(&tx, t, 4), [4]);
    tx.rollback_to("sp").expect("rollback to");
    assert_eq!(lookup_ids(&tx, t, 1), [1], "restored by ROLLBACK TO");
    assert_eq!(lookup_ids(&tx, t, 3), [3]);
    assert_eq!(lookup_ids(&tx, t, 4), Vec::<i32>::new());
    tx.commit().expect("commit");
    let mut after = db.begin(Isolation::ReadCommitted);
    after.begin_statement();
    assert_eq!(lookup_ids(&after, t, 3), [3], "committed into the maintained index");
    assert_eq!(committed_ids(&db, t), [1, 2, 3]);
}

/// A commit applied in place must not change what an older snapshot sees.
#[test]
fn commit_leaves_an_open_snapshot_unchanged() {
    let (db, t) = db_with_unique_id();
    let mut reader = db.begin(Isolation::RepeatableRead);
    reader.begin_statement();
    assert_eq!(lookup_ids(&reader, t, 1), [1]);
    let mut writer = db.begin(Isolation::ReadCommitted);
    writer.begin_statement();
    let one = writer.scan(t)[0].0;
    writer.delete(t, one);
    writer.insert(t, row(5));
    writer.commit().expect("commit");
    reader.begin_statement();
    assert_eq!(ids(&reader, t), [1, 2]);
    assert_eq!(lookup_ids(&reader, t, 1), [1]);
    assert_eq!(lookup_ids(&reader, t, 5), Vec::<i32>::new());
    assert_eq!(committed_ids(&db, t), [2, 5]);
}

/// Concurrent inserts of one key: the later COMMIT fails with 23505 and changes nothing.
#[test]
fn commit_rechecks_unique_keys_committed_concurrently() {
    let (db, t) = db_with_unique_id();
    let mut a = db.begin(Isolation::ReadCommitted);
    a.begin_statement();
    let mut b = db.begin(Isolation::ReadCommitted);
    b.begin_statement();
    a.insert(t, row(7));
    b.insert(t, row(8));
    b.insert(t, row(7));
    a.commit().expect("commit A");
    let err = b.commit().expect_err("duplicate key");
    assert_eq!(err.code, "23505");
    assert_eq!(committed_ids(&db, t), [1, 2, 7]);
    // A key freed by the committing transaction's own delete is not a duplicate.
    let mut c = db.begin(Isolation::ReadCommitted);
    c.begin_statement();
    let seven = c.scan(t).into_iter().find(|(_, r)| r[0] == Value::Int4(7)).expect("row 7").0;
    c.delete(t, seven);
    c.insert(t, row(7));
    c.commit().expect("delete and reinsert one key");
    assert_eq!(committed_ids(&db, t), [1, 2, 7]);
}
