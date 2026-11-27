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

const COLUMN_ROLES: &[&str] = &["itc_reader", "itc_writer"];
const ENFORCE_ROLES: &[&str] = &["itce_reader", "itce_table"];

fn drop_column_roles(c: &mut postgres::Client) {
    for r in COLUMN_ROLES {
        let _ = c.batch_execute(&format!("DROP OWNED BY {r}"));
        let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {r}"));
    }
}

fn drop_enforce_roles(c: &mut postgres::Client) {
    for r in ENFORCE_ROLES {
        let _ = c.batch_execute(&format!("DROP OWNED BY {r}"));
        let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {r}"));
    }
}

fn attacl(c: &mut postgres::Client, table: &str) -> Vec<Vec<String>> {
    rows(c, &format!("SELECT attname, attacl::text FROM pg_attribute WHERE attrelid = '{table}'::regclass AND attnum > 0 ORDER BY attnum"))
}

/// GRANT / REVOKE with a column list set `pg_attribute.attacl`, not `relacl` (#156).
#[test]
fn column_privileges_are_recorded_per_column() {
    let (_db, mut c) = setup();
    drop_column_roles(&mut c);
    let me = rows(&mut c, "SELECT session_user::text")[0][0].clone();
    c.batch_execute(
        "CREATE ROLE itc_reader; CREATE ROLE itc_writer;
         CREATE TABLE cp (id int PRIMARY KEY, email text, secret text);
         GRANT SELECT (id, email) ON cp TO itc_reader",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT relacl IS NULL FROM pg_class WHERE relname = 'cp'"), [["t"]]);
    assert_eq!(
        attacl(&mut c, "cp"),
        [
            vec!["id".to_owned(), format!("{{itc_reader=r/{me}}}")],
            vec!["email".to_owned(), format!("{{itc_reader=r/{me}}}")],
            vec!["secret".to_owned(), "NULL".to_owned()]
        ]
    );
    c.batch_execute(
        "GRANT INSERT (id, email), UPDATE (email), REFERENCES (id) ON cp TO itc_reader;
         GRANT SELECT (id) ON cp TO PUBLIC; GRANT UPDATE (secret) ON cp TO itc_writer WITH GRANT OPTION",
    )
    .expect("more grants");
    assert_eq!(
        attacl(&mut c, "cp"),
        [
            vec!["id".to_owned(), format!("{{itc_reader=arx/{me},=r/{me}}}")],
            vec!["email".to_owned(), format!("{{itc_reader=arw/{me}}}")],
            vec!["secret".to_owned(), format!("{{itc_writer=w*/{me}}}")]
        ]
    );
    assert_eq!(rows(&mut c, "SELECT relacl IS NULL FROM pg_class WHERE relname = 'cp'"), [["t"]]);
    assert_eq!(
        rows(
            &mut c,
            "SELECT grantee, column_name, privilege_type, is_grantable FROM information_schema.column_privileges
             WHERE table_name = 'cp' AND grantee <> current_user ORDER BY 1, 2, 3"
        ),
        [
            ["PUBLIC", "id", "SELECT", "NO"],
            ["itc_reader", "email", "INSERT", "NO"],
            ["itc_reader", "email", "SELECT", "NO"],
            ["itc_reader", "email", "UPDATE", "NO"],
            ["itc_reader", "id", "INSERT", "NO"],
            ["itc_reader", "id", "REFERENCES", "NO"],
            ["itc_reader", "id", "SELECT", "NO"],
            ["itc_writer", "secret", "UPDATE", "YES"],
        ]
    );
    // The owner holds every column privilege through the table's (default) ACL.
    assert_eq!(
        rows(
            &mut c,
            "SELECT column_name, string_agg(privilege_type || ':' || is_grantable, ',' ORDER BY privilege_type)
             FROM information_schema.column_privileges WHERE table_name = 'cp' AND grantee = current_user GROUP BY 1 ORDER BY 1"
        ),
        [
            ["email", "INSERT:YES,REFERENCES:YES,SELECT:YES,UPDATE:YES"],
            ["id", "INSERT:YES,REFERENCES:YES,SELECT:YES,UPDATE:YES"],
            ["secret", "INSERT:YES,REFERENCES:YES,SELECT:YES,UPDATE:YES"]
        ]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT grantor = current_user, column_name, privilege_type FROM information_schema.role_column_grants
             WHERE table_name = 'cp' AND grantee = 'itc_writer'"
        ),
        [["t", "secret", "UPDATE"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT has_column_privilege('itc_reader', 'cp', 'id', 'SELECT'), has_column_privilege('itc_reader', 'cp', 'secret', 'SELECT'),
                    has_column_privilege('itc_reader', 'cp'::regclass, 2::int2, 'UPDATE'), has_column_privilege('itc_reader', 'cp', 99::int2, 'SELECT'),
                    has_column_privilege('itc_writer', 'cp', 'secret', 'UPDATE WITH GRANT OPTION'), has_column_privilege('itc_writer', 'cp', 'id', 'SELECT'),
                    has_column_privilege('cp', 'secret', 'SELECT, UPDATE'), has_any_column_privilege('itc_reader', 'cp', 'SELECT'),
                    has_any_column_privilege('itc_writer', 'cp', 'INSERT'), has_table_privilege('itc_reader', 'cp', 'SELECT')"
        ),
        [["t", "f", "t", "NULL", "t", "t", "t", "t", "f", "f"]]
    );
    let e = expect_code(c.batch_execute("SELECT has_column_privilege('itc_reader', 'cp', 'nope', 'SELECT')"), "42703");
    assert_eq!(message(&e), "column \"nope\" of relation \"cp\" does not exist");
    let e = expect_code(c.batch_execute("SELECT has_any_column_privilege('itc_reader', 'cp', 'DELETE')"), "22023");
    assert_eq!(message(&e), "unrecognized privilege type: \"DELETE\"");
    let e = expect_code(c.batch_execute("GRANT DELETE (id) ON cp TO itc_reader"), "0LP01");
    assert_eq!(message(&e), "invalid privilege type DELETE for column");
    let e = expect_code(c.batch_execute("GRANT SELECT (nope) ON cp TO itc_reader"), "42703");
    assert_eq!(message(&e), "column \"nope\" of relation \"cp\" does not exist");
    let e = expect_code(c.batch_execute("GRANT SELECT (id) ON SCHEMA public TO itc_reader"), "0LP01");
    assert_eq!(message(&e), "column privileges are only valid for relations");
    let e = expect_code(c.batch_execute("DROP ROLE itc_writer"), "2BP01");
    assert_eq!(e.as_db_error().and_then(|d| d.detail()), Some("privileges for column secret of table cp"));
    // REVOKE with a column list; ALL (columns) is SELECT, INSERT, UPDATE, REFERENCES.
    c.batch_execute("REVOKE SELECT (email) ON cp FROM itc_reader; REVOKE GRANT OPTION FOR UPDATE (secret) ON cp FROM itc_writer")
        .expect("column revoke");
    assert_eq!(
        attacl(&mut c, "cp"),
        [
            vec!["id".to_owned(), format!("{{itc_reader=arx/{me},=r/{me}}}")],
            vec!["email".to_owned(), format!("{{itc_reader=aw/{me}}}")],
            vec!["secret".to_owned(), format!("{{itc_writer=w/{me}}}")]
        ]
    );
    c.batch_execute("REVOKE ALL (id, email) ON cp FROM itc_reader; GRANT ALL (email) ON cp TO itc_writer").expect("ALL (columns)");
    assert_eq!(
        attacl(&mut c, "cp"),
        [
            vec!["id".to_owned(), format!("{{=r/{me}}}")],
            vec!["email".to_owned(), format!("{{itc_writer=arwx/{me}}}")],
            vec!["secret".to_owned(), format!("{{itc_writer=w/{me}}}")]
        ]
    );
    // A table-level grant is unchanged (relacl) and implies every column; a column REVOKE
    // leaves it alone, while a table-level REVOKE also revokes the column grants.
    c.batch_execute("GRANT SELECT ON cp TO itc_reader; REVOKE SELECT (id) ON cp FROM itc_reader").expect("table grant");
    assert_eq!(
        rows(&mut c, "SELECT relacl::text FROM pg_class WHERE relname = 'cp'"),
        [[format!("{{{me}=arwdDxtm/{me},itc_reader=r/{me}}}")]]
    );
    assert_eq!(rows(&mut c, "SELECT has_column_privilege('itc_reader', 'cp', 'secret', 'SELECT')"), [["t"]]);
    c.batch_execute("REVOKE UPDATE ON cp FROM itc_writer").expect("table revoke");
    assert_eq!(
        attacl(&mut c, "cp"),
        [
            vec!["id".to_owned(), format!("{{=r/{me}}}")],
            vec!["email".to_owned(), format!("{{itc_writer=arx/{me}}}")],
            vec!["secret".to_owned(), "NULL".to_owned()]
        ]
    );
    drop_column_roles(&mut c);
}

