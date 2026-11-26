//! pg_catalog and information_schema: the introspection queries drivers, ORMs and psql run.

use crate::support::{expect_code, setup, text_value, texts};

fn rows(c: &mut postgres::Client, sql: &str) -> Vec<Vec<String>> {
    texts(c, sql).into_iter().map(|r| r.into_iter().map(|v| v.unwrap_or_else(|| "NULL".into())).collect()).collect()
}

fn schema(c: &mut postgres::Client) {
    c.batch_execute(
        "CREATE TABLE authors (id serial PRIMARY KEY, name varchar(40) NOT NULL, email text UNIQUE, rating numeric(3,1));
         CREATE TABLE books (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                             author_id int NOT NULL REFERENCES authors (id) ON DELETE CASCADE, title text);
         CREATE INDEX books_title_idx ON books (title);
         CREATE VIEW author_names AS SELECT id, name FROM authors;",
    )
    .expect("schema");
}

#[test]
fn pg_class_lists_relations_by_kind() {
    let (_db, mut c) = setup();
    schema(&mut c);
    let got = rows(
        &mut c,
        "SELECT c.relname, c.relkind, c.relnatts FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' ORDER BY c.relname",
    );
    let want: Vec<Vec<&str>> = vec![
        vec!["author_names", "v", "2"],
        vec!["authors", "r", "4"],
        vec!["authors_email_key", "i", "1"],
        vec!["authors_id_seq", "S", "3"],
        vec!["authors_pkey", "i", "1"],
        vec!["books", "r", "3"],
        vec!["books_id_seq", "S", "3"],
        vec!["books_pkey", "i", "1"],
        vec!["books_title_idx", "i", "1"],
    ];
    assert_eq!(got, want);
}

#[test]
fn pg_attribute_and_format_type() {
    let (_db, mut c) = setup();
    schema(&mut c);
    let got = rows(
        &mut c,
        "SELECT attname, format_type(atttypid, atttypmod), attnotnull, atthasdef, attidentity FROM pg_attribute
         WHERE attrelid = 'authors'::regclass AND attnum > 0 AND NOT attisdropped ORDER BY attnum",
    );
    assert_eq!(
        got,
        vec![
            vec!["id", "integer", "t", "t", ""],
            vec!["name", "character varying(40)", "t", "f", ""],
            vec!["email", "text", "f", "f", ""],
            vec!["rating", "numeric(3,1)", "f", "f", ""],
        ]
    );
    // System columns sit at negative attnums.
    assert_eq!(
        text_value(&mut c, "SELECT attname FROM pg_attribute WHERE attrelid = 'books'::regclass AND attnum = -1").as_deref(),
        Some("ctid")
    );
    assert_eq!(
        text_value(&mut c, "SELECT attidentity FROM pg_attribute WHERE attrelid = 'books'::regclass AND attname = 'id'").as_deref(),
        Some("a")
    );
}

#[test]
fn pg_constraint_includes_not_null_constraints() {
    let (_db, mut c) = setup();
    schema(&mut c);
    let got = rows(
        &mut c,
        "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'books'::regclass ORDER BY conname",
    );
    assert_eq!(
        got,
        vec![
            vec!["books_author_id_fkey", "f", "FOREIGN KEY (author_id) REFERENCES authors(id) ON DELETE CASCADE"],
            vec!["books_author_id_not_null", "n", "NOT NULL author_id"],
            vec!["books_id_not_null", "n", "NOT NULL id"],
            vec!["books_pkey", "p", "PRIMARY KEY (id)"],
        ]
    );
    // The foreign key references the primary key's index.
    assert_eq!(
        text_value(
            &mut c,
            "SELECT i.relname FROM pg_constraint k JOIN pg_class i ON i.oid = k.conindid WHERE k.conname = 'books_author_id_fkey'"
        )
        .as_deref(),
        Some("authors_pkey")
    );
}

#[test]
fn pg_index_keys_and_indexdef() {
    let (_db, mut c) = setup();
    schema(&mut c);
    let got = rows(
        &mut c,
        "SELECT i.relname, x.indisunique, x.indisprimary, x.indkey, pg_get_indexdef(x.indexrelid)
         FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = 'books'::regclass ORDER BY i.relname",
    );
    assert_eq!(
        got,
        vec![
            vec!["books_pkey", "t", "t", "1", "CREATE UNIQUE INDEX books_pkey ON public.books USING btree (id)"],
            vec!["books_title_idx", "f", "f", "3", "CREATE INDEX books_title_idx ON public.books USING btree (title)"],
        ]
    );
    // int2vector subscripts start at 0, and = ANY works over the vector.
    assert_eq!(
        rows(
            &mut c,
            "SELECT a.attname FROM pg_index x JOIN pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = ANY (x.indkey)
             WHERE x.indrelid = 'books'::regclass AND x.indisprimary"
        ),
        vec![vec!["id"]]
    );
    assert_eq!(text_value(&mut c, "SELECT indkey[0] FROM pg_index WHERE indexrelid = 'books_title_idx'::regclass").as_deref(), Some("3"));
}

