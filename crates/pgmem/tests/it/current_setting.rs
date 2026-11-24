use crate::support::{expect_code, setup};

#[test]
fn current_setting_known() {
    let (_db, mut c) = setup();
    let mut setting =
        |name: &str| -> String { c.query_one("SELECT current_setting($1)", &[&name]).unwrap_or_else(|e| panic!("{name}: {e}")).get(0) };
    // Deviation from pgmem-go: Go expected server_version "16.0", search_path "public" and TimeZone exactly "UTC"; a
    // Postgres 18 server reports "18.x" and the default search_path `"$user", public`.
    let version = setting("server_version");
    assert!(version.starts_with("18."), "server_version: {version}");
    assert_eq!(setting("search_path"), r#""$user", public"#);
    // The default TimeZone comes from the server's postgresql.conf: initdb writes the host zone
    // (e.g. "Etc/UTC"); either spelling of UTC is accepted.
    let tz = setting("timezone");
    assert!(tz == "UTC" || tz == "Etc/UTC", "timezone: {tz}");
    assert_eq!(setting("client_encoding"), "UTF8");
}

#[test]
fn current_setting_unknown_errors() {
    let (_db, mut c) = setup();
    expect_code(c.query_one("SELECT current_setting('does_not_exist')", &[]), "42704");
}

#[test]
fn current_setting_missing_ok() {
    let (_db, mut c) = setup();
    let got: Option<String> = c.query_one("SELECT current_setting('does_not_exist', true)", &[]).expect("query").get(0);
    assert_eq!(got, None);
}

#[test]
fn version() {
    let (db, mut c) = setup();
    let got: String = c.query_one("SELECT version()", &[]).expect("query").get(0);
    assert!(got.starts_with("PostgreSQL "), "got {got:?}");
    // The pgmem marker is pgmem-specific; real Postgres has its own build string.
    if db.server().is_some() {
        assert!(got.contains("pgmem"), "got {got:?}");
    }
}
