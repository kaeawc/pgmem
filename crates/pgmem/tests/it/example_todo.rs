//! Port of examples/todo/todo_test.go: upserts, soft deletes, timestamp + interval math, bool_and.

use crate::support::{TestDb, setup, utc_ts};
use jiff::{SignedDuration, Timestamp};
use postgres::Client;

const UPSERT_LIST: &str = "-- name: UpsertList :one

INSERT INTO lists (id, name, created_at)
VALUES ($1, $2, $3)
ON CONFLICT (id) DO UPDATE SET name = excluded.name
RETURNING id, name, created_at, deleted_at
";
const SOFT_DELETE_LIST: &str = "-- name: SoftDeleteList :exec
UPDATE lists SET deleted_at = $1 WHERE id = $2
";
const ACTIVE_LISTS: &str = "-- name: ActiveLists :many
SELECT id, name, created_at, deleted_at FROM lists WHERE deleted_at IS NULL ORDER BY created_at DESC
";
const ADD_ITEM: &str = "-- name: AddItem :one
INSERT INTO items (list_id, title, done, due_at, created_at)
VALUES ($1, $2, false, $3, $4)
RETURNING id, list_id, title, done, due_at, created_at, deleted_at
";
const COMPLETE_ITEM: &str = "-- name: CompleteItem :exec
UPDATE items SET done = true WHERE id = $1
";
const ITEMS_DUE_WITHIN: &str = "-- name: ItemsDueWithin :many
SELECT id, list_id, title, done, due_at, created_at, deleted_at FROM items
WHERE deleted_at IS NULL
  AND due_at IS NOT NULL
  AND due_at <= $1::timestamptz + interval '1 day'
ORDER BY due_at
";
const LIST_COMPLETION: &str = "-- name: ListCompletion :many
SELECT list_id, bool_and(done) AS all_done, count(*) AS total
FROM items
WHERE deleted_at IS NULL
GROUP BY list_id
ORDER BY list_id
";

fn setup_schema() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(include_str!("../../../../examples/todo/schema.sql")).expect("schema");
    (db, c)
}

fn now() -> Timestamp {
    utc_ts(2024, 7, 15, 12, 0, 0)
}

/// UpsertList; returns (id, name).
fn upsert(c: &mut Client, id: i64, name: &str) -> (i64, String) {
    let row = c.query_one(UPSERT_LIST, &[&id, &name, &now()]).expect("UpsertList");
    (row.get(0), row.get(1))
}

fn add_item(c: &mut Client, list: i64, title: &str, due: Option<Timestamp>) {
    c.query_one(ADD_ITEM, &[&list, &title, &due, &now()]).expect("AddItem");
}

/// ListCompletion as (list_id, all_done, total).
fn query_completion(c: &mut Client) -> Vec<(i64, bool, i64)> {
    c.query(LIST_COMPLETION, &[]).expect("ListCompletion").iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect()
}

#[test]
fn upsert_list() {
    let (_db, mut c) = setup_schema();
    assert_eq!(upsert(&mut c, 1, "groceries"), (1, "groceries".to_owned()));
    assert_eq!(upsert(&mut c, 1, "shopping"), (1, "shopping".to_owned()));
}

#[test]
fn soft_delete_filters_out() {
    let (_db, mut c) = setup_schema();
    for (i, name) in [(1, "a"), (2, "b"), (3, "c")] {
        upsert(&mut c, i, name);
    }
    c.execute(SOFT_DELETE_LIST, &[&Some(now()), &2i64]).expect("SoftDeleteList");
    let mut active: Vec<i64> = c.query(ACTIVE_LISTS, &[]).expect("ActiveLists").iter().map(|r| r.get(0)).collect();
    // All lists share created_at, so the ORDER BY leaves their relative order unspecified.
    active.sort_unstable();
    assert_eq!(active, vec![1, 3]);
}

#[test]
fn items_due_within() {
    let (_db, mut c) = setup_schema();
    upsert(&mut c, 1, "main");
    add_item(&mut c, 1, "soon", Some(now() + SignedDuration::from_hours(12)));
    add_item(&mut c, 1, "later", Some(now() + SignedDuration::from_hours(48)));
    add_item(&mut c, 1, "anytime", None);
    let titles: Vec<String> = c.query(ITEMS_DUE_WITHIN, &[&now()]).expect("ItemsDueWithin").iter().map(|r| r.get(2)).collect();
    assert_eq!(titles, vec!["soon"]);
}

#[test]
fn list_completion() {
    let (_db, mut c) = setup_schema();
    upsert(&mut c, 1, "alpha");
    upsert(&mut c, 2, "beta");
    for (list, n) in [(1, 3), (2, 2)] {
        for _ in 0..n {
            add_item(&mut c, list, "item", None);
        }
    }
    assert_eq!(query_completion(&mut c), vec![(1, false, 3), (2, false, 2)]);
    for id in 1..=3i64 {
        c.execute(COMPLETE_ITEM, &[&id]).expect("CompleteItem");
    }
    assert_eq!(query_completion(&mut c), vec![(1, true, 3), (2, false, 2)]);
}
