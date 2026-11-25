//! tokio-postgres (async): typed parameters and binary results, pipelining, portals,
//! transactions and savepoints, COPY, LISTEN / NOTIFY and error fields.

use futures_util::{SinkExt, StreamExt, TryStreamExt};
use pgmem_clients::{connect, test_db};
use tokio_postgres::AsyncMessage;
use tokio_postgres::types::Type;

#[tokio::test]
async fn typed_parameters_and_binary_results() {
    let db = test_db().await;
    let c = connect(&db).await;
    c.batch_execute("CREATE TABLE things (id int8 PRIMARY KEY, small int2, n int4, f float8, r float4, name text, data bytea, ok bool, tags text[], nums int4[])")
        .await
        .expect("create");
    let stmt = c.prepare("INSERT INTO things VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)").await.expect("prepare");
    assert_eq!(
        stmt.params(),
        [
            Type::INT8,
            Type::INT2,
            Type::INT4,
            Type::FLOAT8,
            Type::FLOAT4,
            Type::TEXT,
            Type::BYTEA,
            Type::BOOL,
            Type::TEXT_ARRAY,
            Type::INT4_ARRAY
        ]
    );
    let tags = vec!["a".to_owned(), "b c".to_owned()];
    let nums = vec![1i32, -2, 3];
    c.execute(&stmt, &[&1i64, &7i16, &-42i32, &1.5f64, &2.25f32, &"héllo", &vec![0u8, 255], &true, &tags, &nums]).await.expect("insert");
    c.execute(
        &stmt,
        &[
            &2i64,
            &None::<i16>,
            &None::<i32>,
            &None::<f64>,
            &None::<f32>,
            &None::<&str>,
            &None::<Vec<u8>>,
            &None::<bool>,
            &None::<Vec<String>>,
            &None::<Vec<i32>>,
        ],
    )
    .await
    .expect("insert nulls");
    let row = c.query_one("SELECT * FROM things WHERE id = $1", &[&1i64]).await.expect("select");
    assert_eq!(row.get::<_, i16>("small"), 7);
    assert_eq!(row.get::<_, i32>("n"), -42);
    assert_eq!(row.get::<_, f64>("f"), 1.5);
    assert_eq!(row.get::<_, f32>("r"), 2.25);
    assert_eq!(row.get::<_, String>("name"), "héllo");
    assert_eq!(row.get::<_, Vec<u8>>("data"), [0, 255]);
    assert!(row.get::<_, bool>("ok"));
    assert_eq!(row.get::<_, Vec<String>>("tags"), tags);
    assert_eq!(row.get::<_, Vec<i32>>("nums"), nums);
    let row = c.query_one("SELECT * FROM things WHERE id = 2", &[]).await.expect("select nulls");
    assert_eq!(row.get::<_, Option<String>>("name"), None);
    assert_eq!(row.get::<_, Option<Vec<i32>>>("nums"), None);
    // Parameter types inferred from context, and an explicit cast.
    let row = c.query_one("SELECT $1::text || '!', $2 + 1", &[&"x", &41i32]).await.expect("inferred");
    assert_eq!(row.get::<_, String>(0), "x!");
    assert_eq!(row.get::<_, i32>(1), 42);
}

#[tokio::test]
async fn pipelining_and_portals() {
    let db = test_db().await;
    let c = connect(&db).await;
    c.batch_execute("CREATE TABLE seq (n int); INSERT INTO seq SELECT generate_series(1, 10)").await.expect("setup");
    // Several queries in flight on one connection.
    let (a, b, d) = tokio::join!(
        c.query_one("SELECT count(*) FROM seq", &[]),
        c.query_one("SELECT sum(n) FROM seq", &[]),
        c.query_one("SELECT max(n) FROM seq WHERE n < $1", &[&5i32]),
    );
    assert_eq!(a.expect("count").get::<_, i64>(0), 10);
    assert_eq!(b.expect("sum").get::<_, i64>(0), 55);
    assert_eq!(d.expect("max").get::<_, i32>(0), 4);
    // A portal fetched a few rows at a time (Execute with a row limit) inside a transaction.
    let mut c = c;
    let tx = c.transaction().await.expect("begin");
    let stmt = tx.prepare("SELECT n FROM seq ORDER BY n").await.expect("prepare");
    let portal = tx.bind(&stmt, &[]).await.expect("bind");
    let mut seen = Vec::new();
    loop {
        let rows = tx.query_portal(&portal, 4).await.expect("fetch");
        if rows.is_empty() {
            break;
        }
        seen.extend(rows.iter().map(|r| r.get::<_, i32>(0)));
    }
    assert_eq!(seen, (1..=10).collect::<Vec<_>>());
    tx.commit().await.expect("commit");
    // A row stream.
    let params: Vec<i32> = Vec::new();
    let rows: Vec<i32> = c
        .query_raw("SELECT n FROM seq WHERE n % 2 = 0 ORDER BY n", params.iter())
        .await
        .expect("query_raw")
        .map_ok(|r| r.get::<_, i32>(0))
        .try_collect()
        .await
        .expect("stream");
    assert_eq!(rows, [2, 4, 6, 8, 10]);
    // simple_query: several statements, text results.
    let msgs = c.simple_query("SELECT 1 AS one; SELECT 'two' AS two").await.expect("simple query");
    let values: Vec<String> = msgs
        .iter()
        .filter_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => r.get(0).map(str::to_owned),
            _ => None,
        })
        .collect();
    assert_eq!(values, ["1", "two"]);
}

