//! Row-level security: ALTER TABLE ... ROW LEVEL SECURITY, CREATE / ALTER / DROP POLICY, the
//! `pg_policy` / `pg_policies` catalogs, and policy enforcement under SET ROLE. Roles are
//! cluster-wide on PostgreSQL: each test uses its own role names and drops them.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn hint(e: &postgres::Error) -> Option<String> {
    e.as_db_error().and_then(|d| d.hint().map(str::to_owned))
}

fn detail(e: &postgres::Error) -> Option<String> {
    e.as_db_error().and_then(|d| d.detail().map(str::to_owned))
}

/// Creates `roles` (dropping leftovers of an earlier run first).
fn create_roles(c: &mut postgres::Client, roles: &[&str]) {
    drop_roles(c, roles);
    for r in roles {
        c.batch_execute(&format!("CREATE ROLE {r}")).expect("create role");
    }
}

fn drop_roles(c: &mut postgres::Client, roles: &[&str]) {
    let _ = c.batch_execute("RESET ROLE");
    for r in roles {
        let _ = c.batch_execute(&format!("DROP OWNED BY {r}"));
        let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {r}"));
    }
}

#[test]
fn row_security_flags_and_policy_catalog() {
    let (_db, mut c) = setup();
    let roles = ["rls_cat_a", "rls_cat_b"];
    create_roles(&mut c, &roles);
    c.batch_execute("CREATE TABLE rt (id int, owner name, tx text, n int)").expect("table");
    let flags = "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE relname = 'rt'";
    assert_eq!(rows(&mut c, flags), [["f", "f"]]);
    c.batch_execute("ALTER TABLE rt ENABLE ROW LEVEL SECURITY").expect("enable");
    assert_eq!(rows(&mut c, flags), [["t", "f"]]);
    assert_eq!(rows(&mut c, "SELECT rowsecurity FROM pg_tables WHERE tablename = 'rt'"), [["t"]]);
    c.batch_execute("ALTER TABLE rt FORCE ROW LEVEL SECURITY").expect("force");
    assert_eq!(rows(&mut c, flags), [["t", "t"]]);
    c.batch_execute("ALTER TABLE rt NO FORCE ROW LEVEL SECURITY, DISABLE ROW LEVEL SECURITY").expect("disable");
    assert_eq!(rows(&mut c, flags), [["f", "f"]]);

    c.batch_execute(
        "CREATE POLICY p1 ON rt USING (owner = current_user);
         CREATE POLICY p2 ON rt AS RESTRICTIVE FOR UPDATE TO rls_cat_b, rls_cat_a USING (n > 0) WITH CHECK (n < 100);
         CREATE POLICY p3 ON rt FOR INSERT WITH CHECK (tx <> 'bad');
         CREATE POLICY p4 ON rt FOR SELECT TO PUBLIC USING (tx IN ('a', 'b'));
         CREATE POLICY p5 ON rt FOR DELETE USING (id > (SELECT 1));
         CREATE POLICY p6 ON rt FOR SELECT",
    )
    .expect("policies");
    assert_eq!(
        rows(
            &mut c,
            "SELECT schemaname, tablename, policyname, permissive, roles, cmd, qual, with_check FROM pg_policies ORDER BY policyname"
        ),
        [
            ["public", "rt", "p1", "PERMISSIVE", "{public}", "ALL", "(owner = CURRENT_USER)", "NULL"],
            ["public", "rt", "p2", "RESTRICTIVE", "{rls_cat_a,rls_cat_b}", "UPDATE", "(n > 0)", "(n < 100)"],
            ["public", "rt", "p3", "PERMISSIVE", "{public}", "INSERT", "NULL", "(tx <> 'bad'::text)"],
            ["public", "rt", "p4", "PERMISSIVE", "{public}", "SELECT", "(tx = ANY (ARRAY['a'::text, 'b'::text]))", "NULL"],
            ["public", "rt", "p5", "PERMISSIVE", "{public}", "DELETE", "(id > ( SELECT 1))", "NULL"],
            ["public", "rt", "p6", "PERMISSIVE", "{public}", "SELECT", "NULL", "NULL"],
        ]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT polname, polrelid::regclass, polcmd, polpermissive, polroles = '{0}', pg_get_expr(polqual, polrelid), \
             pg_get_expr(polwithcheck, polrelid) FROM pg_policy ORDER BY polname"
        ),
        [
            ["p1", "rt", "*", "t", "t", "(owner = CURRENT_USER)", "NULL"],
            ["p2", "rt", "w", "f", "f", "(n > 0)", "(n < 100)"],
            ["p3", "rt", "a", "t", "t", "NULL", "(tx <> 'bad'::text)"],
            ["p4", "rt", "r", "t", "t", "(tx = ANY (ARRAY['a'::text, 'b'::text]))", "NULL"],
            ["p5", "rt", "d", "t", "t", "(id > ( SELECT 1))", "NULL"],
            ["p6", "rt", "r", "t", "t", "NULL", "NULL"],
        ]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT array_length(polroles, 1), polroles[1] = (SELECT oid FROM pg_roles WHERE rolname = 'rls_cat_b') FROM pg_policy WHERE polname = 'p2'"
        ),
        [["2", "t"]]
    );

    // Errors, in Postgres' order of checks.
    let e = expect_code(c.batch_execute("CREATE POLICY p1 ON rt USING (true)"), "42710");
    assert_eq!(message(&e), "policy \"p1\" for table \"rt\" already exists");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rt FOR INSERT USING (true)"), "42601");
    assert_eq!(message(&e), "only WITH CHECK expression allowed for INSERT");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rt FOR SELECT WITH CHECK (true)"), "42601");
    assert_eq!(message(&e), "WITH CHECK cannot be applied to SELECT or DELETE");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rt FOR DELETE WITH CHECK (true)"), "42601");
    assert_eq!(message(&e), "WITH CHECK cannot be applied to SELECT or DELETE");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rt TO rls_cat_nobody"), "42704");
    assert_eq!(message(&e), "role \"rls_cat_nobody\" does not exist");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rls_cat_nosuch"), "42P01");
    assert_eq!(message(&e), "relation \"rls_cat_nosuch\" does not exist");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rt USING (count(*) > 0)"), "42803");
    assert_eq!(message(&e), "aggregate functions are not allowed in policy expressions");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rt USING (1)"), "42804");
    assert_eq!(message(&e), "argument of POLICY must be type boolean, not type integer");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rt USING (nosuchcol = 1)"), "42703");
    assert_eq!(message(&e), "column \"nosuchcol\" does not exist");
    c.batch_execute("CREATE VIEW rv AS SELECT * FROM rt").expect("view");
    let e = expect_code(c.batch_execute("CREATE POLICY px ON rv USING (true)"), "42809");
    assert_eq!(message(&e), "\"rv\" is not a table");
    c.batch_execute("DROP VIEW rv").expect("drop view");

    // ALTER POLICY.
    let e = expect_code(c.batch_execute("ALTER POLICY nope ON rt RENAME TO x"), "42704");
    assert_eq!(message(&e), "policy \"nope\" for table \"rt\" does not exist");
    let e = expect_code(c.batch_execute("ALTER POLICY p1 ON rt RENAME TO p2"), "42710");
    assert_eq!(message(&e), "policy \"p2\" for table \"rt\" already exists");
    let e = expect_code(c.batch_execute("ALTER POLICY p3 ON rt USING (true)"), "42601");
    assert_eq!(message(&e), "only WITH CHECK expression allowed for INSERT");
    let e = expect_code(c.batch_execute("ALTER POLICY p4 ON rt WITH CHECK (true)"), "42601");
    assert_eq!(message(&e), "only USING expression allowed for SELECT, DELETE");
    c.batch_execute(
        "ALTER POLICY p1 ON rt RENAME TO p1r;
         ALTER POLICY p1r ON rt TO rls_cat_b USING (owner <> 'x') WITH CHECK (n IS NOT NULL);
         ALTER POLICY p3 ON rt WITH CHECK (tx <> 'worse')",
    )
    .expect("alter policy");
    assert_eq!(
        rows(&mut c, "SELECT policyname, roles, qual, with_check FROM pg_policies WHERE policyname IN ('p1r', 'p3') ORDER BY 1"),
        [["p1r", "{rls_cat_b}", "(owner <> 'x'::name)", "(n IS NOT NULL)"], ["p3", "{public}", "NULL", "(tx <> 'worse'::text)"]]
    );

    // COMMENT ON POLICY.
    c.batch_execute("COMMENT ON POLICY p4 ON rt IS 'visible ones'").expect("comment");
    assert_eq!(
        rows(&mut c, "SELECT d.description, d.classoid::regclass FROM pg_description d JOIN pg_policy p ON p.oid = d.objoid"),
        [["visible ones", "pg_policy"]]
    );

    // A role named by policies cannot be dropped.
    let e = expect_code(c.batch_execute("DROP ROLE rls_cat_b"), "2BP01");
    assert_eq!(message(&e), "role \"rls_cat_b\" cannot be dropped because some objects depend on it");
    assert_eq!(detail(&e).as_deref(), Some("target of policy p1r on table rt\ntarget of policy p2 on table rt"));

    // Columns: renames follow, drops of a used column are refused.
    c.batch_execute("ALTER TABLE rt RENAME COLUMN tx TO kind").expect("rename column");
    assert_eq!(rows(&mut c, "SELECT with_check FROM pg_policies WHERE policyname = 'p3'"), [["(kind <> 'worse'::text)"]]);
    let e = expect_code(c.batch_execute("ALTER TABLE rt DROP COLUMN kind"), "2BP01");
    assert_eq!(message(&e), "cannot drop column kind of table rt because other objects depend on it");
    assert_eq!(
        detail(&e).as_deref(),
        Some("policy p3 on table rt depends on column kind of table rt\npolicy p4 on table rt depends on column kind of table rt")
    );

    // DROP POLICY.
    let e = expect_code(c.batch_execute("DROP POLICY nope ON rt"), "42704");
    assert_eq!(message(&e), "policy \"nope\" for table \"rt\" does not exist");
    c.batch_execute("DROP POLICY IF EXISTS nope ON rt; DROP POLICY p6 ON rt").expect("drop policy");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_policies"), [["5"]]);
    c.batch_execute("DROP TABLE rt").expect("drop table");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_policy"), [["0"]]);
    drop_roles(&mut c, &roles);
}