/// Column privileges are enforced: only the granted columns may be read, inserted or
/// updated; WHERE, RETURNING and ON CONFLICT read columns too (#156).
#[test]
fn column_privileges_are_enforced() {
    let (_db, mut c) = setup();
    drop_enforce_roles(&mut c);
    c.batch_execute(
        "CREATE ROLE itce_reader; CREATE ROLE itce_table;
         CREATE TABLE ce (id int PRIMARY KEY, email text, secret text); INSERT INTO ce VALUES (1, 'a', 's');
         CREATE TABLE ce2 (id int, note text); INSERT INTO ce2 VALUES (1, 'n');
         CREATE VIEW cev AS SELECT id, email, secret FROM ce;
         GRANT SELECT (id, email), INSERT (id, email), UPDATE (email) ON ce TO itce_reader;
         GRANT SELECT (id) ON ce2 TO itce_reader; GRANT SELECT (id) ON cev TO itce_reader;
         GRANT INSERT, UPDATE, DELETE ON ce TO itce_table",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itce_reader").expect("SET ROLE");
    let ok = |c: &mut postgres::Client, sql: &str| c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let denied = |c: &mut postgres::Client, sql: &str, rel: &str| {
        let e = expect_code(c.batch_execute(sql), "42501");
        assert_eq!(message(&e), format!("permission denied for {rel}"), "{sql}");
    };
    assert_eq!(rows(&mut c, "SELECT id, email FROM ce"), [["1", "a"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM ce"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT ce2.id FROM ce JOIN ce2 USING (id)"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT id FROM ce NATURAL JOIN ce2"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT (SELECT max(ce2.id) FROM ce2 WHERE ce2.id = ce.id) FROM ce"), [["1"]]);
    assert_eq!(rows(&mut c, "WITH w AS (SELECT id FROM ce) SELECT * FROM w"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT id FROM cev"), [["1"]]);
    assert_eq!(
        rows(&mut c, "SELECT has_column_privilege('ce', 'email', 'SELECT'), has_column_privilege('ce', 'secret', 'SELECT')"),
        [["t", "f"]]
    );
    for sql in [
        "SELECT * FROM ce",
        "SELECT secret FROM ce",
        "SELECT id FROM ce WHERE secret = 's'",
        "SELECT id FROM ce ORDER BY secret",
        "SELECT ce FROM ce",
        "SELECT id FROM (SELECT * FROM ce) s",
        "SELECT * FROM ce JOIN ce2 USING (id)",
        "SELECT ctid FROM ce",
        "SELECT x.secret FROM (ce JOIN ce2 USING (id)) AS x",
        "SELECT 1 FROM ce2 WHERE EXISTS (SELECT secret FROM ce)",
        "SELECT (SELECT ce.secret) FROM ce",
        "SELECT id FROM ce UNION SELECT length(secret) FROM ce",
        "SELECT row_to_json(ce) FROM ce",
        "WITH w AS (SELECT * FROM ce) SELECT id FROM w",
    ] {
        denied(&mut c, sql, "table ce");
    }
    // The extended protocol is checked the same way.
    assert_eq!(c.query("SELECT email FROM ce WHERE id = $1", &[&1i32]).expect("granted columns").len(), 1);
    let e = expect_code(c.query("SELECT id FROM ce WHERE secret = $1", &[&"s"]), "42501");
    assert_eq!(message(&e), "permission denied for table ce");
    denied(&mut c, "SELECT secret FROM cev", "view cev");
    ok(&mut c, "INSERT INTO ce (id, email) VALUES (2, 'b')");
    ok(&mut c, "INSERT INTO ce VALUES (3, 'c')");
    assert_eq!(rows(&mut c, "INSERT INTO ce (id) VALUES (4) RETURNING id"), [["4"]]);
    ok(&mut c, "INSERT INTO ce (id, email) VALUES (1, 'q') ON CONFLICT (id) DO UPDATE SET email = excluded.email");
    ok(&mut c, "INSERT INTO ce (id, email) VALUES (1, 'q') ON CONFLICT DO NOTHING");
    for sql in [
        "INSERT INTO ce VALUES (5, 'c', 'x')",
        "INSERT INTO ce (id, secret) VALUES (5, 'x')",
        "INSERT INTO ce (id) VALUES (5) RETURNING *",
        "INSERT INTO ce (id, email) VALUES (1, 'q') ON CONFLICT (id) DO UPDATE SET email = ce.secret",
    ] {
        denied(&mut c, sql, "table ce");
    }
    ok(&mut c, "UPDATE ce SET email = 'z' WHERE id = 1");
    ok(&mut c, "UPDATE ce SET email = email || '!'");
    for sql in [
        "UPDATE ce SET secret = 'z'",
        "UPDATE ce SET email = secret",
        "UPDATE ce SET email = 'y' WHERE secret = 's'",
        "UPDATE ce SET email = 'y' RETURNING secret",
        "DELETE FROM ce WHERE id = 1",
    ] {
        denied(&mut c, sql, "table ce");
    }
    // Table-level INSERT / UPDATE / DELETE without SELECT: statements that read columns fail.
    c.batch_execute("RESET ROLE; SET ROLE itce_table").expect("SET ROLE");
    ok(&mut c, "UPDATE ce SET email = 'x'");
    ok(&mut c, "INSERT INTO ce VALUES (6, 'f', 'g')");
    ok(&mut c, "INSERT INTO ce VALUES (6) ON CONFLICT DO NOTHING");
    for sql in [
        "UPDATE ce SET email = 'x' WHERE id = 1",
        "UPDATE ce SET email = 'x' RETURNING ctid",
        "DELETE FROM ce WHERE id = 1",
        "INSERT INTO ce VALUES (6) ON CONFLICT (id) DO NOTHING",
        "INSERT INTO ce VALUES (7) RETURNING id",
    ] {
        denied(&mut c, sql, "table ce");
    }
    ok(&mut c, "DELETE FROM ce");
    // REVOKE takes the column privilege away.
    c.batch_execute("RESET ROLE; REVOKE SELECT (email) ON ce FROM itce_reader; SET ROLE itce_reader").expect("revoke");
    assert_eq!(rows(&mut c, "SELECT id FROM ce"), Vec::<Vec<String>>::new());
    denied(&mut c, "SELECT email FROM ce", "table ce");
    c.batch_execute("RESET ROLE").expect("RESET ROLE");
    c.batch_execute("DROP VIEW cev").expect("drop view");
    drop_enforce_roles(&mut c);
}

const VIEW_ROLES: &[&str] = &["itcv_writer", "itcv_trig"];

fn drop_view_roles(c: &mut postgres::Client) {
    for r in VIEW_ROLES {
        let _ = c.batch_execute(&format!("DROP OWNED BY {r}"));
        let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {r}"));
    }
}

/// Column privileges on a view used as the INSERT / UPDATE target are checked against the
/// view's columns (the base table with the view owner's rights), also for a view with an
/// INSTEAD OF trigger (#156).
#[test]
fn column_privileges_on_view_targets() {
    let (_db, mut c) = setup();
    drop_view_roles(&mut c);
    c.batch_execute(
        "CREATE ROLE itcv_writer; CREATE ROLE itcv_trig;
         CREATE TABLE cvt (id int PRIMARY KEY, email text, secret text);
         INSERT INTO cvt VALUES (1, 'a', 's');
         CREATE VIEW cvv AS SELECT id, email FROM cvt;
         GRANT UPDATE (email), INSERT (id, email) ON cvv TO itcv_writer;
         CREATE VIEW cvw AS SELECT id, email || '!' AS e2 FROM cvt;
         CREATE FUNCTION cvw_f() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
         CREATE TRIGGER cvw_t INSTEAD OF INSERT OR UPDATE ON cvw FOR EACH ROW EXECUTE FUNCTION cvw_f();
         GRANT INSERT (id), UPDATE (e2) ON cvw TO itcv_trig",
    )
    .expect("setup");
    let ok = |c: &mut postgres::Client, sql: &str| c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let denied = |c: &mut postgres::Client, sql: &str, rel: &str| {
        let e = expect_code(c.batch_execute(sql), "42501");
        assert_eq!(message(&e), format!("permission denied for {rel}"), "{sql}");
    };
    c.batch_execute("SET ROLE itcv_writer").expect("SET ROLE");
    ok(&mut c, "UPDATE cvv SET email = 'x'");
    ok(&mut c, "INSERT INTO cvv (id, email) VALUES (5, 'q')");
    ok(&mut c, "INSERT INTO cvv VALUES (6, 'q')");
    ok(&mut c, "INSERT INTO cvv (id) VALUES (7)");
    for sql in [
        "UPDATE cvv SET id = 2",
        "UPDATE cvv SET email = 'y' WHERE id = 1",
        "UPDATE cvv SET email = 'z' RETURNING email",
        "INSERT INTO cvv (id) VALUES (8) RETURNING id",
        "DELETE FROM cvv",
    ] {
        denied(&mut c, sql, "view cvv");
    }
    c.batch_execute("RESET ROLE; GRANT SELECT (email) ON cvv TO itcv_writer; SET ROLE itcv_writer").expect("grant select");
    ok(&mut c, "UPDATE cvv SET email = 'y' WHERE email = 'x'");
    denied(&mut c, "UPDATE cvv SET email = 'y' WHERE id = 1", "view cvv");
    c.batch_execute("RESET ROLE; SET ROLE itcv_trig").expect("SET ROLE");
    ok(&mut c, "INSERT INTO cvw (id) VALUES (9)");
    ok(&mut c, "UPDATE cvw SET e2 = 'a'");
    denied(&mut c, "INSERT INTO cvw (id, e2) VALUES (9, 'a')", "view cvw");
    denied(&mut c, "UPDATE cvw SET id = 1", "view cvw");
    c.batch_execute("RESET ROLE; DROP VIEW cvw; DROP FUNCTION cvw_f(); DROP VIEW cvv").expect("cleanup");
    drop_view_roles(&mut c);
}

/// Default privileges cannot have column lists; the grant option of a column privilege is
/// reported by has_column_privilege; DROP COLUMN drops the column's grants (#156).
#[test]
fn column_privilege_edges() {
    let (_db, mut c) = setup();
    let _ = c.batch_execute("DROP OWNED BY itcx_writer");
    let _ = c.batch_execute("DROP ROLE IF EXISTS itcx_writer");
    let _ = c.batch_execute("DROP TABLE IF EXISTS cve");
    c.batch_execute("CREATE ROLE itcx_writer; CREATE TABLE cve (id int, email text, extra text)").expect("setup");
    for sql in [
        "ALTER DEFAULT PRIVILEGES GRANT SELECT (id) ON TABLES TO itcx_writer",
        "ALTER DEFAULT PRIVILEGES REVOKE SELECT (id) ON TABLES FROM itcx_writer",
        "ALTER DEFAULT PRIVILEGES GRANT SELECT (id), INSERT ON TABLES TO itcx_writer",
    ] {
        let e = expect_code(c.batch_execute(sql), "0LP01");
        assert_eq!(message(&e), "default privileges cannot be set for columns", "{sql}");
    }
    c.batch_execute("GRANT UPDATE (id) ON cve TO itcx_writer; GRANT UPDATE (email) ON cve TO itcx_writer WITH GRANT OPTION")
        .expect("grants");
    assert_eq!(
        rows(
            &mut c,
            "SELECT has_column_privilege('itcx_writer', 'cve', 'id', 'UPDATE WITH GRANT OPTION'),
                    has_column_privilege('itcx_writer', 'cve', 'id', 'UPDATE'),
                    has_column_privilege('itcx_writer', 'cve', 'email', 'UPDATE WITH GRANT OPTION'),
                    has_any_column_privilege('itcx_writer', 'cve', 'UPDATE WITH GRANT OPTION'),
                    has_column_privilege('itcx_writer', 'cve', 'id', 'SELECT, UPDATE WITH GRANT OPTION')"
        ),
        [["f", "t", "t", "t", "f"]]
    );
    // Once the column is dropped, nothing depends on the role.
    c.batch_execute(
        "REVOKE UPDATE (id, email) ON cve FROM itcx_writer; GRANT SELECT (extra) ON cve TO itcx_writer; ALTER TABLE cve DROP COLUMN extra",
    )
    .expect("drop column");
    c.batch_execute("DROP ROLE itcx_writer").expect("no grants left on the dropped column");
    c.batch_execute("DROP TABLE cve").expect("drop table");
}

fn drop_test_roles(c: &mut postgres::Client, roles: &[&str]) {
    let _ = c.batch_execute("RESET ROLE");
    for r in roles {
        let _ = c.batch_execute(&format!("DROP OWNED BY {r}"));
        let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {r}"));
    }
}

#[track_caller]
fn denied_with(c: &mut postgres::Client, sql: &str, msg: &str) {
    let e = expect_code(c.batch_execute(sql), "42501");
    assert_eq!(message(&e), msg, "{sql}");
}

const FN_OWNER_ROLES: &[&str] = &["itof_a", "itof_b", "itof_m"];

/// Only the owner of a function (a member of the owning role, or a superuser) may replace,
/// alter, comment on or drop it; OWNER TO also needs membership in the new owner (#234).
#[test]
fn function_ownership_is_enforced() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, FN_OWNER_ROLES);
    c.batch_execute(
        "CREATE ROLE itof_a; CREATE ROLE itof_b; CREATE ROLE itof_m IN ROLE itof_a;
         GRANT CREATE ON SCHEMA public TO itof_a, itof_b;
         SET ROLE itof_a;
         CREATE FUNCTION oaf() RETURNS int LANGUAGE sql AS 'select 1';
         CREATE PROCEDURE oap() LANGUAGE sql AS 'select 1';
         RESET ROLE",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itof_b").expect("SET ROLE");
    for (sql, msg) in [
        ("CREATE OR REPLACE FUNCTION oaf() RETURNS int LANGUAGE sql AS 'select 2'", "must be owner of function oaf"),
        ("ALTER FUNCTION oaf() SECURITY DEFINER", "must be owner of function oaf"),
        ("ALTER FUNCTION oaf() OWNER TO itof_b", "must be owner of function oaf"),
        ("ALTER FUNCTION oaf() RENAME TO oag", "must be owner of function oaf"),
        ("ALTER FUNCTION oaf() SET search_path = public", "must be owner of function oaf"),
        ("COMMENT ON FUNCTION oaf() IS 'x'", "must be owner of function oaf"),
        ("DROP FUNCTION oaf()", "must be owner of function oaf"),
        ("DROP FUNCTION IF EXISTS oaf()", "must be owner of function oaf"),
        ("DROP PROCEDURE oap()", "must be owner of procedure oap"),
        ("ALTER PROCEDURE oap() OWNER TO itof_b", "must be owner of function oap"),
    ] {
        denied_with(&mut c, sql, msg);
    }
    // A missing object is reported before ownership.
    let e = expect_code(c.batch_execute("DROP FUNCTION oazz()"), "42883");
    assert_eq!(message(&e), "function oazz() does not exist");
    // A member of the owning role acts as the owner; OWNER TO needs membership in the new owner.
    c.batch_execute("RESET ROLE; SET ROLE itof_m").expect("SET ROLE");
    c.batch_execute("ALTER FUNCTION oaf() SECURITY DEFINER; COMMENT ON FUNCTION oaf() IS 'mine'").expect("member alters");
    denied_with(&mut c, "ALTER FUNCTION oaf() OWNER TO itof_b", "must be able to SET ROLE \"itof_b\"");
    c.batch_execute("CREATE OR REPLACE FUNCTION oaf() RETURNS int LANGUAGE sql AS 'select 3'").expect("member replaces");
    // Superusers bypass the checks.
    c.batch_execute("RESET ROLE; ALTER FUNCTION oaf() OWNER TO itof_b").expect("superuser OWNER TO");
    c.batch_execute("SET ROLE itof_b; ALTER FUNCTION oaf() RENAME TO oag; DROP FUNCTION oag(); RESET ROLE").expect("new owner");
    c.batch_execute("SET ROLE itof_a; DROP PROCEDURE oap(); RESET ROLE").expect("owner drops");
    drop_test_roles(&mut c, FN_OWNER_ROLES);
}

const REL_OWNER_ROLES: &[&str] = &["itor_a", "itor_b", "itor_m"];

/// ALTER / DROP / COMMENT ON relations, indexes, triggers, types, domains and schemas need
/// ownership; the error names the object kind as PostgreSQL does (#234).
#[test]
fn relation_type_schema_ownership_is_enforced() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, REL_OWNER_ROLES);
    c.batch_execute(
        "CREATE ROLE itor_a; CREATE ROLE itor_b; CREATE ROLE itor_m IN ROLE itor_a;
         GRANT CREATE ON SCHEMA public TO itor_a, itor_b;
         CREATE SCHEMA ora AUTHORIZATION itor_a;
         SET ROLE itor_a;
         CREATE TABLE ort (id int); CREATE INDEX ort_i ON ort (id);
         CREATE VIEW orv AS SELECT 1 AS x; CREATE MATERIALIZED VIEW ormv AS SELECT 1 AS x;
         CREATE SEQUENCE ors; CREATE TYPE orty AS ENUM ('a'); CREATE DOMAIN ord AS int;
         CREATE FUNCTION ortf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
         CREATE TRIGGER ortr BEFORE INSERT ON ort FOR EACH ROW EXECUTE FUNCTION ortf();
         RESET ROLE",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itor_b").expect("SET ROLE");
    for (sql, what) in [
        ("DROP TABLE ort", "table ort"),
        ("DROP TABLE IF EXISTS ort", "table ort"),
        ("ALTER TABLE ort ADD COLUMN y int", "table ort"),
        ("ALTER TABLE ort RENAME TO ort2", "table ort"),
        ("ALTER TABLE ort OWNER TO itor_b", "table ort"),
        ("ALTER TABLE ort SET SCHEMA ora", "table ort"),
        ("COMMENT ON TABLE ort IS 'x'", "table ort"),
        ("COMMENT ON COLUMN ort.id IS 'x'", "relation ort"),
        ("CREATE INDEX ON ort (id)", "table ort"),
        ("DROP INDEX ort_i", "index ort_i"),
        ("ALTER INDEX ort_i RENAME TO ort_j", "index ort_i"),
        ("COMMENT ON INDEX ort_i IS 'x'", "index ort_i"),
        ("DROP VIEW orv", "view orv"),
        ("CREATE OR REPLACE VIEW orv AS SELECT 1 AS x", "view orv"),
        ("ALTER VIEW orv RENAME TO orw", "view orv"),
        ("ALTER VIEW orv OWNER TO itor_b", "view orv"),
        ("COMMENT ON VIEW orv IS 'x'", "view orv"),
        ("DROP MATERIALIZED VIEW ormv", "materialized view ormv"),
        ("DROP SEQUENCE ors", "sequence ors"),
        ("ALTER SEQUENCE ors RESTART", "sequence ors"),
        ("ALTER SEQUENCE ors RENAME TO ors2", "sequence ors"),
        ("ALTER SEQUENCE ors OWNER TO itor_b", "sequence ors"),
        ("COMMENT ON SEQUENCE ors IS 'x'", "sequence ors"),
        ("DROP TYPE orty", "type orty"),
        ("ALTER TYPE orty ADD VALUE 'b'", "type orty"),
        ("ALTER TYPE orty RENAME TO orty2", "type orty"),
        ("ALTER TYPE orty OWNER TO itor_b", "type orty"),
        ("COMMENT ON TYPE orty IS 'x'", "type orty"),
        ("DROP DOMAIN ord", "type ord"),
        ("ALTER DOMAIN ord SET NOT NULL", "type ord"),
        ("ALTER DOMAIN ord OWNER TO itor_b", "type ord"),
        ("COMMENT ON DOMAIN ord IS 'x'", "type ord"),
        ("DROP SCHEMA ora", "schema ora"),
        ("ALTER SCHEMA ora RENAME TO orb", "schema ora"),
        ("ALTER SCHEMA ora OWNER TO itor_b", "schema ora"),
        ("COMMENT ON SCHEMA ora IS 'x'", "schema ora"),
        ("DROP TRIGGER ortr ON ort", "relation ort"),
        ("ALTER TRIGGER ortr ON ort RENAME TO ortr2", "table ort"),
        ("COMMENT ON TRIGGER ortr ON ort IS 'x'", "relation ort"),
    ] {
        denied_with(&mut c, sql, &format!("must be owner of {what}"));
    }
    // CREATE TRIGGER needs the TRIGGER privilege, REFRESH needs MAINTAIN.
    denied_with(
        &mut c,
        "CREATE TRIGGER ortr3 BEFORE INSERT ON ort FOR EACH ROW EXECUTE FUNCTION ortf()",
        "permission denied for table ort",
    );
    denied_with(&mut c, "REFRESH MATERIALIZED VIEW ormv", "permission denied for materialized view ormv");
    let e = expect_code(c.batch_execute("DROP TABLE IF EXISTS ort_missing, ort"), "42501");
    assert_eq!(message(&e), "must be owner of table ort");
    // A member of the owning role acts as the owner.
    c.batch_execute("RESET ROLE; SET ROLE itor_m").expect("SET ROLE");
    c.batch_execute(
        "COMMENT ON TABLE ort IS 'm'; DROP INDEX ort_i; ALTER TRIGGER ortr ON ort RENAME TO ortr2; REFRESH MATERIALIZED VIEW ormv;
         ALTER TYPE orty RENAME TO orty2; ALTER DOMAIN ord SET NOT NULL; COMMENT ON SCHEMA ora IS 'm'",
    )
    .expect("member alters");
    c.batch_execute("RESET ROLE").expect("RESET ROLE");
    c.batch_execute("DROP SCHEMA ora").expect("superuser drops");
    drop_test_roles(&mut c, REL_OWNER_ROLES);
}

