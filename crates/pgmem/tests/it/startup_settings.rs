//! Settings a session starts with: the startup packet's `options` (`-c name=value`,
//! `--name=value`) and plain parameters, and `ALTER ROLE` / `ALTER DATABASE ... SET`
//! (`pg_db_role_setting`), with their precedence, `pg_settings.source`, and RESET going back
//! to them. Roles and `ALTER ROLE ALL SET` are cluster-wide on PostgreSQL: names are unique to
//! this module, the tests that create them run one at a time, and they are dropped at the end.
//! Database settings go with the per-test database.

use crate::support::{RawConn, TestDb, message, sqlstate, text_value, texts};
use postgres::{Client, Config, NoTls};

/// Roles and global settings are cluster-wide: the tests that touch them run one at a time.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn config(db: &TestDb) -> Config {
    db.conninfo().parse().expect("conninfo")
}

/// Rows as text (NULL as "NULL").
fn rows(c: &mut Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

/// (setting, source, reset_val) of each named parameter, by name.
fn sources(c: &mut Client, names: &str) -> Vec<Vec<String>> {
    rows(c, &format!("SELECT name, setting, source, reset_val FROM pg_settings WHERE name IN ({names}) ORDER BY name"))
}

fn current_db(c: &mut Client) -> String {
    text_value(c, "SELECT current_database()").expect("current_database")
}

#[test]
fn startup_options_are_session_defaults() {
    let db = TestDb::new();
    let mut c = config(&db)
        .options("-c search_path=other,public --work-mem=2MB  -c application_name=a\\ b -cstartup_t.custom=x")
        .connect(NoTls)
        .expect("connect with options");
    assert_eq!(
        rows(
            &mut c,
            "SELECT current_setting('search_path'), current_setting('work_mem'), current_setting('application_name'), current_setting('startup_t.custom')"
        ),
        [["other,public", "2MB", "a b", "x"]]
    );
    let client = [
        ["application_name", "a b", "client", "a b"],
        ["search_path", "other,public", "client", "other,public"],
        ["work_mem", "2048", "client", "2048"],
    ];
    assert_eq!(sources(&mut c, "'search_path', 'work_mem', 'application_name'"), client);
    c.batch_execute("SET search_path = public; SET work_mem = '8MB'").expect("set");
    assert_eq!(
        sources(&mut c, "'search_path', 'work_mem'"),
        [["search_path", "public", "session", "other,public"], ["work_mem", "8192", "session", "2048"]]
    );
    // RESET and SET ... TO DEFAULT restore the startup value, not the built-in default.
    c.batch_execute("RESET search_path; SET work_mem TO DEFAULT").expect("reset");
    assert_eq!(rows(&mut c, "SHOW search_path"), [["other,public"]]);
    assert_eq!(rows(&mut c, "SHOW work_mem"), [["2MB"]]);
    c.batch_execute("SET search_path = public; SET work_mem = '8MB'; SET application_name = 'z'; RESET ALL").expect("reset all");
    assert_eq!(sources(&mut c, "'search_path', 'work_mem', 'application_name'"), client);
    c.batch_execute("BEGIN; SET LOCAL search_path = public; COMMIT").expect("set local");
    assert_eq!(rows(&mut c, "SHOW search_path"), [["other,public"]]);
    // default_transaction_isolation applies; transaction_isolation is per transaction.
    let mut c = config(&db)
        .options("--default-transaction-isolation=serializable -c transaction_isolation=repeatable\\ read")
        .connect(NoTls)
        .expect("connect");
    assert_eq!(rows(&mut c, "SELECT current_setting('transaction_isolation')"), [["serializable"]]);
}

#[test]
fn startup_option_errors_fail_the_connection() {
    let db = TestDb::new();
    let cases = [
        ("-c startup_t_nosuch=1", "42704", "unrecognized configuration parameter \"startup_t_nosuch\""),
        ("-c work_mem=abc", "22023", "invalid value for parameter \"work_mem\": \"abc\""),
        (
            "-c statement_timeout=-5",
            "22023",
            "-5 ms is outside the valid range for parameter \"statement_timeout\" (0 ms .. 2147483647 ms)",
        ),
        ("--search_path=a\\ b,c", "22023", "invalid value for parameter \"search_path\": \"a b,c\""),
        ("-c shared_buffers=1MB", "55P02", "parameter \"shared_buffers\" cannot be changed without restarting the server"),
        ("-c server_version=1", "55P02", "parameter \"server_version\" cannot be changed"),
        ("-c is_superuser=off", "55P02", "parameter \"is_superuser\" cannot be changed"),
        ("-c archive_command=x", "55P02", "parameter \"archive_command\" cannot be changed now"),
        ("-c search_path", "42601", "-c search_path requires a value"),
        ("--work_mem", "42601", "--work_mem requires a value"),
        ("search_path=x", "42601", "invalid command-line argument for server process: search_path=x"),
    ];
    for (options, code, msg) in cases {
        let Err(e) = config(&db).options(options).connect(NoTls) else { panic!("{options}: connected") };
        assert_eq!((sqlstate(&e), message(&e)), (code.to_owned(), msg.to_owned()), "{options}");
    }
    // Backend-context parameters can be set at connection start.
    let mut c = config(&db).options("-c ignore_system_indexes=on").connect(NoTls).expect("backend parameter");
    assert_eq!(rows(&mut c, "SHOW ignore_system_indexes"), [["on"]]);
}

#[test]
fn plain_startup_parameters_are_settings() {
    let (db, mut c) = crate::support::setup();
    let mut raw = RawConn::connect_with(&db, &mut c, &[("search_path", "plain, public"), ("startup_t.custom", "y")]).expect("connect");
    let r = raw.query(
        "SELECT concat_ws('|', current_setting('search_path'), current_setting('startup_t.custom'), source) FROM pg_settings WHERE name = 'search_path'",
    );
    assert_eq!(r.values, [Some("plain, public|y|client".into())]);
    // Parameters are applied after the options switches.
    let mut raw = RawConn::connect_with(&db, &mut c, &[("options", "-c search_path=opt -c work_mem=2MB"), ("search_path", "param")])
        .expect("connect");
    let r = raw.query("SELECT concat_ws('|', current_setting('search_path'), current_setting('work_mem'))");
    assert_eq!(r.values, [Some("param|2MB".into())]);
    for (params, code, msg) in [
        (("startup_t_nosuch", "1"), "42704", "unrecognized configuration parameter \"startup_t_nosuch\""),
        (("work_mem", "abc"), "22023", "invalid value for parameter \"work_mem\": \"abc\""),
        (("server_version", "1"), "55P02", "parameter \"server_version\" cannot be changed"),
    ] {
        let e = RawConn::connect_with(&db, &mut c, &[params]).err().expect("refused");
        assert_eq!(e, (code.to_owned(), msg.to_owned()));
    }
}

const ROLE: &str = "startup_t_role";

fn cleanup(c: &mut Client) {
    let _ = c.batch_execute(&format!("DROP ROLE IF EXISTS {ROLE}"));
    let _ = c.batch_execute("ALTER ROLE ALL RESET startup_t.global");
}

#[test]
fn role_and_database_settings_apply_to_new_sessions() {
    let _lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (db, mut c) = crate::support::setup();
    cleanup(&mut c);
    let dbname = current_db(&mut c);
    c.batch_execute(&format!(
        "CREATE ROLE {ROLE} LOGIN;
         ALTER ROLE {ROLE} SET work_mem = '3MB';
         ALTER ROLE {ROLE} SET statement_timeout = '7s';
         ALTER ROLE {ROLE} SET search_path = role_s, public;
         ALTER DATABASE \"{dbname}\" SET search_path = db_s, public;
         ALTER DATABASE \"{dbname}\" SET statement_timeout = '9s';
         ALTER DATABASE \"{dbname}\" SET lock_timeout = '4s';
         ALTER ROLE ALL IN DATABASE \"{dbname}\" SET idle_in_transaction_session_timeout = '11s';
         ALTER ROLE {ROLE} IN DATABASE \"{dbname}\" SET search_path = dbrole_s, public;
         ALTER ROLE ALL SET startup_t.global = 'g'"
    ))
    .expect("settings");
    let names = "'search_path', 'work_mem', 'statement_timeout', 'lock_timeout', 'idle_in_transaction_session_timeout'";
    // Role in database > role > database (> ALTER ROLE ALL).
    let mut r = config(&db).user(ROLE).connect(NoTls).expect("connect as role");
    let session = [
        ["idle_in_transaction_session_timeout", "11000", "database", "11000"],
        ["lock_timeout", "4000", "database", "4000"],
        ["search_path", "dbrole_s, public", "database user", "dbrole_s, public"],
        ["statement_timeout", "7000", "user", "7000"],
        ["work_mem", "3072", "user", "3072"],
    ];
    assert_eq!(sources(&mut r, names), session);
    assert_eq!(rows(&mut r, "SELECT current_setting('startup_t.global')"), [["g"]]);
    r.batch_execute("SET search_path = public; SET statement_timeout = 0; RESET search_path").expect("set");
    assert_eq!(rows(&mut r, "SHOW search_path"), [["dbrole_s, public"]]);
    r.batch_execute("RESET ALL").expect("reset all");
    assert_eq!(sources(&mut r, names), session);
    // Startup options override them all.
    let mut r = config(&db).user(ROLE).options("-c search_path=opt -c lock_timeout=1s").connect(NoTls).expect("connect");
    assert_eq!(
        sources(&mut r, "'search_path', 'lock_timeout', 'work_mem'"),
        [["lock_timeout", "1000", "client", "1000"], ["search_path", "opt", "client", "opt"], ["work_mem", "3072", "user", "3072"]]
    );
    // Another role gets the database settings only.
    let mut admin = db.connect();
    assert_eq!(
        sources(&mut admin, "'search_path', 'statement_timeout', 'work_mem'"),
        [
            ["search_path", "db_s, public", "database", "db_s, public"],
            ["statement_timeout", "9000", "database", "9000"],
            ["work_mem", "4096", "default", "4096"]
        ]
    );
    // Sessions already open keep their settings.
    assert_eq!(rows(&mut c, "SHOW search_path"), [["\"$user\", public"]]);
    cleanup(&mut c);
}

#[test]
fn alter_role_and_database_set_store_pg_db_role_setting() {
    let _lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_db, mut c) = crate::support::setup();
    cleanup(&mut c);
    let dbname = current_db(&mut c);
    let ours = format!(
        "SELECT setdatabase = (SELECT oid FROM pg_database WHERE datname = current_database()), setrole::regrole::text, setconfig::text \
         FROM pg_db_role_setting WHERE setrole = (SELECT oid FROM pg_roles WHERE rolname = '{ROLE}') \
            OR setdatabase = (SELECT oid FROM pg_database WHERE datname = current_database()) ORDER BY 1, 2"
    );
    c.batch_execute(&format!(
        "CREATE ROLE {ROLE};
         ALTER ROLE {ROLE} SET search_path = 'x', \"Y\", z;
         ALTER ROLE {ROLE} SET work_mem = '8MB';
         ALTER USER {ROLE} SET datestyle = german;
         ALTER ROLE {ROLE} SET statement_timeout = 1000;
         ALTER ROLE {ROLE} SET startup_t.custom = 1.50;
         ALTER ROLE {ROLE} SET time zone 'UTC';
         ALTER ROLE {ROLE} SET application_name = 'my app';
         ALTER ROLE {ROLE} SET datestyle = iso, dmy;
         ALTER ROLE {ROLE} IN DATABASE \"{dbname}\" SET lock_timeout = '5s';
         ALTER DATABASE \"{dbname}\" SET statement_timeout = '5s';
         ALTER ROLE ALL IN DATABASE \"{dbname}\" SET search_path = d"
    ))
    .expect("alter role set");
    assert_eq!(
        rows(&mut c, &ours),
        [
            [
                "f",
                ROLE,
                "{\"search_path=x, \\\"Y\\\", z\",work_mem=8MB,\"DateStyle=iso, dmy\",statement_timeout=1000,startup_t.custom=1.50,TimeZone=UTC,\"application_name=my app\"}"
            ],
            ["t", "-", "{statement_timeout=5s,search_path=d}"],
            ["t", ROLE, "{lock_timeout=5s}"],
        ]
    );
    assert_eq!(rows(&mut c, &format!("SELECT array_length(rolconfig, 1) FROM pg_roles WHERE rolname = '{ROLE}'")), [["7"]]);
    // psql's \drds.
    assert_eq!(
        rows(
            &mut c,
            &format!(
                "SELECT rolname AS \"Role\", datname = current_database() AS \"Database\", \
                 pg_catalog.array_to_string(setconfig, E'\\n') AS \"Settings\" \
                 FROM pg_catalog.pg_db_role_setting s \
                 LEFT JOIN pg_catalog.pg_database d ON d.oid = setdatabase \
                 LEFT JOIN pg_catalog.pg_roles r ON r.oid = setrole \
                 WHERE r.rolname = '{ROLE}' OR d.datname = current_database() ORDER BY 1, 2"
            )
        ),
        [
            [ROLE, "t", "lock_timeout=5s"],
            [
                ROLE,
                "NULL",
                "search_path=x, \"Y\", z\nwork_mem=8MB\nDateStyle=iso, dmy\nstatement_timeout=1000\nstartup_t.custom=1.50\nTimeZone=UTC\napplication_name=my app"
            ],
            ["NULL", "t", "statement_timeout=5s\nsearch_path=d"],
        ]
    );
    // Values are checked when the statement runs.
    let check = |c: &mut Client, sql: &str, code: &str, msg: &str| {
        let e = c.batch_execute(sql).expect_err(sql);
        assert_eq!((sqlstate(&e), message(&e)), (code.to_owned(), msg.to_owned()), "{sql}");
    };
    check(&mut c, &format!("ALTER ROLE {ROLE} SET work_mem = 'abc'"), "22023", "invalid value for parameter \"work_mem\": \"abc\"");
    check(
        &mut c,
        &format!("ALTER ROLE {ROLE} SET startup_t_nosuch = 1"),
        "42704",
        "unrecognized configuration parameter \"startup_t_nosuch\"",
    );
    check(
        &mut c,
        &format!("ALTER ROLE {ROLE} RESET startup_t_nosuch"),
        "42704",
        "unrecognized configuration parameter \"startup_t_nosuch\"",
    );
    check(&mut c, &format!("ALTER ROLE {ROLE} SET server_version = '1'"), "55P02", "parameter \"server_version\" cannot be changed");
    check(
        &mut c,
        &format!("ALTER ROLE {ROLE} SET shared_buffers = '1MB'"),
        "55P02",
        "parameter \"shared_buffers\" cannot be changed without restarting the server",
    );
    check(
        &mut c,
        &format!("ALTER ROLE {ROLE} SET ignore_system_indexes = on"),
        "55P02",
        "parameter \"ignore_system_indexes\" cannot be set after connection start",
    );
    check(&mut c, &format!("ALTER ROLE {ROLE} SET archive_command = 'x'"), "55P02", "parameter \"archive_command\" cannot be changed now");
    check(
        &mut c,
        &format!("ALTER ROLE {ROLE} SET transaction_isolation = 'serializable'"),
        "25001",
        "SET TRANSACTION ISOLATION LEVEL must be called before any query",
    );
    check(&mut c, "ALTER ROLE startup_t_nosuch SET work_mem = '1MB'", "42704", "role \"startup_t_nosuch\" does not exist");
    check(&mut c, "ALTER DATABASE startup_t_nosuch SET work_mem = '1MB'", "3D000", "database \"startup_t_nosuch\" does not exist");
    check(
        &mut c,
        &format!("ALTER ROLE {ROLE} IN DATABASE startup_t_nosuch SET work_mem = '1MB'"),
        "3D000",
        "database \"startup_t_nosuch\" does not exist",
    );
    // RESET name, SET ... TO DEFAULT, RESET ALL; a rolled-back ALTER leaves no trace.
    c.batch_execute(&format!(
        "ALTER ROLE {ROLE} RESET work_mem;
         ALTER ROLE {ROLE} RESET lock_timeout;
         ALTER ROLE {ROLE} SET search_path TO DEFAULT;
         ALTER ROLE {ROLE} SET search_path = DEFAULT;
         ALTER ROLE {ROLE} IN DATABASE \"{dbname}\" RESET ALL;
         ALTER DATABASE \"{dbname}\" RESET search_path"
    ))
    .expect("reset");
    c.batch_execute("BEGIN").expect("begin");
    c.batch_execute(&format!("ALTER ROLE {ROLE} SET startup_t.rolled = 1")).expect("set");
    c.batch_execute("ROLLBACK").expect("rollback");
    assert_eq!(
        rows(&mut c, &ours),
        [
            ["f", ROLE, "{\"DateStyle=iso, dmy\",statement_timeout=1000,startup_t.custom=1.50,TimeZone=UTC,\"application_name=my app\"}"],
            ["t", "-", "{statement_timeout=5s}"],
        ]
    );
    c.batch_execute(&format!("ALTER ROLE {ROLE} RESET ALL; ALTER DATABASE \"{dbname}\" RESET ALL")).expect("reset all");
    assert!(rows(&mut c, &ours).is_empty());
    assert_eq!(rows(&mut c, &format!("SELECT rolconfig FROM pg_roles WHERE rolname = '{ROLE}'")), [["NULL"]]);
    // DROP ROLE removes the role's settings.
    c.batch_execute(&format!("ALTER ROLE {ROLE} SET work_mem = '1MB'")).expect("set");
    let oid = text_value(&mut c, &format!("SELECT oid FROM pg_roles WHERE rolname = '{ROLE}'")).expect("oid");
    c.batch_execute(&format!("DROP ROLE {ROLE}")).expect("drop role");
    assert_eq!(rows(&mut c, &format!("SELECT count(*) FROM pg_db_role_setting WHERE setrole = {oid}")), [["0"]]);
    cleanup(&mut c);
}
