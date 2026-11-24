use crate::support::{setup, utc_ts};
use jiff::{SignedDuration, Timestamp};
use std::collections::HashMap;

fn distinct_on_setup(c: &mut postgres::Client) {
    c.batch_execute("CREATE TABLE events (id int PRIMARY KEY, user_id int NOT NULL, kind text NOT NULL, ts timestamptz NOT NULL)")
        .expect("create");
    let now = utc_ts(2024, 7, 15, 12, 0, 0);
    for (id, user, kind, minutes) in
        [(1i32, 1i32, "login", 0), (2, 1, "login", 1), (3, 1, "logout", 2), (4, 2, "login", 3), (5, 2, "login", 4)]
    {
        let ts: Timestamp = now + SignedDuration::from_mins(minutes);
        c.execute("INSERT INTO events (id, user_id, kind, ts) VALUES ($1, $2, $3, $4)", &[&id, &user, &kind, &ts]).expect("insert");
    }
}

#[test]
fn distinct_on_latest_per_user() {
    let (_db, mut c) = setup();
    distinct_on_setup(&mut c);
    let rows = c.query("SELECT DISTINCT ON (user_id) user_id, id, kind FROM events ORDER BY user_id, ts DESC", &[]).expect("query");
    let got: HashMap<i32, (i32, String)> = rows.iter().map(|r| (r.get(0), (r.get(1), r.get(2)))).collect();
    assert_eq!(rows.len(), 2);
    // User 1's most recent event is id=3 (logout at +2m); user 2's is id=5.
    assert_eq!(got[&1], (3, "logout".to_owned()));
    assert_eq!(got[&2], (5, "login".to_owned()));
}

#[test]
fn distinct_on_keeps_first_seen() {
    let (_db, mut c) = setup();
    distinct_on_setup(&mut c);
    let rows = c.query("SELECT DISTINCT ON (kind) kind, id FROM events ORDER BY kind, id", &[]).expect("query");
    let got: Vec<(String, i32)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(got, [("login".to_owned(), 1), ("logout".to_owned(), 3)]);
}

#[test]
fn distinct_plain_still_works() {
    let (_db, mut c) = setup();
    distinct_on_setup(&mut c);
    let rows = c.query("SELECT DISTINCT kind FROM events ORDER BY kind", &[]).expect("query");
    let got: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(got, ["login", "logout"]);
}