const OWNER_TO_ROLES: &[&str] = &["itoo_a", "itoo_b", "itoo_m", "itoo_n"];

/// ALTER ... OWNER TO: the current owner (or a member), membership in the new owner, and
/// CREATE on the object's schema; superusers skip all three (#234).
#[test]
fn owner_to_requires_membership_and_create() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, OWNER_TO_ROLES);
    c.batch_execute(
        "CREATE ROLE itoo_a; CREATE ROLE itoo_b; CREATE ROLE itoo_m IN ROLE itoo_a, itoo_b; CREATE ROLE itoo_n;
         GRANT CREATE ON SCHEMA public TO itoo_a;
         SET ROLE itoo_a; CREATE TABLE oot (id int); CREATE SEQUENCE oos;
         CREATE FUNCTION oof() RETURNS int LANGUAGE sql AS 'select 1'; RESET ROLE",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itoo_m").expect("SET ROLE");
    denied_with(&mut c, "ALTER TABLE oot OWNER TO itoo_b", "permission denied for schema public");
    denied_with(&mut c, "ALTER FUNCTION oof() OWNER TO itoo_b", "permission denied for schema public");
    denied_with(&mut c, "ALTER SEQUENCE oos OWNER TO itoo_n", "must be able to SET ROLE \"itoo_n\"");
    c.batch_execute("RESET ROLE; GRANT CREATE ON SCHEMA public TO itoo_b; SET ROLE itoo_m").expect("grant create");
    c.batch_execute("ALTER TABLE oot OWNER TO itoo_b; ALTER FUNCTION oof() OWNER TO itoo_b").expect("OWNER TO");
    c.batch_execute("RESET ROLE; ALTER TABLE oot OWNER TO itoo_n").expect("superuser OWNER TO");
    assert_eq!(rows(&mut c, "SELECT tableowner FROM pg_tables WHERE tablename = 'oot'"), [["itoo_n"]]);
    drop_test_roles(&mut c, OWNER_TO_ROLES);
}

