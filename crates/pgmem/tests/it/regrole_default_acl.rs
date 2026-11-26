//! `regrole`, `pg_default_acl` (ALTER DEFAULT PRIVILEGES stored and applied) and `pg_policies`.
//! Roles are cluster-wide on PostgreSQL: names are unique to this module and dropped at the end.

use crate::support::{expect_code, message, setup, text_value, texts};

/// Roles are cluster-wide: the tests of this module run one at a time.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Rows as text; the session user (`postgres` on PostgreSQL) reads as `ME`.
fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    let me = text_value(c, "SELECT current_user").unwrap_or_default();
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into()).replace(&me, "ME")).collect()).collect()
}

fn cleanup(c: &mut postgres::Client) {
    for r in ["itr_a", "\"ITR b-2\"", "itr_c"] {
        let _ = c.batch_execute(&format!("DROP OWNED BY {r}"));
        let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {r}"));
    }
    let _ = c.batch_execute("DROP SCHEMA IF EXISTS itr_s CASCADE");
}

#[test]
fn regrole_input_output_and_casts() {
    let _lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_db, mut c) = setup();
    cleanup(&mut c);
    c.batch_execute("CREATE ROLE itr_a; CREATE ROLE \"ITR b-2\"").expect("roles");
    assert_eq!(
        rows(&mut c, "SELECT current_user::text::regrole, '-'::regrole, '\"ITR b-2\"'::regrole, initcap(current_user::text)::regrole"),
        [["ME", "-", "\"ITR b-2\"", "ME"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT 10::regrole, 0::regrole, 424242::regrole, 424242::oid::regrole::text, '10'::regrole"),
        [["ME", "-", "424242", "424242", "ME"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT to_regrole('nope') IS NULL, to_regrole('itr_a'), to_regrole('\"ITR b-2\"'), to_regrole('public') IS NULL, to_regrole('a.b') IS NULL"
        ),
        [["t", "itr_a", "\"ITR b-2\"", "t", "t"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT 'itr_a'::regrole::oid = (SELECT oid FROM pg_roles WHERE rolname = 'itr_a'), (SELECT oid FROM pg_roles WHERE rolname = 'itr_a')::regrole, pg_typeof('itr_a'::regrole)"
        ),
        [["t", "itr_a", "regrole"]]
    );
    assert_eq!(rows(&mut c, "SELECT oid, typname, typarray FROM pg_type WHERE typname = 'regrole'"), [["4096", "regrole", "4097"]]);
    let e = expect_code(c.simple_query("SELECT 'nosuch_itr'::regrole"), "42704");
    assert_eq!(message(&e), "role \"nosuch_itr\" does not exist");
    let e = expect_code(c.simple_query("SELECT 'public'::regrole"), "42704");
    assert_eq!(message(&e), "role \"public\" does not exist");
    let e = expect_code(c.simple_query("SELECT 'a.b'::regrole"), "42602");
    assert_eq!(message(&e), "invalid name syntax");
    cleanup(&mut c);
}

#[test]
fn regrole_in_catalog_queries() {
    let _lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_db, mut c) = setup();
    cleanup(&mut c);
    c.batch_execute("CREATE ROLE itr_a; CREATE SCHEMA itr_s; CREATE TABLE itr_s.owned (a int); CREATE TABLE itr_s.mine (a int); ALTER TABLE itr_s.owned OWNER TO itr_a")
        .expect("setup");
    assert_eq!(
        rows(&mut c, "SELECT relname, relowner::regrole FROM pg_class WHERE relnamespace = 'itr_s'::regnamespace ORDER BY 1"),
        [["mine", "ME"], ["owned", "itr_a"]]
    );
    cleanup(&mut c);
}

#[test]
fn default_acl_is_stored_and_normalised() {
    let _lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_db, mut c) = setup();
    cleanup(&mut c);
    c.batch_execute("CREATE ROLE itr_a; CREATE ROLE \"ITR b-2\"").expect("roles");
    let acl = "SELECT defaclrole::regrole, defaclnamespace::regnamespace, defaclobjtype, defaclacl FROM pg_default_acl ORDER BY 1, 3, 2";
    c.batch_execute("ALTER DEFAULT PRIVILEGES FOR ROLE itr_a GRANT SELECT ON TABLES TO \"ITR b-2\"").expect("grant");
    assert_eq!(rows(&mut c, acl), [["itr_a", "-", "r", "{itr_a=arwdDxtm/itr_a,\"\\\"ITR b-2\\\"=r/itr_a\"}"]]);
    assert_eq!(rows(&mut c, "SELECT oid > 0 FROM pg_default_acl"), [["t"]]);
    // Revoking back to the hard-wired default removes the entry.
    c.batch_execute("ALTER DEFAULT PRIVILEGES FOR ROLE itr_a REVOKE SELECT ON TABLES FROM \"ITR b-2\"").expect("revoke");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_default_acl"), [["0"]]);
    c.batch_execute(
        "ALTER DEFAULT PRIVILEGES FOR ROLE itr_a GRANT SELECT ON TABLES TO \"ITR b-2\" WITH GRANT OPTION;
         ALTER DEFAULT PRIVILEGES FOR ROLE itr_a IN SCHEMA public GRANT ALL ON SEQUENCES TO PUBLIC;
         ALTER DEFAULT PRIVILEGES FOR ROLE itr_a REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES FOR ROLE itr_a GRANT USAGE ON TYPES TO itr_a;
         ALTER DEFAULT PRIVILEGES FOR ROLE itr_a GRANT CREATE ON SCHEMAS TO itr_a",
    )
    .expect("more");
    assert_eq!(
        rows(&mut c, acl),
        [
            ["itr_a", "public", "S", "{=rwU/itr_a}"],
            ["itr_a", "-", "f", "{itr_a=X/itr_a}"],
            ["itr_a", "-", "r", "{itr_a=arwdDxtm/itr_a,\"\\\"ITR b-2\\\"=r*/itr_a\"}"],
        ]
    );
    c.batch_execute("ALTER DEFAULT PRIVILEGES FOR ROLE itr_a GRANT EXECUTE ON FUNCTIONS TO PUBLIC").expect("restore functions");
    assert_eq!(rows(&mut c, "SELECT defaclobjtype FROM pg_default_acl ORDER BY 1"), [["S"], ["r"]]);
    // The owner's own privileges can be revoked from the global entry, and come back.
    c.batch_execute("ALTER DEFAULT PRIVILEGES FOR ROLE itr_a REVOKE ALL ON TABLES FROM itr_a").expect("revoke owner");
    assert_eq!(rows(&mut c, "SELECT defaclacl FROM pg_default_acl WHERE defaclobjtype = 'r'"), [["{\"\\\"ITR b-2\\\"=r*/itr_a\"}"]]);
    c.batch_execute("ALTER DEFAULT PRIVILEGES FOR ROLE itr_a GRANT ALL ON TABLES TO itr_a").expect("regrant owner");
    assert_eq!(
        rows(&mut c, "SELECT defaclacl FROM pg_default_acl WHERE defaclobjtype = 'r'"),
        [["{itr_a=arwdDxtm/itr_a,\"\\\"ITR b-2\\\"=r*/itr_a\"}"]]
    );
    // Without FOR ROLE the entry belongs to the current role.
    c.batch_execute("ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO itr_a").expect("current role");
    assert_eq!(rows(&mut c, "SELECT defaclacl FROM pg_default_acl WHERE defaclrole = current_user::text::regrole"), [["{itr_a=r/ME}"]]);
    // Only a member of the role (or a superuser) may change its defaults.
    let me = text_value(&mut c, "SELECT current_user").unwrap_or_default();
    c.batch_execute("CREATE ROLE itr_c").expect("role");
    c.batch_execute("SET ROLE itr_c").expect("set role");
    let e = expect_code(c.batch_execute(&format!("ALTER DEFAULT PRIVILEGES FOR ROLE {me} GRANT SELECT ON TABLES TO itr_c")), "42501");
    assert_eq!(message(&e), "permission denied to change default privileges");
    c.batch_execute("RESET ROLE").expect("reset");
    // DROP OWNED removes the role's entries and the privileges it was granted.
    c.batch_execute("DROP OWNED BY itr_a").expect("drop owned");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_default_acl"), [["0"]]);
    cleanup(&mut c);
}

#[test]
fn default_acl_is_applied_to_new_objects() {
    let _lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_db, mut c) = setup();
    cleanup(&mut c);
    c.batch_execute(
        "CREATE ROLE itr_a; CREATE ROLE \"ITR b-2\"; CREATE SCHEMA itr_s;
         GRANT USAGE, CREATE ON SCHEMA itr_s TO itr_a;
         ALTER DEFAULT PRIVILEGES FOR ROLE itr_a IN SCHEMA itr_s GRANT SELECT, INSERT ON TABLES TO \"ITR b-2\";
         ALTER DEFAULT PRIVILEGES FOR ROLE itr_a GRANT SELECT ON TABLES TO PUBLIC;
         ALTER DEFAULT PRIVILEGES FOR ROLE itr_a GRANT USAGE ON SEQUENCES TO \"ITR b-2\";
         ALTER DEFAULT PRIVILEGES FOR ROLE itr_a IN SCHEMA itr_s GRANT UPDATE ON SEQUENCES TO \"ITR b-2\";
         SET ROLE itr_a;
         CREATE TABLE itr_s.t1 (a serial);
         CREATE VIEW itr_s.v1 AS SELECT 1 AS x;
         RESET ROLE;
         CREATE TABLE itr_s.by_me (a int)",
    )
    .expect("setup");
    assert_eq!(
        rows(
            &mut c,
            "SELECT relname, relowner::regrole, relacl FROM pg_class WHERE relnamespace = 'itr_s'::regnamespace AND relkind IN ('r', 'v', 'S') ORDER BY 1"
        ),
        [
            ["by_me", "ME", "NULL"],
            ["t1", "itr_a", "{=r/itr_a,itr_a=arwdDxtm/itr_a,\"\\\"ITR b-2\\\"=ar/itr_a\"}"],
            ["t1_a_seq", "itr_a", "{itr_a=rwU/itr_a,\"\\\"ITR b-2\\\"=wU/itr_a\"}"],
            ["v1", "itr_a", "{=r/itr_a,itr_a=arwdDxtm/itr_a,\"\\\"ITR b-2\\\"=ar/itr_a\"}"],
        ]
    );
    cleanup(&mut c);
}

#[test]
fn pg_policies_exists_with_postgres_columns() {
    let (_db, mut c) = setup();
    let stmt = c.prepare("SELECT * FROM pg_policies").expect("prepare");
    let cols: Vec<(String, String)> = stmt.columns().iter().map(|col| (col.name().to_owned(), col.type_().name().to_owned())).collect();
    let want = [
        ("schemaname", "name"),
        ("tablename", "name"),
        ("policyname", "name"),
        ("permissive", "text"),
        ("roles", "_name"),
        ("cmd", "text"),
        ("qual", "text"),
        ("with_check", "text"),
    ];
    assert_eq!(cols, want.map(|(n, t)| (n.to_owned(), t.to_owned())));
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_policies"), [["0"]]);
}
