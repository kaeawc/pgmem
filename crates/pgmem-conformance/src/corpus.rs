//! The corpus format, a small sqllogictest dialect:
//!
//! ```text
//! # comment
//! statement ok
//! CREATE TABLE t (a int PRIMARY KEY, b text)
//!
//! statement error 23505 duplicate key value violates unique constraint "t_pkey"
//! INSERT INTO t VALUES (1, 'x'), (1, 'y')
//!
//! query rowsort
//! SELECT a, b FROM t
//! ----
//! a:int4 | b:text
//! 1 | x
//! 2 | NULL
//! ```
//!
//! A record's SQL runs to the next blank line (or `----`). `statement` runs its SQL with the
//! simple query protocol; `query` also describes it, and its result block holds the column names
//! and types (`name:type`), then one line per row in text format (`NULL` for null, `(empty)` for
//! the empty string). `rowsort` compares rows ignoring order. An error's message is optional; when
//! present it must match exactly. A `connection NAME` line before a record runs it on another
//! session of the same database (opened on first use).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expect {
    Ok,
    Error { code: String, message: Option<String> },
    Rows { sort: bool, columns: String, rows: Vec<String> },
}

impl Expect {
    /// True if `actual` satisfies this expectation.
    pub fn matches(&self, actual: &Expect) -> bool {
        match (self, actual) {
            (Expect::Error { code, message }, Expect::Error { code: c, message: m }) => code == c && (message.is_none() || message == m),
            (Expect::Rows { sort, columns, rows }, Expect::Rows { columns: c, rows: r, .. }) => {
                if columns != c {
                    return false;
                }
                if *sort {
                    let (mut a, mut b) = (rows.clone(), r.clone());
                    a.sort();
                    b.sort();
                    a == b
                } else {
                    rows == r
                }
            }
            (a, b) => a == b,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Record {
    /// 1-based line of the record header.
    pub line: usize,
    pub connection: Option<String>,
    pub sql: String,
    pub expect: Expect,
    /// Stable identity (file, connection and SQL; not the line), used by the failure baseline.
    pub key: String,
}

impl Record {
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(c) = &self.connection {
            out.push_str(&format!("connection {c}\n"));
        }
        match &self.expect {
            Expect::Ok => out.push_str("statement ok\n"),
            Expect::Error { code, message } => {
                out.push_str("statement error ");
                out.push_str(code);
                if let Some(m) = message {
                    out.push(' ');
                    out.push_str(m);
                }
                out.push('\n');
            }
            Expect::Rows { sort, .. } => out.push_str(if *sort { "query rowsort\n" } else { "query\n" }),
        }
        out.push_str(&self.sql);
        out.push('\n');
        if let Expect::Rows { columns, rows, .. } = &self.expect {
            out.push_str("----\n");
            out.push_str(columns);
            out.push('\n');
            for r in rows {
                out.push_str(r);
                out.push('\n');
            }
        }
        out
    }
}

pub enum Chunk {
    /// Lines kept verbatim (comments, blank lines).
    Text(String),
    Record(Record),
}

pub struct CorpusFile {
    pub path: PathBuf,
    /// Path relative to the corpus root, with `/` separators.
    pub name: String,
    pub chunks: Vec<Chunk>,
}

impl CorpusFile {
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.chunks.iter().filter_map(|c| match c {
            Chunk::Record(r) => Some(r),
            Chunk::Text(_) => None,
        })
    }

    /// The area a file reports under: its directory below the corpus root.
    pub fn area(&self) -> &str {
        self.name.split_once('/').map_or("(root)", |(a, _)| a)
    }

    pub fn render(&self) -> String {
        self.chunks
            .iter()
            .map(|c| match c {
                Chunk::Text(t) => t.clone(),
                Chunk::Record(r) => r.render(),
            })
            .collect()
    }
}

/// 64-bit FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

fn parse_header(line: &str) -> Option<Expect> {
    let mut words = line.splitn(3, ' ');
    match (words.next()?, words.next(), words.next()) {
        ("statement", Some("ok"), None) => Some(Expect::Ok),
        ("statement", Some("error"), rest) => {
            let rest = rest.unwrap_or("");
            let (code, message) = match rest.split_once(' ') {
                Some((c, m)) => (c.to_owned(), Some(m.to_owned())),
                None => (rest.to_owned(), None),
            };
            Some(Expect::Error { code, message })
        }
        ("query", None, None) => Some(Expect::Rows { sort: false, columns: String::new(), rows: Vec::new() }),
        ("query", Some("rowsort"), None) => Some(Expect::Rows { sort: true, columns: String::new(), rows: Vec::new() }),
        _ => None,
    }
}

pub fn parse(path: &Path, name: &str, src: &str) -> Result<CorpusFile, String> {
    let lines: Vec<&str> = src.lines().collect();
    let mut chunks = Vec::new();
    let mut text = String::new();
    let mut connection: Option<String> = None;
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(c) = line.strip_prefix("connection ") {
            connection = Some(c.trim().to_owned());
            i += 1;
            continue;
        }
        let Some(mut expect) = (line.starts_with("statement ") || line.starts_with("query")).then(|| parse_header(line)).flatten() else {
            if connection.is_some() {
                return Err(format!("{name}:{}: `connection` must precede a record", i + 1));
            }
            text.push_str(line);
            text.push('\n');
            i += 1;
            continue;
        };
        if !text.is_empty() {
            chunks.push(Chunk::Text(std::mem::take(&mut text)));
        }
        let header = i + 1;
        i += 1;
        let mut sql = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() && lines[i] != "----" {
            sql.push(lines[i]);
            i += 1;
        }
        if sql.is_empty() {
            return Err(format!("{name}:{header}: record without SQL"));
        }
        if let Expect::Rows { columns, rows, .. } = &mut expect {
            if lines.get(i) != Some(&"----") {
                return Err(format!("{name}:{header}: query without a ---- result block"));
            }
            i += 1;
            if let Some(c) = lines.get(i).filter(|l| !l.is_empty()) {
                columns.push_str(c);
                i += 1;
                while i < lines.len() && !lines[i].is_empty() {
                    rows.push(lines[i].to_owned());
                    i += 1;
                }
            }
        } else if lines.get(i) == Some(&"----") {
            return Err(format!("{name}:{header}: only a query has a result block"));
        }
        let sql = sql.join("\n");
        let id = format!("{}\u{0}{sql}", connection.as_deref().unwrap_or(""));
        let base = format!("{name}:{:016x}", fnv1a(&id));
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        let key = if *n == 1 { base } else { format!("{base}#{n}") };
        chunks.push(Chunk::Record(Record { line: header, connection: connection.take(), sql, expect, key }));
    }
    if connection.is_some() {
        return Err(format!("{name}: trailing `connection` line"));
    }
    if !text.is_empty() {
        chunks.push(Chunk::Text(text));
    }
    Ok(CorpusFile { path: path.to_owned(), name: name.to_owned(), chunks })
}