const USAGE_ROLES: &[&str] = &["itou_a"];

/// CREATE TRIGGER needs EXECUTE on the trigger function; `$user` in search_path is the
/// current role; schemas without USAGE drop out of the search path and qualified names in
/// them are refused (#234).
#[test]
fn trigger_execute_user_schema_and_usage() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, USAGE_ROLES);
    c.batch_execute(
        "CREATE ROLE itou_a; GRANT CREATE ON SCHEMA public TO itou_a;
         CREATE FUNCTION outf() RETURNS trigger LANGUAGE plpgsql AS 'begin return new; end';
         REVOKE EXECUTE ON FUNCTION outf() FROM PUBLIC;
         CREATE SCHEMA itou_a AUTHORIZATION itou_a;
         CREATE SCHEMA ouhidden; CREATE TABLE ouhidden.ouh (id int); CREATE TABLE public.ouh (id int, v int);
         GRANT SELECT ON ouhidden.ouh, public.ouh TO itou_a;
         CREATE FUNCTION ouhidden.ouhf() RETURNS int LANGUAGE sql AS 'select 1';
         SET ROLE itou_a; CREATE TABLE outt (id int)",
    )
    .expect("setup");
    denied_with(
        &mut c,
        "CREATE TRIGGER outr BEFORE INSERT ON outt FOR EACH ROW EXECUTE FUNCTION outf()",
        "permission denied for function outf",
    );
    assert_eq!(rows(&mut c, "SELECT current_schema(), current_schemas(false)::text"), [["itou_a", "{itou_a,public}"]]);
    c.batch_execute("CREATE TABLE oumine (id int)").expect("create in $user");
    assert_eq!(rows(&mut c, "SELECT schemaname FROM pg_tables WHERE tablename = 'oumine'"), [["itou_a"]]);
    c.batch_execute("SET search_path = ouhidden, public").expect("search_path");
    assert_eq!(rows(&mut c, "SELECT current_schema(), current_schemas(false)::text"), [["public", "{public}"]]);
    assert_eq!(rows(&mut c, "SELECT count(v) FROM ouh"), [["0"]]);
    for sql in ["SELECT * FROM ouhidden.ouh", "SELECT ouhidden.ouhf()", "DROP TABLE ouhidden.ouh"] {
        denied_with(&mut c, sql, "permission denied for schema ouhidden");
    }
    c.batch_execute("RESET ROLE; RESET search_path").expect("reset");
    assert_eq!(rows(&mut c, "SELECT current_schemas(false)::text"), [["{public}"]]);
    c.batch_execute("DROP SCHEMA ouhidden CASCADE; DROP TABLE public.ouh; DROP FUNCTION outf()").expect("cleanup");
    drop_test_roles(&mut c, USAGE_ROLES);
}

const CREATE_ROLES: &[&str] = &["itcr_a", "itcr_b"];

