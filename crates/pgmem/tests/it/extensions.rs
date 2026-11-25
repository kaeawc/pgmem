//! CREATE / DROP EXTENSION and the functions and types of pgcrypto, uuid-ossp, citext,
//! pg_trgm and hstore.

use crate::support::{expect_code, message, setup, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn install(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE EXTENSION IF NOT EXISTS pgcrypto; CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"; CREATE EXTENSION IF NOT EXISTS citext;
         CREATE EXTENSION IF NOT EXISTS pg_trgm; CREATE EXTENSION IF NOT EXISTS hstore",
    )
    .expect("extensions");
}

#[test]
fn extension_ddl_and_catalog() {
    let (_db, mut c) = setup();
    install(&mut c);
    let e = expect_code(c.batch_execute("CREATE EXTENSION pgcrypto"), "42710");
    assert_eq!(message(&e), "extension \"pgcrypto\" already exists");
    let e = expect_code(c.batch_execute("CREATE EXTENSION no_such_extension"), "0A000");
    assert_eq!(message(&e), "extension \"no_such_extension\" is not available");
    assert_eq!(
        rows(
            &mut c,
            "SELECT extname FROM pg_extension WHERE extname IN ('citext', 'hstore', 'pg_trgm', 'pgcrypto', 'uuid-ossp') ORDER BY 1"
        ),
        [["citext"], ["hstore"], ["pg_trgm"], ["pgcrypto"], ["uuid-ossp"]]
    );
    c.batch_execute("CREATE TABLE members (email citext PRIMARY KEY)").expect("citext column");
    let e = expect_code(c.batch_execute("DROP EXTENSION citext"), "2BP01");
    assert_eq!(message(&e), "cannot drop extension citext because other objects depend on it");
    c.batch_execute("DROP EXTENSION hstore; DROP EXTENSION IF EXISTS hstore").expect("drop");
    let e = expect_code(c.batch_execute("DROP EXTENSION hstore"), "42704");
    assert_eq!(message(&e), "extension \"hstore\" does not exist");
}

#[test]
fn pgcrypto_and_uuid_ossp() {
    let (_db, mut c) = setup();
    install(&mut c);
    assert_eq!(
        rows(
            &mut c,
            "SELECT encode(digest('abc', 'sha256'), 'hex'), encode(digest('abc', 'sha1'), 'hex'), encode(hmac('data', 'key', 'sha1'), 'hex')"
        ),
        [[
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            "104152c5bfdca07bc633eebd46199f0255c9f49d"
        ]]
    );
    let e = expect_code(c.batch_execute("SELECT digest('x', 'nope')"), "22023");
    assert_eq!(message(&e), "Cannot use \"nope\": No such hash algorithm");
    assert_eq!(rows(&mut c, "SELECT crypt('pw', '$1$abcdefgh$')"), [["$1$abcdefgh$IQtUouv7y7Q9dRWkQEPCc."]]);
    // A bcrypt hash verifies with crypt(password, hash), as applications check passwords.
    assert_eq!(
        rows(&mut c, "SELECT crypt('hunter2', h) = h, crypt('wrong', h) = h FROM (SELECT crypt('hunter2', gen_salt('bf', 4)) AS h) s"),
        [["t", "f"]]
    );
    assert_eq!(rows(&mut c, "SELECT length(gen_random_bytes(16)), substr(gen_salt('bf'), 1, 7)"), [["16", "$2a$06$"]]);
    assert_eq!(
        rows(
            &mut c,
            "SELECT uuid_generate_v5(uuid_ns_url(), 'https://example.com'), uuid_generate_v3(uuid_ns_dns(), 'example.com'), uuid_nil()"
        ),
        [["4fd35a71-71ef-5a55-a9d9-aa75c889a6d0", "9073926b-929f-31c2-abc9-fad77ae3e8eb", "00000000-0000-0000-0000-000000000000"]]
    );
    assert_eq!(rows(&mut c, "SELECT uuid_generate_v4() <> uuid_generate_v4(), substr(uuid_generate_v1()::text, 15, 1)"), [["t", "1"]]);
}

#[test]
fn citext_trgm_and_hstore() {
    let (_db, mut c) = setup();
    install(&mut c);
    c.batch_execute("CREATE TABLE members (email citext PRIMARY KEY); INSERT INTO members VALUES ('Alice@Example.com')").expect("setup");
    assert_eq!(rows(&mut c, "SELECT email FROM members WHERE email = 'alice@example.COM'"), [["Alice@Example.com"]]);
    expect_code(c.batch_execute("INSERT INTO members VALUES ('ALICE@example.com')"), "23505");
    assert_eq!(
        rows(
            &mut c,
            "SELECT 'ABC'::citext = 'abc'::citext, 'a'::citext < 'B'::citext, 'abc'::citext LIKE 'A%', pg_typeof('x'::citext)::text"
        ),
        [["t", "t", "t", "citext"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT similarity('word', 'words'), word_similarity('word', 'two words'), 'postgres' % 'postgre', show_trgm('cat')"),
        [["0.5714286", "0.8", "t", "{\"  c\",\" ca\",\"at \",cat}"]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT 'a=>1, b=>2'::hstore -> 'b', 'a=>1, b=>2'::hstore ? 'a', 'a=>1'::hstore || 'c=>3'::hstore, akeys('x=>1, y=>2'::hstore), hstore('k', 'v')"
        ),
        [["2", "t", "\"a\"=>\"1\", \"c\"=>\"3\"", "{x,y}", "\"k\"=>\"v\""]]
    );
    assert_eq!(
        rows(&mut c, "SELECT 'b=>2, a=>1, a=>3'::hstore, hstore_to_jsonb('a=>1, b=>NULL')"),
        [["\"a\"=>\"1\", \"b\"=>\"2\"", "{\"a\": \"1\", \"b\": null}"]]
    );
}
