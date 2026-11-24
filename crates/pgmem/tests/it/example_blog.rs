//! Port of examples/blog/blog_test.go: the sqlc-generated blog queries against schema.sql.

use crate::support::{TestDb, setup, utc_ts};
use jiff::{SignedDuration, Timestamp};
use postgres::Client;

const CREATE_USER: &str = "-- name: CreateUser :one
INSERT INTO users (email, name, created_at)
VALUES ($1, $2, $3)
RETURNING id, email, name, created_at
";
const GET_USER: &str = "-- name: GetUser :one
SELECT id, email, name, created_at FROM users WHERE id = $1
";
const LIST_USERS: &str = "-- name: ListUsers :many
SELECT id, email, name, created_at FROM users ORDER BY id
";
const CREATE_POST: &str = "-- name: CreatePost :one
INSERT INTO posts (author_id, title, body, published, created_at)
VALUES ($1, $2, $3, $4, $5)
RETURNING id, author_id, title, body, published, created_at
";
const GET_POST: &str = "-- name: GetPost :one
SELECT id, author_id, title, body, published, created_at FROM posts WHERE id = $1
";
const PUBLISH_POST: &str = "-- name: PublishPost :one
UPDATE posts SET published = true WHERE id = $1 RETURNING id, author_id, title, body, published, created_at
";
const LIST_PUBLISHED_POSTS: &str = "-- name: ListPublishedPosts :many
SELECT p.id, p.title, p.body, p.created_at, u.name AS author_name
FROM posts p
JOIN users u ON u.id = p.author_id
WHERE p.published = true
ORDER BY p.created_at DESC
LIMIT $1 OFFSET $2
";
const CREATE_COMMENT: &str = "-- name: CreateComment :one
INSERT INTO comments (post_id, author_id, body, created_at)
VALUES ($1, $2, $3, $4)
RETURNING id, post_id, author_id, body, created_at
";
const LIST_COMMENTS_FOR_POST: &str = "-- name: ListCommentsForPost :many
SELECT c.id, c.post_id, c.author_id, c.body, c.created_at, u.name AS author_name
FROM comments c
JOIN users u ON u.id = c.author_id
WHERE c.post_id = $1
ORDER BY c.created_at
";
const COUNT_COMMENTS_PER_POST: &str = "-- name: CountCommentsPerPost :many
SELECT post_id, count(*) AS comment_count
FROM comments
GROUP BY post_id
ORDER BY post_id
";
const POST_WITH_COMMENT_COUNT: &str = "-- name: PostWithCommentCount :one
SELECT p.id, p.title,
       (SELECT count(*) FROM comments c WHERE c.post_id = p.id) AS comment_count
FROM posts p
WHERE p.id = $1
";
const DELETE_POST: &str = "-- name: DeletePost :exec
DELETE FROM posts WHERE id = $1
";

fn setup_schema() -> (TestDb, Client) {
    let (db, mut c) = setup();
    c.batch_execute(include_str!("../../../../examples/blog/schema.sql")).expect("schema");
    (db, c)
}

fn now() -> Timestamp {
    utc_ts(2024, 7, 15, 12, 0, 0)
}

/// CreateUser; returns the new id.
fn create_user(c: &mut Client, email: &str, name: &str) -> i64 {
    let row = c.query_one(CREATE_USER, &[&email, &name, &now()]).expect("CreateUser");
    assert_eq!(row.get::<_, String>(1), email);
    row.get(0)
}

/// CreatePost; returns (id, published).
fn create_post(c: &mut Client, author: i64, title: &str, body: &str, published: bool, at: Timestamp) -> (i64, bool) {
    let row = c.query_one(CREATE_POST, &[&author, &title, &body, &published, &at]).expect("CreatePost");
    (row.get(0), row.get(4))
}

fn create_comment(c: &mut Client, post: i64, author: i64, body: &str, at: Timestamp) {
    c.query_one(CREATE_COMMENT, &[&post, &author, &body, &at]).expect("CreateComment");
}

/// ListPublishedPosts; returns the author names. LIMIT/OFFSET parameters are bigint in Postgres.
fn list_published(c: &mut Client, limit: i64, offset: i64) -> Vec<String> {
    c.query(LIST_PUBLISHED_POSTS, &[&limit, &offset]).expect("ListPublishedPosts").iter().map(|r| r.get(4)).collect()
}

#[test]
fn user_crud() {
    let (_db, mut c) = setup_schema();
    let id = create_user(&mut c, "ada@example.com", "Ada");
    let got = c.query_one(GET_USER, &[&id]).expect("GetUser");
    assert_eq!(got.get::<_, i64>(0), id);
    assert_eq!(got.get::<_, String>(2), "Ada");
    let all = c.query(LIST_USERS, &[]).expect("ListUsers");
    assert_eq!(all.len(), 1);
}

#[test]
fn posts_and_comments() {
    let (_db, mut c) = setup_schema();
    let ada = create_user(&mut c, "ada@example.com", "Ada");
    let bob = create_user(&mut c, "bob@example.com", "Bob");
    let (post, published) = create_post(&mut c, ada, "First", "hello world", false, now());
    assert!(!published, "post should be draft initially");

    assert!(list_published(&mut c, 10, 0).is_empty(), "expected no published posts");

    let row = c.query_one(PUBLISH_POST, &[&post]).expect("PublishPost");
    assert!(row.get::<_, bool>(4), "PublishPost RETURNING published");
    let row = c.query_one(GET_POST, &[&post]).expect("GetPost");
    assert!(row.get::<_, bool>(4), "GetPost after publish");

    assert_eq!(list_published(&mut c, 10, 0), vec!["Ada"]);

    create_comment(&mut c, post, bob, "nice", now());
    create_comment(&mut c, post, ada, "thanks", now() + SignedDuration::from_mins(1));

    let comments: Vec<String> = c.query(LIST_COMMENTS_FOR_POST, &[&post]).expect("ListCommentsForPost").iter().map(|r| r.get(5)).collect();
    assert_eq!(comments, vec!["Bob", "Ada"]);

    let counts: Vec<(i64, i64)> =
        c.query(COUNT_COMMENTS_PER_POST, &[]).expect("CountCommentsPerPost").iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(counts, vec![(post, 2)]);

    let with_count = c.query_one(POST_WITH_COMMENT_COUNT, &[&post]).expect("PostWithCommentCount");
    assert_eq!(with_count.get::<_, i64>(2), 2);
}

#[test]
fn pagination() {
    let (_db, mut c) = setup_schema();
    let user = create_user(&mut c, "ada@example.com", "Ada");
    for i in 0..5 {
        create_post(&mut c, user, "Post", "...", true, now() + SignedDuration::from_hours(i));
    }
    assert_eq!(list_published(&mut c, 2, 0).len(), 2);
    assert_eq!(list_published(&mut c, 2, 2).len(), 2);
    assert_eq!(list_published(&mut c, 2, 4).len(), 1);
}

#[test]
fn delete_fk_reject() {
    let (_db, mut c) = setup_schema();
    let user = create_user(&mut c, "ada@example.com", "Ada");
    let (post, _) = create_post(&mut c, user, "x", "y", true, now());
    create_comment(&mut c, post, user, "comment", now());
    // Deleting a post that still has comments violates the FK (foreign_key_violation).
    crate::support::expect_code(c.execute(DELETE_POST, &[&post]), "23503");
}