/// CREATE on the schema is required to create objects in it, rename them and move them in
/// (`permission denied for schema s`); the public schema grants PUBLIC only USAGE (#236).
#[test]
fn schema_create_privilege_is_enforced() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, CREATE_ROLES);
    let _ = c.batch_execute("DROP SCHEMA IF EXISTS crs CASCADE");
    c.batch_execute(
        "CREATE ROLE itcr_a; CREATE ROLE itcr_b; CREATE SCHEMA crs; GRANT USAGE ON SCHEMA crs TO itcr_a, itcr_b;
         CREATE TABLE crs.owned (id int); ALTER TABLE crs.owned OWNER TO itcr_a;
         CREATE FUNCTION crs.fo() RETURNS int LANGUAGE sql AS 'select 1'; ALTER FUNCTION crs.fo() OWNER TO itcr_a;
         CREATE TYPE crs.tyo AS ENUM ('a'); ALTER TYPE crs.tyo OWNER TO itcr_a;
         SET ROLE itcr_a",
    )
    .expect("setup");
    for sql in [
        "CREATE TABLE crs.t1 (id int)",
        "CREATE TABLE t1 (id int)",
        "CREATE TABLE crs.t2 AS SELECT 1",
        "CREATE VIEW crs.v AS SELECT 1",
        "CREATE MATERIALIZED VIEW crs.mv AS SELECT 1",
        "CREATE SEQUENCE crs.sq",
        "CREATE FUNCTION crs.f() RETURNS int LANGUAGE sql AS 'select 1'",
        "CREATE PROCEDURE crs.p() LANGUAGE sql AS 'select 1'",
        "CREATE TYPE crs.ty AS ENUM ('a')",
        "CREATE DOMAIN crs.d AS int",
        "CREATE INDEX ON crs.owned (id)",
        "ALTER TABLE crs.owned RENAME TO owned2",
        "ALTER FUNCTION crs.fo() RENAME TO fo2",
    ] {
        let want = if sql == "CREATE TABLE t1 (id int)" { "public" } else { "crs" };
        denied_with(&mut c, sql, &format!("permission denied for schema {want}"));
    }
    // ALTER TYPE RENAME needs no CREATE; SET SCHEMA needs CREATE on the target.
    c.batch_execute("ALTER TYPE crs.tyo RENAME TO tyo2").expect("type rename");
    denied_with(&mut c, "ALTER TABLE crs.owned SET SCHEMA public", "permission denied for schema public");
    c.batch_execute("CREATE TEMP TABLE crtmp (id int)").expect("temp tables need no CREATE");
    c.batch_execute("RESET ROLE; GRANT CREATE ON SCHEMA crs TO itcr_a; SET ROLE itcr_a").expect("grant");
    c.batch_execute(
        "CREATE TABLE crs.t1 (id int); CREATE VIEW crs.v AS SELECT 1; CREATE SEQUENCE crs.sq; CREATE INDEX ON crs.owned (id);
         CREATE FUNCTION crs.f() RETURNS int LANGUAGE sql AS 'select 1'; CREATE TYPE crs.ty AS ENUM ('a');
         ALTER TABLE crs.owned RENAME TO owned2",
    )
    .expect("create with CREATE");
    // Ownership is checked before CREATE on the target schema.
    denied_with(&mut c, "SET ROLE itcr_b; ALTER TABLE crs.t1 SET SCHEMA public", "must be owner of table t1");
    // Database-level CREATE: schemas.
    c.batch_execute("RESET ROLE; SET ROLE itcr_a").expect("role");
    let db = rows(&mut c, "SELECT current_database()")[0][0].clone();
    denied_with(&mut c, "CREATE SCHEMA crs_new", &format!("permission denied for database {db}"));
    c.batch_execute(&format!(
        "RESET ROLE; GRANT CREATE ON DATABASE {db} TO itcr_a; SET ROLE itcr_a; CREATE SCHEMA crs_new; DROP SCHEMA crs_new"
    ))
    .expect("schema with database CREATE");
    c.batch_execute(&format!("RESET ROLE; DROP SCHEMA crs CASCADE; REVOKE CREATE ON DATABASE {db} FROM itcr_a")).expect("cleanup");
    drop_test_roles(&mut c, CREATE_ROLES);
}

const TYPE_USAGE_ROLES: &[&str] = &["ittu_a"];

/// A qualified type name in a schema without USAGE is refused, and a SECURITY DEFINER
/// function resolves `$user` as its owner (#236).
#[test]
fn type_usage_and_definer_user_schema() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, TYPE_USAGE_ROLES);
    let _ = c.batch_execute("DROP SCHEMA IF EXISTS tus CASCADE; DROP SCHEMA IF EXISTS ittu_a CASCADE; DROP FUNCTION IF EXISTS tusd()");
    c.batch_execute(
        "CREATE ROLE ittu_a; GRANT CREATE ON SCHEMA public TO ittu_a;
         CREATE SCHEMA tus; CREATE TYPE tus.ty AS ENUM ('a'); CREATE DOMAIN tus.d AS int;
         CREATE SCHEMA ittu_a AUTHORIZATION ittu_a;
         CREATE FUNCTION tusd() RETURNS text LANGUAGE sql SECURITY DEFINER AS 'select current_schema()';
         ALTER FUNCTION tusd() OWNER TO ittu_a",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT tusd()"), [["ittu_a"]]);
    c.batch_execute("SET ROLE ittu_a").expect("SET ROLE");
    for sql in [
        "CREATE TABLE tut (a tus.ty)",
        "SELECT 'a'::tus.ty",
        "SELECT CAST(1 AS tus.d)",
        "CREATE FUNCTION tuf(x tus.ty) RETURNS int LANGUAGE sql AS 'select 1'",
        "CREATE TABLE tut2 (a int); ALTER TABLE tut2 ADD COLUMN b tus.d",
    ] {
        denied_with(&mut c, sql, "permission denied for schema tus");
    }
    c.batch_execute("RESET ROLE; DROP TABLE IF EXISTS tut2; DROP FUNCTION tusd(); DROP SCHEMA tus CASCADE; DROP SCHEMA ittu_a CASCADE")
        .expect("cleanup");
    drop_test_roles(&mut c, TYPE_USAGE_ROLES);
}

const PROACL_ROLES: &[&str] = &["itpa_a", "itpa_b"];

/// `pg_proc.proacl` holds the real ACL once EXECUTE was granted or revoked: PUBLIC's default
/// entry, the owner's, then each grant (#236).
#[test]
fn proacl_reflects_execute_grants() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, PROACL_ROLES);
    let owner = rows(&mut c, "SELECT current_user")[0][0].clone();
    let acl = |c: &mut postgres::Client| rows(c, "SELECT proacl::text FROM pg_proc WHERE proname = 'pacl_f'");
    c.batch_execute("CREATE ROLE itpa_a; CREATE ROLE itpa_b; CREATE FUNCTION pacl_f() RETURNS int LANGUAGE sql AS 'select 1'")
        .expect("setup");
    assert_eq!(acl(&mut c), [["NULL"]]);
    c.batch_execute("GRANT EXECUTE ON FUNCTION pacl_f() TO itpa_a").expect("grant");
    assert_eq!(acl(&mut c), [[format!("{{=X/{owner},{owner}=X/{owner},itpa_a=X/{owner}}}")]]);
    c.batch_execute("REVOKE EXECUTE ON FUNCTION pacl_f() FROM PUBLIC").expect("revoke public");
    assert_eq!(acl(&mut c), [[format!("{{{owner}=X/{owner},itpa_a=X/{owner}}}")]]);
    c.batch_execute("REVOKE EXECUTE ON FUNCTION pacl_f() FROM itpa_a").expect("revoke");
    assert_eq!(acl(&mut c), [[format!("{{{owner}=X/{owner}}}")]]);
    c.batch_execute("GRANT EXECUTE ON FUNCTION pacl_f() TO itpa_a WITH GRANT OPTION").expect("grant option");
    assert_eq!(acl(&mut c), [[format!("{{{owner}=X/{owner},itpa_a=X*/{owner}}}")]]);
    c.batch_execute("DROP FUNCTION pacl_f()").expect("cleanup");
    drop_test_roles(&mut c, PROACL_ROLES);
}

#[track_caller]
fn assert_denied(c: &mut postgres::Client, sql: &str, rel: &str) {
    let e = expect_code(c.batch_execute(sql), "42501");
    assert_eq!(message(&e), format!("permission denied for {rel}"), "{sql}");
}