#[test]
fn drop_owned_removes_the_role_from_policies() {
    let (_db, mut c) = setup();
    let roles = ["rls_own_a", "rls_own_b"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE ot (id int);
         CREATE POLICY two ON ot TO rls_own_a, rls_own_b USING (true);
         CREATE POLICY one ON ot TO rls_own_a USING (true);
         DROP OWNED BY rls_own_a",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT policyname, roles FROM pg_policies WHERE tablename = 'ot'"), [["two", "{rls_own_b}"]]);
    c.batch_execute("DROP TABLE ot").expect("drop");
    drop_roles(&mut c, &roles);
}

#[test]
fn policies_filter_and_check_rows_for_other_roles() {
    let (_db, mut c) = setup();
    let roles = ["rls_enf_u1", "rls_enf_u2"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE rt (id int, owner name, tx text, n int);
         GRANT ALL ON rt TO PUBLIC;
         INSERT INTO rt VALUES (1, 'rls_enf_u1', 'a', 1), (2, 'rls_enf_u2', 'b', 2);
         ALTER TABLE rt ENABLE ROW LEVEL SECURITY",
    )
    .expect("setup");
    // Enabled without policies: nothing is visible or writable.
    c.batch_execute("SET ROLE rls_enf_u1").expect("set role");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM rt"), [["0"]]);
    let e = expect_code(c.batch_execute("INSERT INTO rt VALUES (3, 'rls_enf_u1', 'c', 3)"), "42501");
    assert_eq!(message(&e), "new row violates row-level security policy for table \"rt\"");
    assert_eq!(c.execute("UPDATE rt SET n = 0", &[]).expect("update"), 0);
    assert_eq!(c.execute("DELETE FROM rt", &[]).expect("delete"), 0);
    // The owner (here the superuser) is not subject to the policies.
    c.batch_execute("RESET ROLE").expect("reset");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM rt"), [["2"]]);
    c.batch_execute(
        "CREATE POLICY p1 ON rt USING (owner = current_user);
         CREATE POLICY p2 ON rt AS RESTRICTIVE FOR UPDATE TO rls_enf_u1, rls_enf_u2 USING (n > 0) WITH CHECK (n < 100);
         CREATE POLICY p3 ON rt FOR INSERT WITH CHECK (tx <> 'bad');
         CREATE POLICY p4 ON rt FOR SELECT TO PUBLIC USING (tx IN ('a', 'b'))",
    )
    .expect("policies");
    c.batch_execute("SET ROLE rls_enf_u1").expect("set role");
    // Permissive policies are ORed.
    assert_eq!(rows(&mut c, "SELECT id, owner FROM rt AS x ORDER BY id"), [["1", "rls_enf_u1"], ["2", "rls_enf_u2"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM rt WHERE tx = 'b'"), [["1"]]);
    c.batch_execute("INSERT INTO rt VALUES (4, 'rls_enf_u1', 'bad', 4), (5, 'rls_enf_u2', 'ok', 5)").expect("insert");
    // The restrictive UPDATE policy names itself when it fails.
    let e = expect_code(c.batch_execute("UPDATE rt SET n = 200 WHERE id = 1"), "42501");
    assert_eq!(message(&e), "new row violates row-level security policy \"p2\" for table \"rt\"");
    assert_eq!(c.execute("UPDATE rt SET n = -1", &[]).expect("update"), 2);
    assert_eq!(c.execute("UPDATE rt SET n = -2", &[]).expect("update"), 0);
    assert_eq!(rows(&mut c, "SELECT id, n FROM rt ORDER BY id"), [["1", "-1"], ["2", "2"], ["4", "-1"]]);
    // DELETE sees the rows of the ALL policy only.
    assert_eq!(c.execute("DELETE FROM rt WHERE id > 0", &[]).expect("delete"), 2);
    c.batch_execute("SET ROLE rls_enf_u2").expect("set role");
    assert_eq!(rows(&mut c, "SELECT id FROM rt ORDER BY id"), [["2"], ["5"]]);
    c.batch_execute("RESET ROLE").expect("reset");
    assert_eq!(rows(&mut c, "SELECT id FROM rt ORDER BY id"), [["2"], ["5"]]);
    c.batch_execute("DROP TABLE rt").expect("drop");
    drop_roles(&mut c, &roles);
}

#[test]
fn select_policies_apply_when_dml_reads_the_target() {
    let (_db, mut c) = setup();
    let roles = ["rls_dml_c1"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE ct (id int, k text, v int);
         GRANT ALL ON ct TO PUBLIC;
         INSERT INTO ct VALUES (1, 's', 1), (2, 'h', 2);
         ALTER TABLE ct ENABLE ROW LEVEL SECURITY;
         CREATE POLICY ins ON ct FOR INSERT WITH CHECK (true);
         CREATE POLICY upd ON ct FOR UPDATE USING (true) WITH CHECK (true);
         CREATE POLICY del ON ct FOR DELETE USING (true);
         CREATE POLICY sel ON ct FOR SELECT USING (k = 's');
         SET ROLE rls_dml_c1",
    )
    .expect("setup");
    c.batch_execute("INSERT INTO ct VALUES (3, 'h', 3)").expect("insert without RETURNING");
    // RETURNING reads the new row: it must pass the SELECT policies.
    let e = expect_code(c.batch_execute("INSERT INTO ct VALUES (4, 'h', 4) RETURNING id"), "42501");
    assert_eq!(message(&e), "new row violates row-level security policy for table \"ct\"");
    assert_eq!(rows(&mut c, "INSERT INTO ct VALUES (5, 'h', 5) RETURNING 1"), [["1"]]);
    // A WHERE on the target's columns adds the SELECT policies' USING.
    assert_eq!(c.execute("UPDATE ct SET v = 10 WHERE id = 2", &[]).expect("update"), 0);
    assert_eq!(c.execute("UPDATE ct SET v = 10", &[]).expect("update"), 4);
    let e = expect_code(c.batch_execute("UPDATE ct SET k = 'h' WHERE id = 1"), "42501");
    assert_eq!(message(&e), "new row violates row-level security policy for table \"ct\"");
    assert_eq!(c.execute("UPDATE ct SET k = 'h'", &[]).expect("update"), 4);
    assert_eq!(rows(&mut c, "UPDATE ct SET v = v + 1 RETURNING id"), Vec::<Vec<String>>::new());
    assert_eq!(c.execute("DELETE FROM ct WHERE k = 'h'", &[]).expect("delete"), 0);
    assert_eq!(c.execute("DELETE FROM ct", &[]).expect("delete"), 4);
    c.batch_execute("RESET ROLE").expect("reset");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM ct"), [["0"]]);
    c.batch_execute("DROP TABLE ct").expect("drop");
    drop_roles(&mut c, &roles);
}

#[test]
fn owners_force_bypassrls_and_row_security_off() {
    let (_db, mut c) = setup();
    let roles = ["rls_frc_a", "rls_frc_owner", "rls_frc_member"];
    create_roles(&mut c, &roles);
    c.batch_execute("DROP ROLE IF EXISTS rls_frc_byp; CREATE ROLE rls_frc_byp BYPASSRLS; GRANT rls_frc_owner TO rls_frc_member")
        .expect("roles");
    c.batch_execute(
        "CREATE TABLE bt (id int PRIMARY KEY, who text, v int);
         ALTER TABLE bt OWNER TO rls_frc_owner;
         GRANT ALL ON bt TO PUBLIC;
         INSERT INTO bt VALUES (1, 'rls_frc_a', 10), (2, 'x', 20);
         ALTER TABLE bt ENABLE ROW LEVEL SECURITY;
         CREATE POLICY mine ON bt TO rls_frc_a USING (who = current_user)",
    )
    .expect("setup");
    c.batch_execute("SET ROLE rls_frc_a").expect("set role");
    assert_eq!(rows(&mut c, "SELECT id FROM bt"), [["1"]]);
    let e = expect_code(c.batch_execute("CREATE POLICY notmine ON bt USING (true)"), "42501");
    assert_eq!(message(&e), "must be owner of table bt");
    let e = expect_code(c.batch_execute("ALTER POLICY mine ON bt USING (true)"), "42501");
    assert_eq!(message(&e), "must be owner of table bt");
    let e = expect_code(c.batch_execute("DROP POLICY mine ON bt"), "42501");
    assert_eq!(message(&e), "must be owner of relation bt");
    let e = expect_code(c.batch_execute("ALTER TABLE bt DISABLE ROW LEVEL SECURITY"), "42501");
    assert_eq!(message(&e), "must be owner of table bt");
    assert_eq!(rows(&mut c, "SELECT row_security_active('bt')"), [["t"]]);
    // The owner, and members of the owning role, bypass the policies unless FORCEd.
    c.batch_execute("SET ROLE rls_frc_owner").expect("set role");
    assert_eq!(rows(&mut c, "SELECT count(*), row_security_active('bt') FROM bt"), [["2", "f"]]);
    c.batch_execute("SET ROLE rls_frc_member").expect("set role");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM bt"), [["2"]]);
    c.batch_execute("SET ROLE rls_frc_owner; ALTER TABLE bt FORCE ROW LEVEL SECURITY").expect("force");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM bt"), [["0"]]);
    // row_security = off: an error instead of filtered results.
    c.batch_execute("SET row_security = off").expect("set");
    let e = expect_code(c.batch_execute("SELECT * FROM bt"), "42501");
    assert_eq!(message(&e), "query would be affected by row-level security policy for table \"bt\"");
    assert_eq!(hint(&e).as_deref(), Some("To disable the policy for the table's owner, use ALTER TABLE NO FORCE ROW LEVEL SECURITY."));
    c.batch_execute("SET ROLE rls_frc_a").expect("set role");
    let e = expect_code(c.batch_execute("SELECT * FROM bt WHERE false"), "42501");
    assert_eq!(message(&e), "query would be affected by row-level security policy for table \"bt\"");
    assert_eq!(hint(&e), None);
    let e = expect_code(c.batch_execute("INSERT INTO bt VALUES (9, 'rls_frc_a', 1)"), "42501");
    assert_eq!(message(&e), "query would be affected by row-level security policy for table \"bt\"");
    assert_eq!(rows(&mut c, "SELECT row_security_active('bt')"), [["t"]]);
    // BYPASSRLS roles and superusers see everything, even with row_security off.
    c.batch_execute("SET ROLE rls_frc_byp").expect("set role");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM bt"), [["2"]]);
    c.batch_execute("RESET ROLE").expect("reset");
    assert_eq!(rows(&mut c, "SELECT count(*), row_security_active('bt') FROM bt"), [["2", "f"]]);
    c.batch_execute("RESET row_security; DROP TABLE bt").expect("drop");
    drop_roles(&mut c, &["rls_frc_byp", "rls_frc_member", "rls_frc_a", "rls_frc_owner"]);
}

#[test]
fn views_apply_policies_as_their_owner_and_copy_respects_them() {
    let (_db, mut c) = setup();
    let roles = ["rls_vw_a", "rls_vw_b", "rls_vw_owner"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE bt (id int, who text);
         ALTER TABLE bt OWNER TO rls_vw_owner;
         GRANT ALL ON bt TO PUBLIC;
         INSERT INTO bt VALUES (1, 'rls_vw_a'), (2, 'rls_vw_b'), (3, 'x');
         ALTER TABLE bt ENABLE ROW LEVEL SECURITY;
         CREATE POLICY mine ON bt TO rls_vw_a USING (who = current_user);
         CREATE VIEW bv_owner AS SELECT * FROM bt;
         ALTER VIEW bv_owner OWNER TO rls_vw_owner;
         CREATE VIEW bv_a AS SELECT * FROM bt;
         ALTER VIEW bv_a OWNER TO rls_vw_a;
         CREATE VIEW bv_inv WITH (security_invoker) AS SELECT * FROM bt;
         ALTER VIEW bv_inv OWNER TO rls_vw_owner;
         GRANT ALL ON bv_owner, bv_a, bv_inv TO PUBLIC;
         SET ROLE rls_vw_b",
    )
    .expect("setup");
    // The table owner owns bv_owner: no policies. bv_a applies rls_vw_a's policies, with
    // current_user still the invoker. bv_inv applies the invoker's (none: nothing visible).
    assert_eq!(rows(&mut c, "SELECT count(*) FROM bv_owner"), [["3"]]);
    assert_eq!(rows(&mut c, "SELECT id FROM bv_a"), [["2"]]);
    assert_eq!(rows(&mut c, "SELECT count(*) FROM bv_inv"), [["0"]]);
    let e = expect_code(c.batch_execute("INSERT INTO bv_a VALUES (5, 'rls_vw_a')"), "42501");
    assert_eq!(message(&e), "new row violates row-level security policy for table \"bt\"");
    c.batch_execute("INSERT INTO bv_a VALUES (6, 'rls_vw_b')").expect("insert through view");
    assert_eq!(c.execute("UPDATE bv_a SET id = id + 10", &[]).expect("update through view"), 2);
    // COPY TO shows the visible rows; COPY FROM is refused.
    c.batch_execute("SET ROLE rls_vw_a").expect("set role");
    let mut out = String::new();
    std::io::Read::read_to_string(&mut c.copy_out("COPY bt TO STDOUT").expect("copy out"), &mut out).expect("read");
    assert_eq!(out, "1\trls_vw_a\n");
    let e = expect_code(c.batch_execute("COPY bt FROM STDIN"), "0A000");
    assert_eq!(message(&e), "COPY FROM not supported with row-level security");
    assert_eq!(hint(&e).as_deref(), Some("Use INSERT statements instead."));
    c.batch_execute("RESET ROLE").expect("reset");
    assert_eq!(rows(&mut c, "SELECT id, who FROM bt ORDER BY id"), [["1", "rls_vw_a"], ["3", "x"], ["12", "rls_vw_b"], ["16", "rls_vw_b"]]);
    c.batch_execute("DROP VIEW bv_owner, bv_a, bv_inv; DROP TABLE bt").expect("drop");
    drop_roles(&mut c, &roles);
}

#[test]
fn policies_reading_their_own_table_recurse() {
    let (_db, mut c) = setup();
    let roles = ["rls_rec_a"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE rec (id int);
         CREATE TABLE other (id int);
         INSERT INTO other VALUES (1), (2);
         INSERT INTO rec VALUES (1), (3);
         ALTER TABLE rec ENABLE ROW LEVEL SECURITY;
         GRANT ALL ON rec, other TO PUBLIC;
         CREATE POLICY via_other ON rec USING (id IN (SELECT id FROM other));
         SET ROLE rls_rec_a",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT id FROM rec"), [["1"]]);
    c.batch_execute("RESET ROLE; CREATE POLICY self ON rec USING (id IN (SELECT id FROM rec)); SET ROLE rls_rec_a").expect("policy");
    let e = expect_code(c.batch_execute("SELECT * FROM rec"), "42P17");
    assert_eq!(message(&e), "infinite recursion detected in policy for relation \"rec\"");
    c.batch_execute("RESET ROLE; DROP TABLE rec, other").expect("drop");
    drop_roles(&mut c, &roles);
}

#[test]
fn instead_of_triggers_see_rows_through_policies() {
    let (_db, mut c) = setup();
    let roles = ["rls_io_a", "rls_io_own"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE it (id int, who name);
         ALTER TABLE it OWNER TO rls_io_own;
         INSERT INTO it VALUES (1, 'a'), (2, 'b'), (3, 'c');
         ALTER TABLE it ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;
         CREATE POLICY own ON it TO rls_io_own USING (id >= 2);
         CREATE POLICY a ON it TO rls_io_a USING (id = 1);
         GRANT ALL ON it TO PUBLIC;
         CREATE TABLE ilog (id int);
         GRANT ALL ON ilog TO PUBLIC;
         CREATE VIEW iv AS SELECT * FROM it;
         ALTER VIEW iv OWNER TO rls_io_own;
         CREATE VIEW iv_inv WITH (security_invoker) AS SELECT * FROM it;
         GRANT ALL ON iv, iv_inv TO PUBLIC;
         CREATE FUNCTION ilog_f() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN
             IF TG_OP = 'INSERT' THEN INSERT INTO ilog VALUES (NEW.id); RETURN NEW; END IF;
             INSERT INTO ilog VALUES (OLD.id);
             RETURN OLD;
           END $$;
         CREATE TRIGGER t1 INSTEAD OF INSERT OR UPDATE OR DELETE ON iv FOR EACH ROW EXECUTE FUNCTION ilog_f();
         CREATE TRIGGER t2 INSTEAD OF INSERT OR UPDATE OR DELETE ON iv_inv FOR EACH ROW EXECUTE FUNCTION ilog_f();
         SET ROLE rls_io_a",
    )
    .expect("setup");
    let logged = |c: &mut postgres::Client, sql: &str| {
        c.batch_execute(sql).expect(sql);
        let r = rows(c, "SELECT id FROM ilog ORDER BY id");
        c.batch_execute("DELETE FROM ilog").expect("clear log");
        r
    };
    // The view's owner's policies apply to the OLD rows; a security_invoker view's the invoker's.
    assert_eq!(logged(&mut c, "DELETE FROM iv"), [["2"], ["3"]]);
    assert_eq!(logged(&mut c, "UPDATE iv SET who = 'z' WHERE true"), [["2"], ["3"]]);
    assert_eq!(logged(&mut c, "DELETE FROM iv_inv"), [["1"]]);
    assert_eq!(logged(&mut c, "UPDATE iv_inv SET who = 'z'"), [["1"]]);
    // INSERT ... SELECT sources and WHERE subqueries read tables through their policies.
    assert_eq!(logged(&mut c, "INSERT INTO iv_inv SELECT * FROM it"), [["1"]]);
    assert_eq!(logged(&mut c, "DELETE FROM iv WHERE id IN (SELECT id + 1 FROM it)"), [["2"]]);
    c.batch_execute("RESET ROLE; DROP VIEW iv, iv_inv; DROP TABLE it, ilog; DROP FUNCTION ilog_f()").expect("drop");
    drop_roles(&mut c, &roles);
}

#[test]
fn execute_parameters_and_call_arguments_reject_subqueries() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE sq (secret text);
         INSERT INTO sq VALUES ('s');
         PREPARE sq_p(text) AS SELECT $1;
         CREATE PROCEDURE sq_proc(INOUT x text) LANGUAGE sql AS $$ SELECT x $$",
    )
    .expect("setup");
    for (sql, msg) in [
        ("EXECUTE sq_p((SELECT secret FROM sq))", "cannot use subquery in EXECUTE parameter"),
        ("EXECUTE sq_p((1 IN (SELECT 1))::text)", "cannot use subquery in EXECUTE parameter"),
        ("EXECUTE sq_p(count(*)::text)", "aggregate functions are not allowed in EXECUTE parameters"),
        ("CALL sq_proc((SELECT secret FROM sq))", "cannot use subquery in CALL argument"),
        ("CALL sq_proc(EXISTS (SELECT 1)::text)", "cannot use subquery in CALL argument"),
        ("CALL sq_proc(count(*)::text)", "aggregate functions are not allowed in CALL arguments"),
    ] {
        let e = c.batch_execute(sql).expect_err(sql);
        assert_eq!(message(&e), msg, "{sql}");
        assert!(matches!(crate::support::sqlstate(&e).as_str(), "0A000" | "42803"), "{sql}: {e:?}");
    }
    assert_eq!(rows(&mut c, "EXECUTE sq_p('x')"), [["x"]]);
    c.batch_execute("CALL sq_proc('y')").expect("call");
}

