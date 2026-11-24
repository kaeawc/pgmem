//! Port of examples/events/events_test.go: jsonb @> and ->>, regex match, extract(epoch) and date_trunc.

use crate::support::{TestDb, setup, utc_ts};
use jiff::{SignedDuration, Timestamp};
use postgres::Client;
use serde_json::{Value, json};

const RECORD_EVENT: &str = "-- name: RecordEvent :one

INSERT INTO events (kind, body, message, created_at)
VALUES ($1, $2, $3, $4) RETURNING id, kind, body, message, created_at
";
const EVENTS_BY_PAYLOAD_KEY: &str = "-- name: EventsByPayloadKey :many
SELECT id, kind, body, message, created_at FROM events
WHERE body @> $1::jsonb
ORDER BY created_at
";
const EVENTS_BY_KIND_FIELD: &str = "-- name: EventsByKindField :many
SELECT id, body ->> 'user_id' AS user_id, message
FROM events
WHERE kind = $1
ORDER BY created_at DESC
LIMIT $2
";
const ERROR_EVENTS: &str = "-- name: ErrorEvents :many
SELECT id, kind, body, message, created_at FROM events
WHERE message ~* 'error'
ORDER BY created_at DESC
";
const EVENT_EPOCHS: &str = "-- name: EventEpochs :many
SELECT id, extract(epoch FROM created_at)::bigint AS epoch
FROM events
ORDER BY id
";
const EVENTS_PER_HOUR: &str = "-- name: EventsPerHour :many
SELECT date_trunc('hour', created_at)::timestamptz AS hour,
       count(*) AS n
FROM events
GROUP BY date_trunc('hour', created_at)
ORDER BY hour
";

fn setup_schema() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(include_str!("../../../../examples/events/schema.sql")).expect("schema");
    (db, c)
}

fn base() -> Timestamp {
    utc_ts(2024, 7, 15, 9, 30, 0)
}

fn seed(c: &mut Client) {
    let events = [
        ("login", json!({"user_id": "u1", "ip": "10.0.0.1"}), "user u1 signed in", base()),
        ("login", json!({"user_id": "u2", "ip": "10.0.0.2"}), "user u2 signed in", base() + SignedDuration::from_mins(20)),
        ("error", json!({"code": 500, "path": "/api/x"}), "ERROR: handler panicked", base() + SignedDuration::from_hours(2)),
        ("purchase", json!({"user_id": "u1", "amount": 42}), "u1 bought widget", base() + SignedDuration::from_hours(3)),
    ];
    for (kind, body, message, at) in events {
        c.query_one(RECORD_EVENT, &[&kind, &body, &message, &at]).expect("RecordEvent");
    }
}

#[test]
fn events_by_payload_key() {
    let (_db, mut c) = setup_schema();
    seed(&mut c);
    let rows = c.query(EVENTS_BY_PAYLOAD_KEY, &[&json!({"user_id": "u1"})]).expect("EventsByPayloadKey");
    assert_eq!(rows.len(), 2, "login + purchase by u1");
    for r in &rows {
        let body: Value = r.get(2);
        assert!(body.to_string().contains(r#""u1""#), "row body {body} lacks u1");
    }
}

#[test]
fn events_by_kind_field() {
    let (_db, mut c) = setup_schema();
    seed(&mut c);
    let users: Vec<Option<String>> =
        c.query(EVENTS_BY_KIND_FIELD, &[&"login", &10i64]).expect("EventsByKindField").iter().map(|r| r.get(1)).collect();
    // created_at DESC: u2 (later) before u1.
    assert_eq!(users, vec![Some("u2".to_owned()), Some("u1".to_owned())]);
}

#[test]
fn error_events() {
    let (_db, mut c) = setup_schema();
    seed(&mut c);
    let messages: Vec<String> = c.query(ERROR_EVENTS, &[]).expect("ErrorEvents").iter().map(|r| r.get(3)).collect();
    assert_eq!(messages.len(), 1, "only the 'ERROR:' row matches: {messages:?}");
    assert!(messages[0].contains("ERROR"));
}

#[test]
fn event_epochs() {
    let (_db, mut c) = setup_schema();
    seed(&mut c);
    let epochs: Vec<i64> = c.query(EVENT_EPOCHS, &[]).expect("EventEpochs").iter().map(|r| r.get(1)).collect();
    let b = base().as_second();
    assert_eq!(epochs, vec![b, b + 20 * 60, b + 2 * 3600, b + 3 * 3600]);
}

#[test]
fn events_per_hour() {
    let (_db, mut c) = setup_schema();
    seed(&mut c);
    let got: Vec<(Timestamp, i64)> = c.query(EVENTS_PER_HOUR, &[]).expect("EventsPerHour").iter().map(|r| (r.get(0), r.get(1))).collect();
    let at = |h| utc_ts(2024, 7, 15, h, 0, 0);
    assert_eq!(got, vec![(at(9), 2), (at(11), 1), (at(12), 1)]);
}