#[track_caller]
fn assert_ok(c: &mut postgres::Client, sql: &str) {
    c.batch_execute(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

/// Every relation a DML statement reads needs SELECT: UPDATE ... FROM, DELETE ... USING,
/// subqueries anywhere, INSERT ... SELECT, ON CONFLICT, and the bodies of WITH queries,
/// data-modifying ones included (#190).
#[test]
fn dml_reads_need_select() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, &["itdr_user"]);
    c.batch_execute(
        "CREATE ROLE itdr_user;
         CREATE TABLE drt (id int PRIMARY KEY, v text); CREATE TABLE drs (id int, w text);
         CREATE TABLE drc (id int, pub text, sec text);
         INSERT INTO drt VALUES (1, 'a'); INSERT INTO drs VALUES (1, 'b'); INSERT INTO drc VALUES (1, 'p', 's');
         GRANT SELECT, INSERT, UPDATE, DELETE ON drt TO itdr_user; GRANT SELECT (id, pub) ON drc TO itdr_user",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itdr_user").expect("SET ROLE");
    for sql in [
        "UPDATE drt SET v = 'x' FROM drs WHERE drs.id = drt.id",
        "DELETE FROM drt USING drs WHERE drs.id = drt.id",
        "UPDATE drt SET v = 'x' WHERE id IN (SELECT id FROM drs)",
        "UPDATE drt SET v = (SELECT w FROM drs)",
        "UPDATE drt SET v = 'x' RETURNING (SELECT w FROM drs)",
        "DELETE FROM drt RETURNING (SELECT w FROM drs)",
        "DELETE FROM drt WHERE EXISTS (SELECT 1 FROM drs)",
        "INSERT INTO drt VALUES ((SELECT id FROM drs), 'a')",
        "INSERT INTO drt SELECT id, w FROM drs",
        "INSERT INTO drt VALUES (5, 'a') RETURNING (SELECT w FROM drs)",
        "WITH x AS (SELECT * FROM drt) UPDATE drt SET v = 'x' FROM drs",
        "WITH x AS (DELETE FROM drs RETURNING *) SELECT 1",
        "WITH x AS (INSERT INTO drs VALUES (1)) SELECT 1",
        "WITH x AS (UPDATE drt SET v = 'q' WHERE id IN (SELECT id FROM drs) RETURNING *) SELECT 1",
        "WITH x AS (SELECT 1) INSERT INTO drt SELECT * FROM drs",
        "INSERT INTO drt VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = 'y' WHERE EXISTS (SELECT 1 FROM drs)",
        "INSERT INTO drt VALUES (1, 'z') ON CONFLICT (id) DO UPDATE SET v = (SELECT w FROM drs)",
        // Every other place a sublink can stand.
        "UPDATE drt SET v = 'x' WHERE id = ANY (SELECT id FROM drs)",
        "DELETE FROM drt WHERE id < ALL (SELECT id FROM drs)",
        "MERGE INTO drt USING (SELECT 1 AS id) s ON drt.id = s.id WHEN MATCHED AND drt.id = SOME (SELECT id FROM drs) THEN DELETE",
        "SELECT CASE WHEN id > 0 THEN (SELECT w FROM drs) END FROM drt",
        "SELECT coalesce(v, (SELECT w FROM drs)) FROM drt",
        "SELECT string_agg(v, ',' ORDER BY (SELECT w FROM drs)) FROM drt",
        "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY (SELECT id FROM drs)) FROM drt",
        "SELECT count(*) FILTER (WHERE EXISTS (SELECT FROM drs)) FROM drt",
        "SELECT row_number() OVER (ORDER BY (SELECT w FROM drs)) FROM drt",
        "SELECT count(*) OVER (PARTITION BY (SELECT w FROM drs)) FROM drt",
        "SELECT count(*) OVER w FROM drt WINDOW w AS (PARTITION BY (SELECT w FROM drs))",
        "SELECT * FROM drt ORDER BY (SELECT w FROM drs)",
        "SELECT count(*) FROM drt GROUP BY (SELECT w FROM drs)",
        "SELECT count(*) FROM drt HAVING EXISTS (SELECT FROM drs)",
        "SELECT DISTINCT ON ((SELECT w FROM drs)) id FROM drt",
        "SELECT * FROM drt LIMIT (SELECT id FROM drs)",
        "SELECT * FROM drt, LATERAL (SELECT w FROM drs WHERE drs.id = drt.id) x",
        "SELECT * FROM drt JOIN drt d2 ON d2.id IN (SELECT id FROM drs)",
        "SELECT * FROM (VALUES ((SELECT w FROM drs))) x",
        "SELECT * FROM generate_series(1, (SELECT id FROM drs))",
        "SELECT id FROM drt UNION SELECT id FROM drs",
        "(SELECT id FROM drt) EXCEPT (SELECT id FROM drs ORDER BY 1)",
        "SELECT ARRAY(SELECT w FROM drs)",
        "SELECT (SELECT (SELECT w FROM drs))",
    ] {
        assert_denied(&mut c, sql, "table drs");
    }
    // A SELECT WITH query nothing references is not run, so not checked; a data-modifying one is.
    assert_ok(&mut c, "WITH x AS (SELECT * FROM drs) UPDATE drt SET v = 'x'");
    assert_ok(&mut c, "WITH x AS MATERIALIZED (SELECT * FROM drs) SELECT 1");
    assert_denied(&mut c, "WITH x AS (SELECT * FROM drs) SELECT * FROM x", "table drs");
    assert_denied(&mut c, "WITH x AS (SELECT * FROM drs), y AS (SELECT * FROM x) SELECT * FROM y", "table drs");
    // Column privileges decide when the table-level privilege is missing.
    assert_ok(&mut c, "UPDATE drt SET v = drc.pub FROM drc WHERE drc.id = drt.id");
    assert_ok(&mut c, "DELETE FROM drt USING drc WHERE drc.id = drt.id + 10");
    assert_ok(&mut c, "UPDATE drt SET v = 'x' FROM drc");
    assert_ok(&mut c, "UPDATE drt SET v = 'x' WHERE EXISTS (SELECT pub FROM drc)");
    assert_ok(&mut c, "INSERT INTO drt SELECT id + 20, pub FROM drc");
    for sql in [
        "UPDATE drt SET v = drc.sec FROM drc WHERE drc.id = drt.id",
        "DELETE FROM drt USING drc WHERE drc.sec = drt.v",
        "UPDATE drt SET v = 'x' WHERE EXISTS (SELECT sec FROM drc)",
        "INSERT INTO drt SELECT id, sec FROM drc",
        "WITH x AS (SELECT sec FROM drc) DELETE FROM drt WHERE EXISTS (SELECT FROM x)",
    ] {
        assert_denied(&mut c, sql, "table drc");
    }
    // The target is checked before the relations it reads.
    c.batch_execute("RESET ROLE; REVOKE ALL ON drt FROM itdr_user; SET ROLE itdr_user").expect("revoke");
    for sql in [
        "UPDATE drt SET v = 'x' FROM drs",
        "DELETE FROM drt USING drs",
        "INSERT INTO drt SELECT * FROM drs",
        "WITH x AS (SELECT * FROM drs) UPDATE drt SET v = 'x' FROM x",
        "WITH x AS (DELETE FROM drs) DELETE FROM drt",
        "WITH x AS (DELETE FROM drs) SELECT * FROM drt",
    ] {
        assert_denied(&mut c, sql, "table drt");
    }
    c.batch_execute("RESET ROLE").expect("RESET ROLE");
    drop_test_roles(&mut c, &["itdr_user"]);
}

