//! Roles and privileges: CREATE / ALTER / DROP ROLE, memberships, GRANT / REVOKE with
//! enforcement under SET ROLE, default privileges, ownership, DROP OWNED. Roles are
//! cluster-wide on PostgreSQL: names are unique to this module and dropped at the end.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

const ROLES: &[&str] = &["itp_reader", "itp_writer", "itp_login", "itp_child"];

fn drop_roles(c: &mut postgres::Client) {
    for r in ROLES {
        let _ = c.batch_execute(&format!("DROP OWNED BY {r}"));
        let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {r}"));
    }
}

#[test]
fn roles_grants_and_enforcement() {
    let (_db, mut c) = setup();
    drop_roles(&mut c);
    c.batch_execute(
        "CREATE ROLE itp_reader; CREATE ROLE itp_writer NOLOGIN;
         CREATE ROLE itp_login WITH LOGIN PASSWORD 'secret' CONNECTION LIMIT 5; CREATE ROLE itp_child IN ROLE itp_reader",
    )
    .expect("roles");
    let e = expect_code(c.batch_execute("CREATE ROLE itp_reader"), "42710");
    assert_eq!(message(&e), "role \"itp_reader\" already exists");
    assert_eq!(
        rows(&mut c, "SELECT rolname, rolcanlogin, rolsuper, rolconnlimit FROM pg_roles WHERE rolname LIKE 'itp\\_%' ORDER BY rolname"),
        [["itp_child", "f", "f", "-1"], ["itp_login", "t", "f", "5"], ["itp_reader", "f", "f", "-1"], ["itp_writer", "f", "f", "-1"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT pg_has_role('itp_child', 'itp_reader', 'MEMBER'), pg_has_role('itp_reader', 'itp_child', 'MEMBER')"),
        [["t", "f"]]
    );
    c.batch_execute(
        "CREATE TABLE docs (id int, body text); CREATE TABLE secret (id int);
         GRANT SELECT ON docs TO itp_reader; GRANT INSERT, UPDATE ON TABLE docs TO itp_writer;
         ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO itp_reader; CREATE TABLE later (id int)",
    )
    .expect("grants");
    assert_eq!(
        rows(
            &mut c,
            "SELECT has_table_privilege('itp_reader', 'docs', 'SELECT'), has_table_privilege('itp_reader', 'docs', 'INSERT'), has_table_privilege('itp_child', 'docs', 'SELECT'), has_table_privilege('itp_reader', 'later', 'SELECT')"
        ),
        [["t", "f", "t", "t"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT privilege_type FROM information_schema.role_table_grants WHERE grantee = 'itp_writer' AND table_name = 'docs' ORDER BY 1"
        ),
        [["INSERT"], ["UPDATE"]]
    );
    let e = expect_code(c.batch_execute("GRANT USAGE ON docs TO itp_reader"), "0LP01");
    assert_eq!(message(&e), "invalid privilege type USAGE for table");
    let e = expect_code(c.batch_execute("GRANT SELECT ON docs TO itp_nobody"), "42704");
    assert_eq!(message(&e), "role \"itp_nobody\" does not exist");
    // Privileges are enforced for the current role.
    let e = expect_code(c.batch_execute("SET ROLE itp_nobody"), "22023");
    assert_eq!(message(&e), "role \"itp_nobody\" does not exist");
    c.batch_execute("SET ROLE itp_child").expect("SET ROLE");
    assert_eq!(rows(&mut c, "SELECT current_user, session_user = current_user"), [["itp_child", "f"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM docs"), [["0"]]);
    let e = expect_code(c.batch_execute("SELECT * FROM secret"), "42501");
    assert_eq!(message(&e), "permission denied for table secret");
    let e = expect_code(c.batch_execute("INSERT INTO docs VALUES (1, 'x')"), "42501");
    assert_eq!(message(&e), "permission denied for table docs");
    c.batch_execute("RESET ROLE").expect("RESET ROLE");
    assert_eq!(rows(&mut c, "SELECT current_user = session_user"), [["t"]]);
    c.batch_execute("REVOKE SELECT ON docs FROM itp_reader").expect("revoke");
    assert_eq!(rows(&mut c, "SELECT has_table_privilege('itp_reader', 'docs', 'SELECT')"), [["f"]]);
    // A role with privileges or objects cannot be dropped until they are gone.
    let e = expect_code(c.batch_execute("DROP ROLE itp_writer"), "2BP01");
    assert_eq!(message(&e), "role \"itp_writer\" cannot be dropped because some objects depend on it");
    c.batch_execute("ALTER TABLE secret OWNER TO itp_writer").expect("owner");
    c.batch_execute("DROP OWNED BY itp_writer; DROP ROLE itp_writer").expect("DROP OWNED");
    expect_code(c.batch_execute("SELECT * FROM secret"), "42P01");
    let e = expect_code(c.batch_execute("DROP ROLE itp_writer"), "42704");
    assert_eq!(message(&e), "role \"itp_writer\" does not exist");
    c.batch_execute("ALTER ROLE itp_login WITH NOLOGIN; ALTER ROLE itp_login SET search_path = public").expect("alter role");
    assert_eq!(rows(&mut c, "SELECT rolcanlogin FROM pg_roles WHERE rolname = 'itp_login'"), [["f"]]);
    drop_roles(&mut c);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_roles WHERE rolname LIKE 'itp\\_%'"), [["0"]]);
}
