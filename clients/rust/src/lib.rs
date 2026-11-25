//! Client-compatibility suites for tokio-postgres and sqlx (`tests/`). Each test gets a fresh
//! database: a pgmem server of its own (the `pgmem` binary, `PGMEM_BIN` or
//! `target/release/pgmem`), or, with `PGMEM_CLIENT_DSN` set to a PostgreSQL URL
//! (`postgres://postgres@127.0.0.1:5432/postgres`), a new database on that server.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A database for one test; a pgmem server stops when this is dropped.
pub struct TestDb {
    pub url: String,
    child: Option<Child>,
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn binary() -> PathBuf {
    if let Ok(b) = std::env::var("PGMEM_BIN") {
        return PathBuf::from(b);
    }
    let mut dir = std::env::current_dir().expect("current dir");
    loop {
        let candidate = dir.join("target/release/pgmem");
        if candidate.exists() {
            return candidate;
        }
        if !dir.pop() {
            return PathBuf::from("pgmem");
        }
    }
}

/// True when the suite runs against real PostgreSQL.
pub fn is_postgres() -> bool {
    std::env::var("PGMEM_CLIENT_DSN").is_ok_and(|d| !d.is_empty())
}

pub async fn test_db() -> TestDb {
    match std::env::var("PGMEM_CLIENT_DSN") {
        Ok(admin) if !admin.is_empty() => {
            let name = format!("pgmem_client_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
            let (client, conn) = tokio_postgres::connect(&admin, tokio_postgres::NoTls).await.expect("connect to PGMEM_CLIENT_DSN");
            tokio::spawn(conn);
            client.batch_execute(&format!("CREATE DATABASE {name}")).await.expect("create database");
            let base = admin.rsplit_once('/').map_or(admin.as_str(), |(b, _)| b);
            TestDb { url: format!("{base}/{name}"), child: None }
        }
        _ => {
            let mut child = Command::new(binary())
                .args(["--port", "0"])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("start pgmem (set PGMEM_BIN or run `cargo build --release -p pgmem`)");
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("stdout")).read_line(&mut line).expect("read pgmem URL");
            TestDb { url: line.trim().to_owned(), child: Some(child) }
        }
    }
}

/// A tokio-postgres connection to `db`, its connection task running in the background.
pub async fn connect(db: &TestDb) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&db.url, tokio_postgres::NoTls).await.expect("connect");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    client
}