/// Every `.slt` file under `root`, sorted by name.
pub fn discover(root: &Path) -> Result<Vec<CorpusFile>, String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else if path.extension().is_some_and(|e| e == "slt") {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(root, &mut paths).map_err(|e| format!("{}: {e}", root.display()))?;
    paths.sort();
    paths
        .iter()
        .map(|p| {
            let name =
                p.strip_prefix(root).unwrap_or(p).components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            let src = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            parse(p, &name, &src)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "# setup\nstatement ok\nCREATE TABLE t (a int)\n\nconnection b\nstatement error 23505 duplicate key\nINSERT INTO t\nVALUES (1)\n\nquery rowsort\nSELECT a FROM t\n----\na:int4\n1\nNULL\n\nquery\nSELECT 1 WHERE false\n----\n?column?:int4\n";

    #[test]
    fn parse_render_round_trip() {
        let f = parse(Path::new("x.slt"), "types/x.slt", SRC).expect("parse");
        assert_eq!(f.render(), SRC);
        let recs: Vec<_> = f.records().collect();
        assert_eq!(recs.len(), 4);
        assert_eq!(recs[1].connection.as_deref(), Some("b"));
        assert_eq!(recs[1].sql, "INSERT INTO t\nVALUES (1)");
        assert_eq!(recs[1].expect, Expect::Error { code: "23505".into(), message: Some("duplicate key".into()) });
        assert_eq!(recs[2].line, 10);
        assert_eq!(f.area(), "types");
    }

    #[test]
    fn rowsort_and_optional_message() {
        let want = Expect::Rows { sort: true, columns: "a".into(), rows: vec!["2".into(), "1".into()] };
        assert!(want.matches(&Expect::Rows { sort: false, columns: "a".into(), rows: vec!["1".into(), "2".into()] }));
        let err = Expect::Error { code: "22012".into(), message: None };
        assert!(err.matches(&Expect::Error { code: "22012".into(), message: Some("division by zero".into()) }));
        assert!(!err.matches(&Expect::Ok));
    }

    #[test]
    fn duplicate_sql_gets_distinct_keys() {
        let f = parse(Path::new("x.slt"), "x.slt", "statement ok\nSELECT 1\n\nstatement ok\nSELECT 1\n").expect("parse");
        let keys: Vec<_> = f.records().map(|r| r.key.clone()).collect();
        assert_ne!(keys[0], keys[1]);
        assert!(keys[1].ends_with("#2"));
    }
}
