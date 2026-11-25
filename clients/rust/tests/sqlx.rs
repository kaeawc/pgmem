//! sqlx (Postgres driver): pools and its statement cache, binds and `FromRow`, user enum types
//! (resolved through pg_type), json, arrays, transactions, PgListener, COPY and errors.

use futures_util::TryStreamExt;
use pgmem_clients::test_db;
use sqlx::postgres::{PgListener, PgPoolOptions};
use sqlx::{Connection, Row};

#[derive(Debug, PartialEq, sqlx::Type)]
#[sqlx(type_name = "mood", rename_all = "lowercase")]
enum Mood {
    Happy,
    Sad,
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Person {
    id: i64,
    name: String,
    mood: Mood,
    tags: Vec<String>,
    score: Option<f64>,
}

async fn pool(url: &str) -> sqlx::PgPool {
    PgPoolOptions::new().max_connections(3).connect(url).await.expect("pool")
}

#[tokio::test]
async fn queries_types_and_statement_cache() {
    let db = test_db().await;
    let pool = pool(&db.url).await;
    sqlx::raw_sql(
        "CREATE TYPE mood AS ENUM ('happy', 'sad');
         CREATE TABLE people (id bigserial PRIMARY KEY, name text NOT NULL, mood mood NOT NULL, tags text[] NOT NULL DEFAULT '{}', score float8, doc jsonb)",
    )
    .execute(&pool)
    .await
    .expect("schema");
    for (name, mood, tags) in [("ann", Mood::Happy, vec!["a"]), ("bob", Mood::Sad, vec!["b", "c"])] {
        let id: i64 = sqlx::query_scalar("INSERT INTO people (name, mood, tags) VALUES ($1, $2, $3) RETURNING id")
            .bind(name)
            .bind(mood)
            .bind(tags)
            .fetch_one(&pool)
            .await
            .expect("insert");
        assert!(id > 0);
    }
    // The same statement many times: sqlx reuses its named prepared statement.
    for _ in 0..5 {
        let people: Vec<Person> =
            sqlx::query_as("SELECT id, name, mood, tags, score FROM people ORDER BY id").fetch_all(&pool).await.expect("select");
        assert_eq!(people.len(), 2);
        assert_eq!(
            people[1],
            Person { id: people[1].id, name: "bob".into(), mood: Mood::Sad, tags: vec!["b".into(), "c".into()], score: None }
        );
    }
    let missing: Option<Person> = sqlx::query_as("SELECT id, name, mood, tags, score FROM people WHERE name = $1")
        .bind("zed")
        .fetch_optional(&pool)
        .await
        .expect("optional");
    assert_eq!(missing, None);
    sqlx::query("UPDATE people SET doc = $1 WHERE name = 'ann'")
        .bind(sqlx::types::Json(sample_doc()))
        .execute(&pool)
        .await
        .expect("json bind");
    let row = sqlx::query("SELECT doc, doc->>'k' AS k, mood::text AS m FROM people WHERE name = 'ann'")
        .fetch_one(&pool)
        .await
        .expect("json read");
    let doc: sqlx::types::JsonValue = row.get("doc");
    assert_eq!(doc.to_string(), "{\"k\":\"v\",\"n\":[1,2]}");
    assert_eq!(row.get::<String, _>("k"), "v");
    assert_eq!(row.get::<String, _>("m"), "happy");
    let streamed: Vec<String> =
        sqlx::query_scalar("SELECT name FROM people ORDER BY name DESC").fetch(&pool).try_collect().await.expect("stream");
    assert_eq!(streamed, ["bob", "ann"]);
}

fn sample_doc() -> sqlx::types::JsonValue {
    let mut doc = sqlx::types::JsonValue::Object(Default::default());
    doc["k"] = "v".into();
    doc["n"] = vec![1, 2].into();
    doc
}

#[tokio::test]
async fn transactions_and_errors() {
    let db = test_db().await;
    let mut conn = sqlx::PgConnection::connect(&db.url).await.expect("connect");
    sqlx::raw_sql("CREATE TABLE ledger (id int PRIMARY KEY, amount int NOT NULL CHECK (amount <> 0))")
        .execute(&mut conn)
        .await
        .expect("schema");
    let mut tx = conn.begin().await.expect("begin");
    sqlx::query("INSERT INTO ledger VALUES (1, 10)").execute(&mut *tx).await.expect("insert");
    tx.rollback().await.expect("rollback");
    let mut tx = conn.begin().await.expect("begin");
    sqlx::query("INSERT INTO ledger VALUES (2, 20)").execute(&mut *tx).await.expect("insert");
    tx.commit().await.expect("commit");
    let ids: Vec<i32> = sqlx::query_scalar("SELECT id FROM ledger").fetch_all(&mut conn).await.expect("ids");
    assert_eq!(ids, [2]);
    let err = sqlx::query("INSERT INTO ledger VALUES ($1, $2)").bind(2).bind(5).execute(&mut conn).await.expect_err("duplicate");
    let db_err = err.as_database_error().expect("database error");
    assert_eq!(db_err.code().as_deref(), Some("23505"));
    assert_eq!(db_err.constraint(), Some("ledger_pkey"));
    assert!(db_err.is_unique_violation());
    let err = sqlx::query("INSERT INTO ledger VALUES (3, 0)").execute(&mut conn).await.expect_err("check");
    assert!(err.as_database_error().is_some_and(|e| e.is_check_violation()));
    // Errors inside a transaction abort it until rollback.
    let mut tx = conn.begin().await.expect("begin");
    sqlx::query("SELECT 1/0").execute(&mut *tx).await.expect_err("division by zero");
    let err = sqlx::query("SELECT 1").execute(&mut *tx).await.expect_err("aborted");
    assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("25P02"));
    tx.rollback().await.expect("rollback");
    let one: i32 = sqlx::query_scalar("SELECT 1").fetch_one(&mut conn).await.expect("usable");
    assert_eq!(one, 1);
}

#[tokio::test]
async fn listener_and_copy() {
    let db = test_db().await;
    let pool = pool(&db.url).await;
    let mut listener = PgListener::connect(&db.url).await.expect("listener");
    listener.listen_all(["orders", "audit"]).await.expect("listen");
    sqlx::query("SELECT pg_notify('orders', 'o-1')").execute(&pool).await.expect("notify");
    sqlx::raw_sql("NOTIFY audit, 'a-1'").execute(&pool).await.expect("notify");
    let mut got = Vec::new();
    for _ in 0..2 {
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv()).await.expect("in time").expect("notification");
        got.push((n.channel().to_owned(), n.payload().to_owned()));
    }
    assert_eq!(got, [("orders".to_owned(), "o-1".to_owned()), ("audit".to_owned(), "a-1".to_owned())]);
    sqlx::raw_sql("CREATE TABLE points (x int, y int)").execute(&pool).await.expect("create");
    let mut conn = pool.acquire().await.expect("acquire");
    let mut copy = conn.copy_in_raw("COPY points FROM STDIN (FORMAT csv)").await.expect("copy in");
    copy.send(b"1,2\n3,4\n".as_slice()).await.expect("send");
    assert_eq!(copy.finish().await.expect("finish"), 2);
    let out: Vec<u8> = conn
        .copy_out_raw("COPY points TO STDOUT")
        .await
        .expect("copy out")
        .try_fold(Vec::new(), |mut acc, b| async move {
            acc.extend_from_slice(&b);
            Ok(acc)
        })
        .await
        .expect("read");
    assert_eq!(String::from_utf8(out).expect("utf8"), "1\t2\n3\t4\n");
}
