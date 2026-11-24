//! Port of examples/chat/chat_test.go: UNION ALL feeds, EXISTS gating, self-joins and ON CONFLICT DO NOTHING.

use crate::support::{TestDb, setup, utc_ts};
use jiff::{SignedDuration, Timestamp};
use postgres::Client;

const ADD_USER: &str = "-- name: AddUser :one

INSERT INTO users (name) VALUES ($1) RETURNING id, name
";
const ADD_ROOM: &str = "-- name: AddRoom :one
INSERT INTO rooms (name) VALUES ($1) RETURNING id, name
";
const SUBSCRIBE: &str = "-- name: Subscribe :exec
INSERT INTO subscriptions (user_id, room_id) VALUES ($1, $2)
ON CONFLICT (user_id, room_id) DO NOTHING
";
const POST_MESSAGE: &str = "-- name: PostMessage :one
INSERT INTO messages (room_id, author_id, parent_id, body, sent_at)
VALUES ($1, $2, $3, $4, $5)
RETURNING id, room_id, author_id, parent_id, body, sent_at
";
const POST_SYSTEM_MESSAGE: &str = "-- name: PostSystemMessage :one
INSERT INTO system_messages (room_id, body, sent_at)
VALUES ($1, $2, $3) RETURNING id, room_id, body, sent_at
";
const ROOM_FEED: &str = "-- name: RoomFeed :many
SELECT m.id, m.body, m.sent_at, 'user' AS source
FROM messages m
WHERE m.room_id = $1
UNION ALL
SELECT s.id, s.body, s.sent_at, 'system' AS source
FROM system_messages s
WHERE s.room_id = $1
ORDER BY sent_at DESC
LIMIT $2 OFFSET $3
";
const SUBSCRIBED_ROOMS: &str = "-- name: SubscribedRooms :many
SELECT r.id, r.name FROM rooms r
WHERE EXISTS (
    SELECT 1 FROM subscriptions s
    WHERE s.user_id = $1 AND s.room_id = r.id
)
ORDER BY r.name
";
const REPLY_THREAD: &str = "-- name: ReplyThread :many
SELECT m.id, m.body AS reply_body, m.sent_at,
       parent.body AS parent_body
FROM messages m
JOIN messages parent ON parent.id = m.parent_id
WHERE m.parent_id = $1 OR parent.id = $1
ORDER BY m.sent_at
";

fn setup_schema() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(include_str!("../../../../examples/chat/schema.sql")).expect("schema");
    (db, c)
}

struct Fixture {
    alice: i64,
    bob: i64,
    room: i64,
    parent: i64,
}

fn subscribe(c: &mut Client, user: i64, room: i64) {
    c.execute(SUBSCRIBE, &[&user, &room]).expect("Subscribe");
}

fn post_message(c: &mut Client, room: i64, author: i64, parent: Option<i64>, body: &str, at: Timestamp) -> i64 {
    c.query_one(POST_MESSAGE, &[&room, &author, &parent, &body, &at]).expect("PostMessage").get(0)
}

fn fixture(c: &mut Client) -> Fixture {
    let alice: i64 = c.query_one(ADD_USER, &[&"alice"]).expect("AddUser alice").get(0);
    let bob: i64 = c.query_one(ADD_USER, &[&"bob"]).expect("AddUser bob").get(0);
    let room: i64 = c.query_one(ADD_ROOM, &[&"general"]).expect("AddRoom").get(0);
    subscribe(c, alice, room);
    let now = utc_ts(2024, 7, 15, 12, 0, 0);
    let parent = post_message(c, room, alice, None, "hello world", now);
    post_message(c, room, bob, Some(parent), "hi back", now + SignedDuration::from_mins(1));
    c.query_one(POST_SYSTEM_MESSAGE, &[&room, &"user alice joined", &(now - SignedDuration::from_hours(1))]).expect("PostSystemMessage");
    Fixture { alice, bob, room, parent }
}

/// RoomFeed as (body, source). LIMIT/OFFSET parameters are bigint in Postgres.
fn query_feed(c: &mut Client, room: i64, limit: i64, offset: i64) -> Vec<(String, String)> {
    c.query(ROOM_FEED, &[&room, &limit, &offset]).expect("RoomFeed").iter().map(|r| (r.get(1), r.get(3))).collect()
}

fn query_subscribed(c: &mut Client, user: i64) -> Vec<i64> {
    c.query(SUBSCRIBED_ROOMS, &[&user]).expect("SubscribedRooms").iter().map(|r| r.get(0)).collect()
}

#[test]
fn room_feed_union_all() {
    let (_db, mut c) = setup_schema();
    let f = fixture(&mut c);
    let rows = query_feed(&mut c, f.room, 10, 0);
    let sources: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
    // ORDER BY sent_at DESC: reply first, then parent, then system.
    assert_eq!(sources, vec!["user", "user", "system"]);
    let page = query_feed(&mut c, f.room, 1, 1);
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].0, "hello world");
}

#[test]
fn subscribed_rooms() {
    let (_db, mut c) = setup_schema();
    let f = fixture(&mut c);
    assert_eq!(query_subscribed(&mut c, f.alice), vec![f.room]);
    assert!(query_subscribed(&mut c, f.bob).is_empty());
}

#[test]
fn subscribe_on_conflict_do_nothing() {
    let (_db, mut c) = setup_schema();
    let f = fixture(&mut c);
    subscribe(&mut c, f.alice, f.room);
    assert_eq!(query_subscribed(&mut c, f.alice).len(), 1);
}

#[test]
fn reply_thread() {
    let (_db, mut c) = setup_schema();
    let f = fixture(&mut c);
    let rows: Vec<(String, String)> =
        c.query(REPLY_THREAD, &[&Some(f.parent)]).expect("ReplyThread").iter().map(|r| (r.get(1), r.get(3))).collect();
    assert_eq!(rows, vec![("hi back".to_owned(), "hello world".to_owned())]);
}