/// MERGE needs the privilege of each action it lists on the target (column privileges for
/// UPDATE SET and INSERT columns), SELECT on the target columns its conditions, expressions
/// and RETURNING read, and SELECT on the source (#190).
#[test]
fn merge_privileges() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, &["itmg_user"]);
    c.batch_execute(
        "CREATE ROLE itmg_user;
         CREATE TABLE mgt (id int PRIMARY KEY, v text, sec text); CREATE TABLE mgs (id int, w text);
         INSERT INTO mgt VALUES (1, 'a', 's'); INSERT INTO mgs VALUES (1, 'b'), (2, 'c')",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itmg_user").expect("SET ROLE");
    assert_denied(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN DO NOTHING", "table mgt");
    c.batch_execute("RESET ROLE; GRANT SELECT ON mgs TO itmg_user; SET ROLE itmg_user").expect("grant");
    for sql in [
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN DO NOTHING",
        "MERGE INTO mgt USING mgs ON true WHEN MATCHED THEN DO NOTHING",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN UPDATE SET v = mgs.w",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN NOT MATCHED THEN INSERT VALUES (mgs.id, mgs.w)",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN DELETE",
    ] {
        assert_denied(&mut c, sql, "table mgt");
    }
    c.batch_execute("RESET ROLE; GRANT UPDATE (v) ON mgt TO itmg_user; SET ROLE itmg_user").expect("grant");
    let e = expect_code(c.batch_execute("MERGE INTO mgt USING mgs ON true WHEN MATCHED THEN UPDATE SET v = mgs.w"), "21000");
    assert_eq!(message(&e), "MERGE command cannot affect row a second time");
    for sql in [
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN UPDATE SET v = mgs.w",
        "MERGE INTO mgt USING mgs ON true WHEN MATCHED THEN UPDATE SET sec = mgs.w",
        "MERGE INTO mgt USING mgs ON true WHEN MATCHED THEN UPDATE SET v = mgt.v",
    ] {
        assert_denied(&mut c, sql, "table mgt");
    }
    c.batch_execute("RESET ROLE; GRANT SELECT (id) ON mgt TO itmg_user; SET ROLE itmg_user").expect("grant");
    assert_ok(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN UPDATE SET v = mgs.w");
    assert_ok(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED AND mgt.id = 1 THEN UPDATE SET v = mgs.w");
    assert_eq!(
        rows(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN UPDATE SET v = mgs.w RETURNING mgt.id, merge_action()"),
        [["1", "UPDATE"]]
    );
    assert_ok(&mut c, "MERGE INTO mgt USING (SELECT * FROM mgs) x ON mgt.id = x.id WHEN MATCHED THEN DO NOTHING");
    assert_ok(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN NOT MATCHED BY SOURCE THEN UPDATE SET v = 'q'");
    for sql in [
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED AND mgt.sec = 'x' THEN UPDATE SET v = mgs.w",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN UPDATE SET v = mgs.w RETURNING mgt.v",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN NOT MATCHED THEN INSERT VALUES (mgs.id, mgs.w)",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN DELETE",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN NOT MATCHED BY SOURCE AND mgt.sec IS NULL THEN DO NOTHING",
    ] {
        assert_denied(&mut c, sql, "table mgt");
    }
    c.batch_execute("RESET ROLE; GRANT INSERT (id, v) ON mgt TO itmg_user; SET ROLE itmg_user").expect("grant");
    assert_ok(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN NOT MATCHED THEN INSERT VALUES (mgs.id + 10, mgs.w)");
    assert_ok(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN NOT MATCHED THEN INSERT (id, v) VALUES (mgs.id + 20, mgs.w)");
    for sql in [
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN NOT MATCHED THEN INSERT (id, sec) VALUES (mgs.id + 30, mgs.w)",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN NOT MATCHED AND mgs.id > 100 THEN INSERT VALUES (mgs.id + 40, mgs.w, 'x')",
    ] {
        assert_denied(&mut c, sql, "table mgt");
    }
    // The source needs SELECT on the columns read, or on some column when none is.
    c.batch_execute("RESET ROLE; REVOKE SELECT ON mgs FROM itmg_user; GRANT SELECT (id) ON mgs TO itmg_user; SET ROLE itmg_user")
        .expect("grant");
    assert_ok(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN DO NOTHING");
    for sql in [
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN UPDATE SET v = mgs.w",
        "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED AND mgs.w = 'q' THEN DO NOTHING",
    ] {
        assert_denied(&mut c, sql, "table mgs");
    }
    c.batch_execute("RESET ROLE; REVOKE ALL ON mgs FROM itmg_user; SET ROLE itmg_user").expect("revoke");
    assert_denied(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN DO NOTHING", "table mgs");
    // Only DO NOTHING actions: SELECT on some target column.
    c.batch_execute("RESET ROLE; REVOKE ALL ON mgt FROM itmg_user; GRANT SELECT ON mgs TO itmg_user; SET ROLE itmg_user").expect("revoke");
    assert_denied(&mut c, "MERGE INTO mgt USING mgs ON true WHEN MATCHED THEN DO NOTHING", "table mgt");
    c.batch_execute("RESET ROLE; GRANT SELECT (sec) ON mgt TO itmg_user; SET ROLE itmg_user").expect("grant");
    assert_ok(&mut c, "MERGE INTO mgt USING mgs ON true WHEN MATCHED THEN DO NOTHING");
    assert_denied(&mut c, "MERGE INTO mgt USING mgs ON mgt.id = mgs.id WHEN MATCHED THEN DO NOTHING", "table mgt");
    c.batch_execute("RESET ROLE").expect("RESET ROLE");
    drop_test_roles(&mut c, &["itmg_user"]);
}

/// COPY table TO needs SELECT on the copied columns, COPY table FROM INSERT on them (checked
/// before the copy starts and before the relation kind is), COPY (query) TO what the query
/// needs (#190).
#[test]
fn copy_privileges() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, &["itcp_user"]);
    c.batch_execute(
        "CREATE ROLE itcp_user;
         CREATE TABLE cpt (id int PRIMARY KEY, v text, sec text); CREATE TABLE cps (id int);
         CREATE VIEW cpv AS SELECT * FROM cps;
         INSERT INTO cpt VALUES (1, 'a', 's')",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itcp_user").expect("SET ROLE");
    let copy_out = |c: &mut postgres::Client, sql: &str| -> Result<String, postgres::Error> {
        use std::io::Read;
        let mut out = String::new();
        c.copy_out(sql)?.read_to_string(&mut out).expect("read COPY data");
        Ok(out)
    };
    let copy_in = |c: &mut postgres::Client, sql: &str, data: &str| -> Result<u64, postgres::Error> {
        use std::io::Write;
        let mut w = c.copy_in(sql)?;
        w.write_all(data.as_bytes()).expect("write COPY data");
        w.finish()
    };
    // Over the simple protocol: the error comes before any CopyIn / CopyOut response.
    for (sql, rel) in [
        ("COPY cpt TO STDOUT", "table cpt"),
        ("COPY (SELECT * FROM cpt) TO STDOUT", "table cpt"),
        ("COPY cpv TO STDOUT", "view cpv"),
        ("COPY cpt FROM STDIN", "table cpt"),
    ] {
        assert_denied(&mut c, sql, rel);
    }
    assert_eq!(copy_out(&mut c, "COPY (SELECT 1) TO STDOUT").expect("no relation"), "1\n");
    c.batch_execute(
        "RESET ROLE; GRANT SELECT (id, v), INSERT (id) ON cpt TO itcp_user; GRANT SELECT ON cpv TO itcp_user; SET ROLE itcp_user",
    )
    .expect("grant");
    assert_eq!(copy_out(&mut c, "COPY cpt (id, v) TO STDOUT").expect("granted columns"), "1\ta\n");
    assert_eq!(copy_out(&mut c, "COPY (SELECT id FROM cpt) TO STDOUT").expect("granted column"), "1\n");
    assert_eq!(copy_in(&mut c, "COPY cpt (id) FROM STDIN", "2\n").expect("granted column"), 1);
    assert_eq!(copy_in(&mut c, "COPY cpt (id) FROM STDIN WHERE id > 5", "3\n").expect("WHERE needs no SELECT"), 0);
    for sql in [
        "COPY cpt TO STDOUT",
        "COPY cpt (id, sec) TO STDOUT",
        "COPY cpt (id, v) FROM STDIN",
        "COPY cpt FROM STDIN",
        "COPY (INSERT INTO cps VALUES (1) RETURNING 1) TO STDOUT",
    ] {
        assert_denied(&mut c, sql, if sql.contains("cps") { "table cps" } else { "table cpt" });
    }
    // With the privilege, the relation kind is the error.
    let e = expect_code(c.batch_execute("COPY cpv TO STDOUT"), "42809");
    assert_eq!(message(&e), "cannot copy from view \"cpv\"");
    c.batch_execute("RESET ROLE; DROP VIEW cpv").expect("cleanup");
    drop_test_roles(&mut c, &["itcp_user"]);
}

/// UPDATE / DELETE on a view with INSTEAD OF triggers need SELECT on the view columns that
/// WHERE, SET expressions and RETURNING read (#190).
#[test]
fn instead_of_view_reads_need_select() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, &["itiv_user"]);
    c.batch_execute(
        "CREATE ROLE itiv_user;
         CREATE TABLE ivb (id int, v text, sec text); INSERT INTO ivb VALUES (1, 'a', 's');
         CREATE VIEW ivv AS SELECT id, v, sec FROM ivb;
         CREATE FUNCTION ivf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; RETURN NEW; END $$;
         CREATE TRIGGER ivt INSTEAD OF INSERT OR UPDATE OR DELETE ON ivv FOR EACH ROW EXECUTE FUNCTION ivf();
         GRANT UPDATE, DELETE ON ivv TO itiv_user",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itiv_user").expect("SET ROLE");
    assert_ok(&mut c, "UPDATE ivv SET v = 'x'");
    // DEFAULT reads nothing (the rewriter substitutes the view column's default).
    assert_ok(&mut c, "UPDATE ivv SET v = DEFAULT");
    assert_ok(&mut c, "UPDATE ivv SET (v, sec) = (DEFAULT, 'q')");
    assert_ok(&mut c, "DELETE FROM ivv");
    for sql in [
        "UPDATE ivv SET v = 'x' WHERE id = 1",
        "UPDATE ivv SET v = v",
        "UPDATE ivv SET v = 'x' RETURNING id",
        "DELETE FROM ivv WHERE id = 1",
        "DELETE FROM ivv RETURNING id",
    ] {
        assert_denied(&mut c, sql, "view ivv");
    }
    c.batch_execute("RESET ROLE; GRANT SELECT (id) ON ivv TO itiv_user; SET ROLE itiv_user").expect("grant");
    assert_ok(&mut c, "UPDATE ivv SET v = 'x' WHERE id = 1");
    assert_ok(&mut c, "DELETE FROM ivv WHERE id = 1");
    for sql in [
        "UPDATE ivv SET v = 'x' WHERE sec = 's'",
        "DELETE FROM ivv WHERE v = 'a'",
        "DELETE FROM ivv RETURNING *",
        "UPDATE ivv SET v = 'x' RETURNING v",
    ] {
        assert_denied(&mut c, sql, "view ivv");
    }
    c.batch_execute("RESET ROLE; DROP VIEW ivv; DROP FUNCTION ivf()").expect("cleanup");
    drop_test_roles(&mut c, &["itiv_user"]);
}

fn detail(e: &postgres::Error) -> Option<String> {
    e.as_db_error().and_then(|d| d.detail().map(str::to_owned))
}

/// Constraint-violation DETAIL shows only what the role may read: the failing row lists the
/// columns it can select or is writing (by name, unless it can select the whole table), and
/// key details need SELECT on every key column (#190).
#[test]
fn constraint_detail_hides_unreadable_columns() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, &["itcd_user"]);
    c.batch_execute(
        "CREATE ROLE itcd_user;
         CREATE TABLE cdt (id int PRIMARY KEY, v text NOT NULL, sec text CHECK (sec <> 'bad'), u int UNIQUE, a int, b int, UNIQUE (a, b));
         CREATE TABLE cdp (id int PRIMARY KEY); CREATE TABLE cdc (id int, pid int REFERENCES cdp, x text);
         INSERT INTO cdt VALUES (1, 'a', 's', 1, 1, 1); INSERT INTO cdp VALUES (1); INSERT INTO cdc VALUES (1, 1, 'x');
         GRANT INSERT, UPDATE ON cdt, cdc TO itcd_user; GRANT DELETE ON cdp TO itcd_user",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itcd_user").expect("SET ROLE");
    let fails = |c: &mut postgres::Client, sql: &str, code: &str| detail(&expect_code(c.batch_execute(sql), code));
    let row = "Failing row contains (id, v, sec, u, a, b) = (2, null, s, 2, 2, 2).";
    assert_eq!(fails(&mut c, "INSERT INTO cdt VALUES (2, NULL, 's', 2, 2, 2)", "23502").as_deref(), Some(row));
    assert_eq!(
        fails(&mut c, "INSERT INTO cdt VALUES (2, 'a', 'bad', 2, 2, 2)", "23514").as_deref(),
        Some("Failing row contains (id, v, sec, u, a, b) = (2, a, bad, 2, 2, 2).")
    );
    assert_eq!(fails(&mut c, "INSERT INTO cdt VALUES (1, 'a', 's', 2, 2, 2)", "23505"), None);
    assert_eq!(fails(&mut c, "INSERT INTO cdc VALUES (2, 5, 'y')", "23503").as_deref(), Some("Key is not present in table \"cdp\"."));
    assert_eq!(fails(&mut c, "DELETE FROM cdp", "23503").as_deref(), Some("Key is still referenced from table \"cdc\"."));
    c.batch_execute("RESET ROLE; GRANT SELECT (id, v, a) ON cdt TO itcd_user; GRANT SELECT (pid) ON cdc TO itcd_user; GRANT SELECT (id) ON cdp TO itcd_user; SET ROLE itcd_user").expect("grant");
    assert_eq!(fails(&mut c, "INSERT INTO cdt VALUES (2, NULL, 's', 2, 2, 2)", "23502").as_deref(), Some(row));
    assert_eq!(fails(&mut c, "INSERT INTO cdt VALUES (1, 'a', 's', 2, 2, 2)", "23505").as_deref(), Some("Key (id)=(1) already exists."));
    assert_eq!(fails(&mut c, "INSERT INTO cdt VALUES (2, 'a', 's', 1, 2, 2)", "23505"), None);
    assert_eq!(fails(&mut c, "INSERT INTO cdt VALUES (2, 'a', 's', 2, 1, 1)", "23505"), None);
    assert_eq!(
        fails(&mut c, "UPDATE cdt SET sec = 'bad'", "23514").as_deref(),
        Some("Failing row contains (id, v, sec, a) = (1, a, bad, 1).")
    );
    assert_eq!(
        fails(&mut c, "INSERT INTO cdc VALUES (2, 5, 'y')", "23503").as_deref(),
        Some("Key (pid)=(5) is not present in table \"cdp\".")
    );
    assert_eq!(fails(&mut c, "DELETE FROM cdp", "23503").as_deref(), Some("Key (id)=(1) is still referenced from table \"cdc\"."));
    c.batch_execute(
        "RESET ROLE; REVOKE ALL ON cdt FROM itcd_user; GRANT INSERT (id, v), SELECT (sec) ON cdt TO itcd_user; SET ROLE itcd_user",
    )
    .expect("grant");
    assert_eq!(
        fails(&mut c, "INSERT INTO cdt (id, v) VALUES (3, NULL)", "23502").as_deref(),
        Some("Failing row contains (id, v, sec) = (3, null, null).")
    );
    // With SELECT on the table, the whole row (unnamed) and the keys are shown.
    c.batch_execute("RESET ROLE; GRANT SELECT, INSERT ON cdt TO itcd_user; SET ROLE itcd_user").expect("grant");
    assert_eq!(
        fails(&mut c, "INSERT INTO cdt VALUES (2, NULL, 's', 2, 2, 2)", "23502").as_deref(),
        Some("Failing row contains (2, null, s, 2, 2, 2).")
    );
    assert_eq!(fails(&mut c, "INSERT INTO cdt VALUES (2, 'a', 's', 1, 2, 2)", "23505").as_deref(), Some("Key (u)=(1) already exists."));
    c.batch_execute("RESET ROLE").expect("RESET ROLE");
    drop_test_roles(&mut c, &["itcd_user"]);
}

/// information_schema.table_privileges shows grants whose grantor or grantee is a role the
/// current user has the privileges of (or PUBLIC); role_table_grants omits PUBLIC (#190).
#[test]
fn table_privileges_filtered_by_enabled_roles() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, &["ittp_user", "ittp_other"]);
    c.batch_execute(
        "CREATE ROLE ittp_user; CREATE ROLE ittp_other;
         CREATE TABLE tpa (id int); CREATE TABLE tpb (id int);
         GRANT INSERT ON tpa TO ittp_user; GRANT SELECT ON tpb TO ittp_other; GRANT SELECT ON tpb TO PUBLIC;
         GRANT UPDATE ON tpb TO ittp_user",
    )
    .expect("setup");
    c.batch_execute("SET ROLE ittp_user").expect("SET ROLE");
    let q = |view: &str| {
        format!(
            "SELECT grantee, table_name, privilege_type FROM information_schema.{view} WHERE table_name IN ('tpa', 'tpb') ORDER BY 1, 2, 3"
        )
    };
    assert_eq!(
        rows(&mut c, &q("table_privileges")),
        [["PUBLIC", "tpb", "SELECT"], ["ittp_user", "tpa", "INSERT"], ["ittp_user", "tpb", "UPDATE"]]
    );
    assert_eq!(rows(&mut c, &q("role_table_grants")), [["ittp_user", "tpa", "INSERT"], ["ittp_user", "tpb", "UPDATE"]]);
    c.batch_execute("RESET ROLE").expect("RESET ROLE");
    drop_test_roles(&mut c, &["ittp_user", "ittp_other"]);
}

