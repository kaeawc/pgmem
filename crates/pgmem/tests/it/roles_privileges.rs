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