#[test]
fn policies_resolve_names_when_created() {
    let (_db, mut c) = setup();
    let roles = ["rls_sp_a"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE SCHEMA app;
         CREATE FUNCTION app.f() RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE TABLE pt (id int, t int);
         INSERT INTO pt VALUES (1, 1), (2, 2), (3, 3);
         CREATE TABLE ref (id int);
         INSERT INTO ref VALUES (3);
         GRANT ALL ON pt, ref TO PUBLIC;
         GRANT USAGE ON SCHEMA app TO PUBLIC;
         ALTER TABLE pt ENABLE ROW LEVEL SECURITY;
         SET search_path = app, public;
         CREATE POLICY p ON pt USING (pt.t = f() OR id IN (SELECT id FROM ref));
         RESET search_path;
         CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS 'SELECT 2'",
    )
    .expect("setup");
    let check = |c: &mut postgres::Client| {
        c.batch_execute("SET ROLE rls_sp_a").expect("set role");
        let r = rows(c, "SELECT id FROM pt2 ORDER BY id");
        c.batch_execute("RESET ROLE").expect("reset");
        r
    };
    // The policy calls app.f() whatever the session's search_path finds first.
    c.batch_execute("SET ROLE rls_sp_a").expect("set role");
    assert_eq!(rows(&mut c, "SELECT id FROM pt ORDER BY id"), [["1"], ["3"]]);
    c.batch_execute("SET search_path = public, app").expect("search_path");
    assert_eq!(rows(&mut c, "SELECT id FROM pt ORDER BY id"), [["1"], ["3"]]);
    c.batch_execute("RESET ROLE; RESET search_path").expect("reset");
    // Renaming the policy's table and the table its subquery reads keeps the policy working.
    c.batch_execute("ALTER TABLE ref RENAME TO ref2; ALTER TABLE pt RENAME TO pt2").expect("rename");
    assert_eq!(check(&mut c), [["1"], ["3"]]);
    // Dropping a table a policy reads needs CASCADE, which drops the policy.
    let e = expect_code(c.batch_execute("DROP TABLE ref2"), "2BP01");
    assert_eq!(message(&e), "cannot drop table ref2 because other objects depend on it");
    assert_eq!(detail(&e).as_deref(), Some("policy p on table pt2 depends on table ref2"));
    assert_eq!(hint(&e).as_deref(), Some("Use DROP ... CASCADE to drop the dependent objects too."));
    c.batch_execute("DROP TABLE ref2 CASCADE").expect("drop cascade");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_policy WHERE polname = 'p'"), [["0"]]);
    assert_eq!(check(&mut c), Vec::<Vec<String>>::new());
    c.batch_execute("DROP TABLE pt2; DROP FUNCTION public.f(); DROP SCHEMA app CASCADE").expect("drop");
    drop_roles(&mut c, &roles);
}

