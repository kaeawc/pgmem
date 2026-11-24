use crate::support::{TestDb, utc_ts};
use jiff::{SignedDuration, Timestamp};
use postgres::{Client, NoTls};
use std::time::SystemTime;

fn utc(y: i16, mo: i8, d: i8, h: i8) -> Timestamp {
    utc_ts(y, mo, d, h, 0, 0)
}

fn now(c: &mut Client) -> Timestamp {
    c.query_one("SELECT now()", &[]).expect("SELECT now").get(0)
}

#[test]
fn set_now_pins_clock_for_builtins() {
    let db = TestDb::new();
    let Some(server) = db.server() else { return };
    let pinned = utc(2025, 6, 15, 12);
    server.set_now(Some(SystemTime::from(pinned)));
    assert_eq!(now(&mut db.connect()), pinned);
}

#[test]
fn set_now_zero_time_reverts_to_wall_clock() {
    let db = TestDb::new();
    let Some(server) = db.server() else { return };
    server.set_now(Some(SystemTime::from(utc(2025, 6, 15, 12))));
    server.set_now(None);
    let mut c = db.connect();
    // now() has microsecond precision; truncate the lower bound accordingly.
    let before = Timestamp::now() - SignedDuration::from_micros(1);
    let got = now(&mut c);
    let after = Timestamp::now();
    assert!(got >= before && got <= after + SignedDuration::from_secs(1), "now() after unpin: {got} not in [{before}, {after}]");
}

#[test]
fn set_now_two_servers_independent() {
    if TestDb::new().is_real_postgres() {
        return;
    }
    let a = pgmem::Server::start().expect("start A");
    let b = pgmem::Server::start().expect("start B");
    let (ta, tb) = (utc(2020, 1, 1, 0), utc(2030, 1, 1, 0));
    a.set_now(Some(SystemTime::from(ta)));
    b.set_now(Some(SystemTime::from(tb)));
    for (server, want) in [(&a, ta), (&b, tb)] {
        let mut c = Client::connect(&server.conninfo(), NoTls).expect("connect");
        assert_eq!(now(&mut c), want);
    }
}