/// Deferred constraint checks (at COMMIT and SET CONSTRAINTS ... IMMEDIATE) hide key values
/// like immediate ones; writes through an automatically updatable view count the columns
/// they write on the base table; an INSERT ... SELECT writes as many columns as the query
/// returns; a row routed into a partition is described in the partitioned table's column
/// order (#190).
#[test]
fn constraint_detail_deferred_views_partitions() {
    let (_db, mut c) = setup();
    drop_test_roles(&mut c, &["itcv_user"]);
    c.batch_execute(
        "CREATE ROLE itcv_user;
         CREATE TABLE cvp (id int PRIMARY KEY);
         CREATE TABLE cvc (id int, pid int REFERENCES cvp DEFERRABLE INITIALLY DEFERRED, x text);
         CREATE TABLE cvu (id int, u int UNIQUE DEFERRABLE INITIALLY DEFERRED); INSERT INTO cvu VALUES (1, 1);
         CREATE TABLE cvb (id int, v text NOT NULL, sec text CHECK (sec <> 'bad'), w text); INSERT INTO cvb VALUES (1, 'a', 's', 'w');
         CREATE VIEW cvv AS SELECT w, sec, id, v FROM cvb;
         CREATE VIEW cvo AS SELECT id, v, sec FROM cvb WHERE sec <> 'zz' WITH CHECK OPTION;
         CREATE TABLE cvi (id int, v text NOT NULL, sec text);
         CREATE TABLE cvs (a int, b text); INSERT INTO cvs VALUES (1, NULL);
         CREATE TABLE cvt (id int, v text NOT NULL, sec text) PARTITION BY LIST (id);
         CREATE TABLE cvt1 (sec text, v text NOT NULL, id int); ALTER TABLE cvt ATTACH PARTITION cvt1 FOR VALUES IN (1);
         GRANT INSERT ON cvc, cvu, cvi, cvt TO itcv_user; GRANT INSERT, UPDATE ON cvv, cvo TO itcv_user;
         GRANT SELECT ON cvs TO itcv_user; GRANT SELECT (id) ON cvt TO itcv_user",
    )
    .expect("setup");
    c.batch_execute("SET ROLE itcv_user").expect("SET ROLE");
    let fails = |c: &mut postgres::Client, sql: &str, code: &str| detail(&expect_code(c.batch_execute(sql), code));
    let fk = Some("Key is not present in table \"cvp\".");
    assert_eq!(fails(&mut c, "BEGIN; INSERT INTO cvc VALUES (1, 5, 'y'); COMMIT", "23503").as_deref(), fk);
    assert_eq!(fails(&mut c, "BEGIN; INSERT INTO cvu VALUES (2, 1); COMMIT", "23505"), None);
    c.batch_execute("BEGIN; INSERT INTO cvc VALUES (1, 5, 'y')").expect("deferred");
    assert_eq!(fails(&mut c, "SET CONSTRAINTS ALL IMMEDIATE", "23503").as_deref(), fk);
    c.batch_execute("ROLLBACK").expect("rollback");
    for (sql, code, want) in [
        ("INSERT INTO cvv (id, v) VALUES (2, NULL)", "23502", "(id, v) = (2, null)"),
        ("INSERT INTO cvv (id, sec, v) VALUES (2, 'bad', 'x')", "23514", "(id, v, sec) = (2, x, bad)"),
        ("UPDATE cvv SET sec = 'bad'", "23514", "(sec) = (bad)"),
        ("INSERT INTO cvo (id, v, sec) VALUES (2, 'x', 'zz')", "44000", "(id, v, sec) = (2, x, zz)"),
        ("INSERT INTO cvo (id, v, sec) VALUES (2, NULL, 's')", "23502", "(id, v, sec) = (2, null, s)"),
        ("INSERT INTO cvi SELECT * FROM cvs", "23502", "(id, v) = (1, null)"),
        ("INSERT INTO cvi SELECT a, b FROM cvs UNION ALL SELECT 1, NULL", "23502", "(id, v) = (1, null)"),
        ("INSERT INTO cvi (id, v) SELECT * FROM cvs", "23502", "(id, v) = (1, null)"),
        ("INSERT INTO cvt VALUES (1, NULL, 's')", "23502", "(id, v, sec) = (1, null, s)"),
    ] {
        assert_eq!(fails(&mut c, sql, code), Some(format!("Failing row contains {want}.")), "{sql}");
    }
    // Routed rows are in the target's layout for every role; a direct write uses the partition's.
    c.batch_execute("RESET ROLE").expect("RESET ROLE");
    assert_eq!(fails(&mut c, "INSERT INTO cvt VALUES (1, NULL, 's')", "23502").as_deref(), Some("Failing row contains (1, null, s)."));
    assert_eq!(fails(&mut c, "INSERT INTO cvt1 VALUES ('s', NULL, 1)", "23502").as_deref(), Some("Failing row contains (s, null, 1)."));
    c.batch_execute("DROP VIEW cvv; DROP VIEW cvo").expect("cleanup");
    drop_test_roles(&mut c, &["itcv_user"]);
}