#[test]
fn int2vector_text_casts_and_mixed_index_keys() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE mx (a int, b text, c int); CREATE INDEX mx_i ON mx (a, lower(b)); CREATE INDEX mx_j ON mx (lower(b), c)")
        .expect("setup");
    let got = rows(
        &mut c,
        "SELECT indexrelid::regclass::text, indkey::text, indkey::int2[]::text, indoption::text,
                pg_get_expr(indexprs, indrelid), pg_get_indexdef(indexrelid)
         FROM pg_index WHERE indrelid = 'mx'::regclass ORDER BY 1",
    );
    assert_eq!(
        got,
        vec![
            vec!["mx_i", "1 0", "[0:1]={1,0}", "0 0", "lower(b)", "CREATE INDEX mx_i ON public.mx USING btree (a, lower(b))"],
            vec!["mx_j", "0 3", "[0:1]={0,3}", "0 0", "lower(b)", "CREATE INDEX mx_j ON public.mx USING btree (lower(b), c)"],
        ]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT '3 1 4'::int2vector::text, ('3 1 4'::int2vector)::int2[]::text, '1 2'::oidvector::text, ''::int2vector::int2[]::text"
        ),
        vec![vec!["3 1 4", "[0:2]={3,1,4}", "1 2", "{}"]]
    );
}

#[test]
fn information_schema_columns_and_constraints() {
    let (_db, mut c) = setup();
    schema(&mut c);
    let got = rows(
        &mut c,
        "SELECT column_name, data_type, is_nullable, character_maximum_length, numeric_precision, numeric_scale, is_identity
         FROM information_schema.columns WHERE table_name = 'authors' ORDER BY ordinal_position",
    );
    assert_eq!(
        got,
        vec![
            vec!["id", "integer", "NO", "NULL", "32", "0", "NO"],
            vec!["name", "character varying", "NO", "40", "NULL", "NULL", "NO"],
            vec!["email", "text", "YES", "NULL", "NULL", "NULL", "NO"],
            vec!["rating", "numeric", "YES", "NULL", "3", "1", "NO"],
        ]
    );
    let got = rows(
        &mut c,
        "SELECT constraint_name, constraint_type FROM information_schema.table_constraints WHERE table_name = 'authors' ORDER BY 1",
    );
    assert_eq!(
        got,
        vec![
            vec!["authors_email_key", "UNIQUE"],
            vec!["authors_id_not_null", "CHECK"],
            vec!["authors_name_not_null", "CHECK"],
            vec!["authors_pkey", "PRIMARY KEY"],
        ]
    );
    let got = rows(
        &mut c,
        "SELECT kcu.column_name, ccu.table_name, ccu.column_name, rc.delete_rule
         FROM information_schema.referential_constraints rc
         JOIN information_schema.key_column_usage kcu ON kcu.constraint_name = rc.constraint_name
         JOIN information_schema.constraint_column_usage ccu ON ccu.constraint_name = rc.constraint_name",
    );
    assert_eq!(got, vec![vec!["author_id", "authors", "id", "CASCADE"]]);
    assert_eq!(
        rows(&mut c, "SELECT table_name, table_type FROM information_schema.tables WHERE table_schema = 'public' ORDER BY 1"),
        vec![vec!["author_names", "VIEW"], vec!["authors", "BASE TABLE"], vec!["books", "BASE TABLE"]]
    );
}

#[test]
fn comments_round_trip() {
    let (_db, mut c) = setup();
    schema(&mut c);
    c.batch_execute("COMMENT ON TABLE authors IS 'people who write'; COMMENT ON COLUMN authors.email IS 'contact'").expect("comment");
    assert_eq!(text_value(&mut c, "SELECT obj_description('authors'::regclass, 'pg_class')").as_deref(), Some("people who write"));
    assert_eq!(text_value(&mut c, "SELECT col_description('authors'::regclass, 3)").as_deref(), Some("contact"));
    c.batch_execute("COMMENT ON TABLE authors IS NULL").expect("uncomment");
    assert_eq!(text_value(&mut c, "SELECT obj_description('authors'::regclass, 'pg_class')"), None);
    expect_code(c.simple_query("COMMENT ON COLUMN authors.nope IS 'x'"), "42703");
    expect_code(c.simple_query("COMMENT ON TABLE nope IS 'x'"), "42P01");
    expect_code(c.simple_query("COMMENT ON TABLE author_names IS 'x'"), "42809");
}

