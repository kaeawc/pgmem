use crate::support::{TestDb, setup, utc_ts};
use jiff::Timestamp;

fn utc(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> Timestamp {
    utc_ts(y, mo, d, h, mi, 0)
}

/// `now()` pinned to 2024-07-15 12:00:00 UTC on pgmem; on real Postgres (whose clock cannot be
/// pinned) the same instant as a literal, so every assertion is still checked there.
fn pinned_now(db: &TestDb) -> &'static str {
    match db.server() {
        Some(server) => {
            server.set_now(Some(utc(2024, 7, 15, 12, 0).into()));
            "now()"
        }
        None => "'2024-07-15 12:00:00+00'::timestamptz",
    }
}

#[test]
fn interval_add_to_timestamp() {
    let (db, mut c) = setup();
    let now = pinned_now(&db);
    let got: Timestamp = c.query_one(&format!("SELECT {now} + interval '1 day'"), &[]).expect("query").get(0);
    assert_eq!(got, utc(2024, 7, 16, 12, 0));
}

#[test]
fn interval_subtract_from_timestamp() {
    let (db, mut c) = setup();
    let now = pinned_now(&db);
    let got: Timestamp = c.query_one(&format!("SELECT {now} - interval '5 hours'"), &[]).expect("query").get(0);
    assert_eq!(got, utc(2024, 7, 15, 7, 0));
}

#[test]
fn interval_multi_unit() {
    let (db, mut c) = setup();
    let now = pinned_now(&db);
    let got: Timestamp = c.query_one(&format!("SELECT {now} + interval '2 days 3 hours 15 minutes'"), &[]).expect("query").get(0);
    assert_eq!(got, utc(2024, 7, 17, 15, 15));
}

#[test]
fn interval_as_filter() {
    let (db, mut c) = setup();
    let now = pinned_now(&db);
    c.batch_execute("CREATE TABLE events (id int PRIMARY KEY, at timestamptz NOT NULL)").expect("create");
    c.execute(
        "INSERT INTO events (id, at) VALUES (1, $1), (2, $2), (3, $3)",
        &[
            &utc(2024, 7, 14, 12, 0), // 1 day before pinned now
            &utc(2024, 7, 15, 6, 0),  // 6h before
            &utc(2024, 7, 15, 13, 0), // 1h after
        ],
    )
    .expect("insert");
    let rows = c.query(&format!("SELECT id FROM events WHERE at > {now} - interval '1 day' ORDER BY id"), &[]).expect("query");
    let ids: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(ids, vec![2, 3]);
}
