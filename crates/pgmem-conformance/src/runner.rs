//! Runs corpus files against pgmem or a real PostgreSQL, one fresh database per file.

use crate::corpus::{CorpusFile, Expect, Record};
use postgres::{Client, NoTls, SimpleQueryMessage};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Where the corpus runs.
#[derive(Clone)]
pub enum Target {
    Pgmem,
    /// Admin connection string of a real server; each file gets its own database.
    Postgres(String),
}

/// A fresh database on the target, dropped afterwards.
struct Database {
    _server: Option<pgmem::Server>,
    real: Option<(String, String)>,
    conninfo: String,
}

impl Database {
    fn create(target: &Target) -> Result<Database, String> {
        match target {
            Target::Pgmem => {
                let server = pgmem::Server::start().map_err(|e| format!("start pgmem: {e}"))?;
                let conninfo = server.conninfo();
                Ok(Database { _server: Some(server), real: None, conninfo })
            }
            Target::Postgres(admin) => {
                let name = format!("pgmem_conformance_{}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
                let mut c = Client::connect(admin, NoTls).map_err(|e| format!("connect {admin}: {e}"))?;
                // Postgres' builtin C.UTF-8 collation: text ordering independent of the OS locale.
                let create = format!(
                    "CREATE DATABASE {name} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LOCALE 'C'"
                );
                c.batch_execute(&create).map_err(|e| format!("create database: {e}"))?;
                Ok(Database { _server: None, real: Some((admin.clone(), name.clone())), conninfo: format!("{admin} dbname={name}") })
            }
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some((admin, name)) = &self.real
            && let Ok(mut c) = Client::connect(admin, NoTls)
        {
            let _ = c.batch_execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"));
        }
    }
}

/// The outcome of one record.
pub struct Outcome {
    pub actual: Expect,
    pub passed: bool,
}

fn error_of(e: &postgres::Error) -> Expect {
    match e.as_db_error() {
        Some(db) => Expect::Error { code: db.code().code().to_owned(), message: Some(db.message().to_owned()) },
        // Not a server error (the connection broke, a protocol error, ...): never matches.
        None => Expect::Error { code: "(client)".to_owned(), message: Some(e.to_string().replace('\n', " ")) },
    }
}

fn render_value(v: Option<&str>) -> String {
    match v {
        None => "NULL".to_owned(),
        Some("") => "(empty)".to_owned(),
        Some(s) => s.replace('\n', "\\n").replace('\r', "\\r"),
    }
}

fn observe(client: &mut Client, record: &Record) -> Expect {
    let Expect::Rows { sort, .. } = record.expect else {
        return match client.simple_query(&record.sql) {
            Ok(_) => Expect::Ok,
            Err(e) => error_of(&e),
        };
    };
    // Describe gives the result types a driver sees; the simple query gives the text values.
    let columns = match client.prepare(&record.sql) {
        Ok(stmt) => stmt.columns().iter().map(|c| format!("{}:{}", c.name(), c.type_().name())).collect::<Vec<_>>().join(" | "),
        Err(e) => return error_of(&e),
    };
    match client.simple_query(&record.sql) {
        Ok(messages) => {
            let rows = messages
                .iter()
                .filter_map(|m| match m {
                    SimpleQueryMessage::Row(row) => Some((0..row.len()).map(|i| render_value(row.get(i))).collect::<Vec<_>>().join(" | ")),
                    _ => None,
                })
                .collect();
            Expect::Rows { sort, columns, rows }
        }
        Err(e) => error_of(&e),
    }
}

/// Runs every record of `file` in order and returns one outcome per record.
pub fn run_file(target: &Target, file: &CorpusFile) -> Result<Vec<Outcome>, String> {
    let db = Database::create(target)?;
    let mut sessions: HashMap<String, Client> = HashMap::new();
    let mut outcomes = Vec::new();
    for record in file.records() {
        let name = record.connection.clone().unwrap_or_else(|| "main".to_owned());
        // A session the server closed (a crash, a FATAL error) is replaced by a fresh one.
        if sessions.get(&name).is_some_and(Client::is_closed) {
            sessions.remove(&name);
        }
        let client = match sessions.entry(name) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(Client::connect(&db.conninfo, NoTls).map_err(|e| format!("{}: connect: {e}", file.name))?)
            }
        };
        let actual = observe(client, record);
        let passed = record.expect.matches(&actual);
        outcomes.push(Outcome { actual, passed });
    }
    drop(sessions);
    Ok(outcomes)
}
