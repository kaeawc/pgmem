use crate::support::{expect_code, setup};

#[test]
fn table_fk_basic() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE users (id bigint PRIMARY KEY, name text NOT NULL)").expect("CREATE users");
    c.batch_execute(
        "CREATE TABLE posts (
           id        bigint PRIMARY KEY,
           author_id bigint NOT NULL,
           title     text   NOT NULL,
           FOREIGN KEY (author_id) REFERENCES users(id)
         )",
    )
    .expect("CREATE posts");
    c.execute("INSERT INTO users (id, name) VALUES (1, 'alice')", &[]).expect("INSERT users");
    c.execute("INSERT INTO posts (id, author_id, title) VALUES (10, 1, 'hi')", &[]).expect("INSERT post");
    expect_code(c.execute("INSERT INTO posts (id, author_id, title) VALUES (11, 99, 'bad')", &[]), "23503");
}

#[test]
fn table_fk_named() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE users (id bigint PRIMARY KEY)").expect("CREATE users");
    c.batch_execute(
        "CREATE TABLE posts (
           id        bigint PRIMARY KEY,
           author_id bigint NOT NULL,
           CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users(id)
         )",
    )
    .expect("CREATE posts");
    let e = expect_code(c.execute("INSERT INTO posts (id, author_id) VALUES (1, 99)", &[]), "23503");
    assert_eq!(e.as_db_error().and_then(|d| d.constraint()), Some("posts_author_fk"));
}

#[test]
fn table_fk_on_delete_cascade() {
    let (_db, mut c) = setup();
    c.batch_execute(
        "CREATE TABLE users (id bigint PRIMARY KEY);
         CREATE TABLE posts (
           id        bigint PRIMARY KEY,
           author_id bigint NOT NULL,
           CONSTRAINT posts_author_fk FOREIGN KEY (author_id) REFERENCES users(id) ON DELETE CASCADE
         );
         INSERT INTO users (id) VALUES (1), (2);
         INSERT INTO posts (id, author_id) VALUES (10, 1), (11, 2);",
    )
    .expect("setup");
    c.execute("DELETE FROM users WHERE id = 1", &[]).expect("DELETE");
    let n: i64 = c.query_one("SELECT count(*) FROM posts", &[]).expect("count").get(0);
    assert_eq!(n, 1);
}

#[test]
fn table_fk_unknown_column() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE users (id bigint PRIMARY KEY)").expect("CREATE users");
    expect_code(
        c.batch_execute(
            "CREATE TABLE posts (
               id bigint PRIMARY KEY,
               FOREIGN KEY (author_id) REFERENCES users(id)
             )",
        ),
        "42703",
    );
}

#[test]
fn table_fk_multi_column_rejected() {
    let (_db, mut c) = setup();
    c.batch_execute("CREATE TABLE users (id bigint PRIMARY KEY)").expect("CREATE users");
    // Postgres rejects this one because the referenced-columns list repeats `id`.
    expect_code(
        c.batch_execute(
            "CREATE TABLE posts (
               a bigint NOT NULL,
               b bigint NOT NULL,
               FOREIGN KEY (a, b) REFERENCES users(id, id)
             )",
        ),
        "42830",
    );
}