#[test]
fn locking_selects_add_update_policies() {
    let (_db, mut c) = setup();
    let roles = ["rls_fu_a"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE fu (id int, who name);
         INSERT INTO fu VALUES (1, 'rls_fu_a'), (2, 'x');
         CREATE TABLE fo (id int);
         INSERT INTO fo VALUES (1), (2);
         GRANT ALL ON fu, fo TO PUBLIC;
         ALTER TABLE fu ENABLE ROW LEVEL SECURITY;
         CREATE POLICY s ON fu FOR SELECT USING (true);
         CREATE POLICY u ON fu FOR UPDATE USING (who = current_user);
         SET ROLE rls_fu_a",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT id FROM fu ORDER BY 1"), [["1"], ["2"]]);
    assert_eq!(rows(&mut c, "SELECT id FROM fu ORDER BY 1 FOR UPDATE"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT id FROM fu ORDER BY 1 FOR KEY SHARE"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT fu.id FROM fu, fo WHERE fu.id = fo.id ORDER BY 1 FOR UPDATE OF fo"), [["1"], ["2"]]);
    assert_eq!(rows(&mut c, "SELECT x.id FROM fu x, fo WHERE x.id = fo.id ORDER BY 1 FOR SHARE OF x"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT id FROM (SELECT * FROM fu) s ORDER BY 1 FOR UPDATE"), [["1"]]);
    assert_eq!(rows(&mut c, "SELECT (SELECT count(*) FROM fu) FROM fo FOR UPDATE"), [["2"], ["2"]]);
    c.batch_execute("RESET ROLE; DROP TABLE fu, fo").expect("drop");
    drop_roles(&mut c, &roles);
}

#[test]
fn refresh_materialized_view_applies_the_owners_policies() {
    let (_db, mut c) = setup();
    let roles = ["rls_mv_own"];
    create_roles(&mut c, &roles);
    c.batch_execute(
        "CREATE TABLE mt (id int);
         INSERT INTO mt VALUES (1), (2);
         GRANT ALL ON mt TO PUBLIC;
         ALTER TABLE mt ENABLE ROW LEVEL SECURITY;
         CREATE POLICY p ON mt USING (id = 1);
         CREATE MATERIALIZED VIEW mmv AS SELECT * FROM mt;
         ALTER MATERIALIZED VIEW mmv OWNER TO rls_mv_own",
    )
    .expect("setup");
    assert_eq!(rows(&mut c, "SELECT count(*) FROM mmv"), [["2"]]);
    c.batch_execute("REFRESH MATERIALIZED VIEW mmv").expect("refresh");
    assert_eq!(rows(&mut c, "SELECT id FROM mmv"), [["1"]]);
    c.batch_execute("DROP MATERIALIZED VIEW mmv; DROP TABLE mt").expect("drop");
    drop_roles(&mut c, &roles);
}