#[tokio::test]
async fn transactions_and_savepoints() {
    let db = test_db().await;
    let mut c = connect(&db).await;
    c.batch_execute("CREATE TABLE acct (id int PRIMARY KEY, balance int CHECK (balance >= 0))").await.expect("create");
    let mut tx = c.transaction().await.expect("begin");
    tx.execute("INSERT INTO acct VALUES (1, 100)", &[]).await.expect("insert");
    {
        let sp = tx.savepoint("sp").await.expect("savepoint");
        sp.execute("UPDATE acct SET balance = 50 WHERE id = 1", &[]).await.expect("update");
        sp.rollback().await.expect("rollback to savepoint");
    }
    let err = tx.execute("UPDATE acct SET balance = -1 WHERE id = 1", &[]).await.expect_err("check violation");
    assert_eq!(err.code().map(|c| c.code()), Some("23514"));
    tx.rollback().await.expect("rollback");
    assert_eq!(c.query_one("SELECT count(*) FROM acct", &[]).await.expect("count").get::<_, i64>(0), 0);
    let tx = c.transaction().await.expect("begin");
    tx.execute("INSERT INTO acct VALUES (2, 10)", &[]).await.expect("insert");
    tx.commit().await.expect("commit");
    assert_eq!(c.query_one("SELECT balance FROM acct WHERE id = 2", &[]).await.expect("read").get::<_, i32>(0), 10);
}

#[tokio::test]
async fn copy_in_and_out() {
    let db = test_db().await;
    let c = connect(&db).await;
    c.batch_execute("CREATE TABLE items (id int, name text)").await.expect("create");
    let sink = c.copy_in("COPY items FROM STDIN (FORMAT csv)").await.expect("copy in");
    futures_util::pin_mut!(sink);
    sink.send(bytes_of("1,apple\n2,\"pear, ripe\"\n")).await.expect("send");
    sink.send(bytes_of("3,\n")).await.expect("send");
    assert_eq!(sink.finish().await.expect("finish"), 3);
    let stream = c.copy_out("COPY (SELECT * FROM items ORDER BY id) TO STDOUT").await.expect("copy out");
    let chunks: Vec<Vec<u8>> = stream.map_ok(|b| b.to_vec()).try_collect().await.expect("read");
    assert_eq!(String::from_utf8(chunks.concat()).expect("utf8"), "1\tapple\n2\tpear, ripe\n3\t\\N\n");
}

fn bytes_of(s: &str) -> std::io::Cursor<Vec<u8>> {
    std::io::Cursor::new(s.as_bytes().to_vec())
}

#[tokio::test]
async fn listen_notify() {
    let db = test_db().await;
    let (listener, mut conn) = tokio_postgres::connect(&db.url, tokio_postgres::NoTls).await.expect("connect");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stream = futures_util::stream::poll_fn(move |cx| conn.poll_message(cx));
    tokio::spawn(stream.for_each(move |m| {
        if let Ok(AsyncMessage::Notification(n)) = m {
            let _ = tx.send((n.channel().to_owned(), n.payload().to_owned()));
        }
        futures_util::future::ready(())
    }));
    listener.batch_execute("LISTEN jobs").await.expect("listen");
    let sender = connect(&db).await;
    sender.batch_execute("NOTIFY jobs, 'first'; SELECT pg_notify('jobs', 'second')").await.expect("notify");
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("notification in time");
    assert_eq!(first, Some(("jobs".to_owned(), "first".to_owned())));
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await.expect("notification in time");
    assert_eq!(second, Some(("jobs".to_owned(), "second".to_owned())));
}

#[tokio::test]
async fn error_fields() {
    let db = test_db().await;
    let c = connect(&db).await;
    c.batch_execute("CREATE TABLE users (id int PRIMARY KEY, email text NOT NULL UNIQUE)").await.expect("create");
    c.execute("INSERT INTO users VALUES (1, 'a@x')", &[]).await.expect("insert");
    let err = c.execute("INSERT INTO users VALUES (2, 'a@x')", &[]).await.expect_err("duplicate");
    let db_err = err.as_db_error().expect("database error");
    assert_eq!(db_err.code().code(), "23505");
    assert_eq!(db_err.message(), "duplicate key value violates unique constraint \"users_email_key\"");
    assert_eq!(db_err.detail(), Some("Key (email)=(a@x) already exists."));
    assert_eq!(db_err.table(), Some("users"));
    assert_eq!(db_err.constraint(), Some("users_email_key"));
    let err = c.execute("INSERT INTO users (id) VALUES (3)", &[]).await.expect_err("not null");
    assert_eq!(err.as_db_error().map(|e| (e.code().code(), e.column())), Some(("23502", Some("email"))));
    let err = c.prepare("SELECT nope FROM users").await.expect_err("undefined column");
    assert_eq!(err.code().map(|c| c.code()), Some("42703"));
    // The connection stays usable after errors.
    assert_eq!(c.query_one("SELECT count(*) FROM users", &[]).await.expect("count").get::<_, i64>(0), 1);
}
