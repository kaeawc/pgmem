//! `inet_server_addr()`, `inet_server_port()`, `inet_client_addr()`, `inet_client_port()`: the
//! connection's TCP addresses, NULL without a socket.
//!
//! Real Postgres may run behind NAT (a container's mapped port), so the exact address and port
//! are only asserted on pgmem; the shape (types, inet display, families) is asserted on both.

use crate::support::{expect_code, message, setup};
use pgmem::Embedded;

#[test]
fn tcp_connection_addresses_and_ports() {
    let (db, mut c) = setup();
    let row = c
        .query_one(
            "SELECT host(inet_server_addr()), inet_server_port(), host(inet_client_addr()), inet_client_port(), \
             pg_typeof(inet_server_addr())::text, pg_typeof(inet_client_addr())::text, \
             pg_typeof(inet_server_port())::text, pg_typeof(inet_client_port())::text",
            &[],
        )
        .expect("query");
    let server_host: String = row.get(0);
    let server_port: i32 = row.get(1);
    let client_host: String = row.get(2);
    let client_port: i32 = row.get(3);
    assert!(server_host.parse::<std::net::IpAddr>().is_ok(), "{server_host}");
    assert!(client_host.parse::<std::net::IpAddr>().is_ok(), "{client_host}");
    assert!((1..=65535).contains(&server_port), "{server_port}");
    assert!((1..=65535).contains(&client_port), "{client_port}");
    let types: Vec<String> = (4..8).map(|i| row.get(i)).collect();
    assert_eq!(types, ["inet", "inet", "integer", "integer"]);
    if let Some(server) = db.server() {
        // Connected to 127.0.0.1: the server side is the listening address and port.
        assert_eq!(server_host, server.addr().ip().to_string());
        assert!(server_host == "127.0.0.1" || server_host == "::1", "{server_host}");
        assert_eq!(server_port, i32::from(server.port()));
        assert!(client_host == "127.0.0.1" || client_host == "::1", "{client_host}");
        assert_ne!(client_port, server_port);
    }
}

#[test]
fn addresses_are_host_inets() {
    let (_db, mut c) = setup();
    // inet text output shows the netmask (/32, /128); host() and the client's display drop it.
    let row = c
        .query_one(
            "SELECT inet_server_addr()::text = host(inet_server_addr()) || '/' || masklen(inet_server_addr()), \
             masklen(inet_server_addr()) = CASE family(inet_server_addr()) WHEN 4 THEN 32 ELSE 128 END, \
             masklen(inet_client_addr()) = CASE family(inet_client_addr()) WHEN 4 THEN 32 ELSE 128 END, \
             family(inet_server_addr()) = family(inet_client_addr())",
            &[],
        )
        .expect("query");
    for i in 0..4 {
        assert!(row.get::<_, bool>(i), "column {i}");
    }
    let shown = c.simple_query("SELECT inet_server_addr(), host(inet_server_addr())").expect("query");
    let r = shown.iter().find_map(|m| if let postgres::SimpleQueryMessage::Row(r) = m { Some(r) } else { None }).expect("query");
    assert_eq!(r.get(0), r.get(1), "inet output omits a full-length netmask");
}

#[test]
fn stable_per_connection_and_not_immutable() {
    let (db, mut c) = setup();
    let a: i32 = c.query_one("SELECT inet_client_port()", &[]).expect("query").get(0);
    let b: i32 = c.query_one("SELECT inet_client_port()", &[]).expect("query").get(0);
    assert_eq!(a, b);
    let mut other = db.connect();
    let o: i32 = other.query_one("SELECT inet_client_port()", &[]).expect("query").get(0);
    assert_ne!(a, o, "each connection has its own client port");
    // Stable, not immutable: a generation expression may not call them.
    for f in ["inet_server_addr()", "inet_server_port()", "inet_client_addr()", "inet_client_port()"] {
        let e = expect_code(c.batch_execute(&format!("CREATE TABLE g (a int, b text GENERATED ALWAYS AS ({f}::text) STORED)")), "42P17");
        assert_eq!(message(&e), "generation expression is not immutable", "{f}");
    }
}

#[test]
fn embedded_session_has_no_socket() {
    let e = Embedded::new();
    let mut s = e.session();
    let out = s
        .query(
            "SELECT inet_server_addr() IS NULL, inet_server_port() IS NULL, inet_client_addr() IS NULL, \
             inet_client_port() IS NULL, host(inet_server_addr()) IS NULL",
        )
        .expect("query");
    assert_eq!(out.rows, vec![vec![Some("t".to_owned()); 5]]);
    let mut f = e.fork().session();
    let out = f.query("SELECT inet_client_addr() IS NULL AND inet_server_port() IS NULL").expect("query");
    assert_eq!(out.rows, vec![vec![Some("t".to_owned())]]);
}