#[test]
fn serial_sequences_and_regclass_of_catalogs() {
    let (_db, mut c) = setup();
    schema(&mut c);
    assert_eq!(text_value(&mut c, "SELECT pg_get_serial_sequence('authors', 'id')").as_deref(), Some("public.authors_id_seq"));
    assert_eq!(text_value(&mut c, "SELECT pg_get_serial_sequence('books', 'id')").as_deref(), Some("public.books_id_seq"));
    assert_eq!(text_value(&mut c, "SELECT pg_get_serial_sequence('authors', 'name')"), None);
    assert_eq!(text_value(&mut c, "SELECT 'pg_class'::regclass::text").as_deref(), Some("pg_class"));
    assert_eq!(text_value(&mut c, "SELECT to_regclass('nope')"), None);
    assert_eq!(
        rows(&mut c, "SELECT sequencename, data_type FROM pg_sequences ORDER BY 1"),
        vec![vec!["authors_id_seq", "integer"], vec!["books_id_seq", "bigint"]]
    );
}

#[test]
fn pg_type_and_namespaces() {
    let (_db, mut c) = setup();
    assert_eq!(
        rows(
            &mut c,
            "SELECT typname, typlen, typcategory, typelem::regtype FROM pg_type WHERE typname IN ('int4', '_int4', 'varchar') ORDER BY 1"
        ),
        vec![vec!["_int4", "-1", "A", "integer"], vec!["int4", "4", "N", "-"], vec!["varchar", "-1", "S", "-"]]
    );
    assert_eq!(
        rows(&mut c, "SELECT nspname FROM pg_namespace WHERE nspname IN ('public', 'pg_catalog', 'information_schema') ORDER BY 1"),
        vec![vec!["information_schema"], vec!["pg_catalog"], vec!["public"]]
    );
}

#[test]
fn covering_index_include_columns() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE cov (id int, e text, a int, b text);
         CREATE UNIQUE INDEX cov_expr ON cov (lower(e)) INCLUDE (id) WHERE id > 0;
         CREATE INDEX cov_two ON cov (a) INCLUDE (id, b);
         CREATE INDEX cov_plain ON cov (a, b);",
    )
    .expect("schema");
    let got = rows(
        &mut c,
        "SELECT i.relname, x.indnatts::text, x.indnkeyatts::text, array_to_string(x.indkey::int2[], ' '), pg_get_indexdef(x.indexrelid), i.relnatts::text
         FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = 'cov'::regclass ORDER BY i.relname",
    );
    assert_eq!(
        got,
        vec![
            vec![
                "cov_expr",
                "2",
                "1",
                "0 1",
                "CREATE UNIQUE INDEX cov_expr ON public.cov USING btree (lower(e)) INCLUDE (id) WHERE (id > 0)",
                "2"
            ],
            vec!["cov_plain", "2", "2", "3 4", "CREATE INDEX cov_plain ON public.cov USING btree (a, b)", "2"],
            vec!["cov_two", "3", "1", "3 1 4", "CREATE INDEX cov_two ON public.cov USING btree (a) INCLUDE (id, b)", "3"],
        ]
    );
    assert_eq!(
        rows(&mut c, "SELECT indexdef FROM pg_indexes WHERE indexname = 'cov_two'"),
        vec![vec!["CREATE INDEX cov_two ON public.cov USING btree (a) INCLUDE (id, b)"]]
    );
    // Dropping an INCLUDE column drops the index.
    c.batch_execute("ALTER TABLE cov DROP COLUMN b").expect("drop");
    assert_eq!(rows(&mut c, "SELECT count(*)::text FROM pg_indexes WHERE tablename = 'cov'"), vec![vec!["1"]]);
}

#[test]
fn pg_get_keywords() {
    let (_db, mut c) = setup();
    assert_eq!(rows(&mut c, "SELECT count(*) FROM pg_get_keywords()"), [["494"]]);
    assert_eq!(
        rows(&mut c, "SELECT * FROM pg_get_keywords() WHERE word IN ('abort', 'all', 'between', 'bit', 'authorization') ORDER BY word"),
        [
            ["abort", "U", "t", "unreserved", "can be bare label"],
            ["all", "R", "t", "reserved", "can be bare label"],
            ["authorization", "T", "t", "reserved (can be function or type name)", "can be bare label"],
            ["between", "C", "t", "unreserved (cannot be function or type name)", "can be bare label"],
            ["bit", "C", "t", "unreserved (cannot be function or type name)", "can be bare label"],
        ]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT pg_typeof(word)::text, pg_typeof(catcode)::text, pg_typeof(barelabel)::text, pg_typeof(catdesc)::text, pg_typeof(baredesc)::text FROM pg_get_keywords() LIMIT 1"
        ),
        [["text", "\"char\"", "boolean", "text", "text"]]
    );
    assert_eq!(rows(&mut c, "SELECT word FROM pg_get_keywords() LIMIT 2"), [["abort"], ["absent"]]);
    // Neighbouring behaviour: quote_ident quotes exactly the non-unreserved keywords.
    assert_eq!(
        rows(&mut c, "SELECT count(*) FROM pg_get_keywords() WHERE quote_ident(word) <> word"),
        rows(&mut c, "SELECT count(*) FROM pg_get_keywords() WHERE catcode <> 'U'")
    );
}
