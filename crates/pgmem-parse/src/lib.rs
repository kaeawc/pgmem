//! SQL text → [`pgmem_ir::Statement`]s.
//!
//! Parsing is delegated to `sqlparser`'s PostgreSQL dialect; this crate lowers its AST into the
//! dialect-neutral IR, folds identifiers the way Postgres does (unquoted → lower case), rewrites
//! special syntax into function calls the way Postgres' grammar does, and maps parse failures
//! to `42601 syntax_error` with Postgres-style messages.
//!
//! A handful of utility statements that `sqlparser` doesn't understand but that clients and
//! migration tools routinely send (`SET` variants, `COMMENT ON`, `CREATE EXTENSION`, ...) are
//! recognized by prefix as a fallback and become no-ops, so connection setup never fails.

mod ddl;
mod dml;
mod expr;
mod query;

use pgmem_ir::{IsolationLevel, ObjectName, ObjectType, Statement, TransactionStmt};
use pgmem_types::{SqlError, SqlResult, code};
use sqlparser::ast as sq;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};

/// Parses a query string that may contain several `;`-separated statements (the simple-query
/// protocol). An empty string yields `[Statement::Empty]`.
pub fn parse(sql: &str) -> SqlResult<Vec<Statement>> {
    if sql.trim().trim_matches(';').trim().is_empty() {
        return Ok(vec![Statement::Empty]);
    }
    let parsed = Parser::parse_sql(&PostgreSqlDialect {}, sql).or_else(|err| match rewrite_unsupported(sql) {
        // Retry Postgres syntax sqlparser lacks after rewriting it to an equivalent form.
        Some(rewritten) => Parser::parse_sql(&PostgreSqlDialect {}, &rewritten).map_err(|_| err),
        None => Err(err),
    });
    match parsed {
        Ok(stmts) => stmts.into_iter().map(lower_statement).collect(),
        Err(err) => {
            // Fall back to per-statement prefix recognition for the utility statements
            // sqlparser doesn't model; only when every piece is recognized do we accept it.
            let pieces: Vec<&str> = split_statements(sql);
            let fallback: Option<Vec<Statement>> = pieces.iter().map(|p| fallback_statement(p)).collect();
            fallback.ok_or_else(|| map_parser_error(&err, sql))
        }
    }
}

/// Parses exactly one statement (the extended-query `Parse` message). Postgres rejects
/// multiple commands in a prepared statement with 42601.
pub fn parse_one(sql: &str) -> SqlResult<Statement> {
    let mut stmts = parse(sql)?;
    if stmts.len() > 1 {
        return Err(SqlError::syntax("cannot insert multiple commands into a prepared statement"));
    }
    Ok(stmts.pop().unwrap_or(Statement::Empty))
}

/// Rewrites Postgres syntax that sqlparser does not accept into equivalent syntax it does,
/// for constructs pgmem treats identically: `FOR NO KEY UPDATE` / `FOR KEY SHARE` row locks
/// (row locks are no-ops) and `DROP INDEX CONCURRENTLY` / `REINDEX`-style modifiers. Works on
/// tokens, so string literals and identifiers are never touched. Returns None if nothing
/// changed.
fn rewrite_unsupported(sql: &str) -> Option<String> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize().ok()?;
    let word = |t: &Token, w: &str| matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    // Indexes of significant (non-whitespace) tokens.
    let sig: Vec<usize> = tokens.iter().enumerate().filter(|(_, t)| !matches!(t, Token::Whitespace(_))).map(|(i, _)| i).collect();
    let mut drop = vec![false; tokens.len()];
    for (k, &i) in sig.iter().enumerate() {
        let at = |o: usize, w: &str| sig.get(k + o).is_some_and(|&j| word(&tokens[j], w));
        if word(&tokens[i], "FOR") && at(1, "NO") && at(2, "KEY") && at(3, "UPDATE") {
            drop[sig[k + 1]] = true;
            drop[sig[k + 2]] = true;
        } else if word(&tokens[i], "FOR") && at(1, "KEY") && at(2, "SHARE") {
            drop[sig[k + 1]] = true;
        } else if word(&tokens[i], "DROP") && at(1, "INDEX") && at(2, "CONCURRENTLY") {
            drop[sig[k + 2]] = true;
        }
    }
    if !drop.contains(&true) {
        return None;
    }
    Some(tokens.iter().zip(drop).filter(|(_, d)| !d).map(|(t, _)| t.to_string()).collect())
}

/// Splits on top-level semicolons, respecting quotes and dollar quotes. Only used by the
/// fallback path, so it doesn't need to be exhaustive.
fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'\'' || c == b'"' => quote = Some(c),
            None if c == b';' => {
                out.push(&sql[start..i]);
                start = i + 1;
            }
            None => {}
        }
        i += 1;
    }
    out.push(&sql[start..]);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Prefix recognition for utility statements sqlparser can't parse.
fn fallback_statement(sql: &str) -> Option<Statement> {
    let upper = sql.trim().trim_end_matches(';').trim().to_ascii_uppercase();
    let words: Vec<&str> = upper.split_whitespace().collect();
    let first = *words.first()?;
    let second = words.get(1).copied().unwrap_or("");
    let tag = match (first, second) {
        ("SET", _) => "SET",
        ("RESET", _) => "RESET",
        ("DISCARD", "ALL") => "DISCARD ALL",
        ("DISCARD", _) => "DISCARD",
        ("COMMENT", "ON") => "COMMENT",
        ("CREATE", "EXTENSION") => "CREATE EXTENSION",
        ("DROP", "EXTENSION") => "DROP EXTENSION",
        ("CREATE", "SCHEMA") => "CREATE SCHEMA",
        ("DROP", "SCHEMA") => "DROP SCHEMA",
        ("ANALYZE" | "ANALYSE", _) => "ANALYZE",
        ("VACUUM", _) => "VACUUM",
        ("GRANT", _) => "GRANT",
        ("REVOKE", _) => "REVOKE",
        ("ALTER", "DEFAULT") => "ALTER DEFAULT PRIVILEGES",
        ("CHECKPOINT", _) => "CHECKPOINT",
        _ => return None,
    };
    Some(Statement::NoOp { tag: tag.to_owned() })
}

/// Turns a sqlparser error into Postgres' `syntax error at or near "X"` (42601).
fn map_parser_error(err: &ParserError, sql: &str) -> SqlError {
    let text = err.to_string();
    if let ParserError::RecursionLimitExceeded = err {
        return SqlError::new("54001", "stack depth limit exceeded");
    }
    let found = text.split("found: ").nth(1).map(|rest| rest.split(" at Line:").next().unwrap_or(rest).trim().to_owned());
    let message = match found.as_deref() {
        Some("EOF") | None if text.contains("EOF") => "syntax error at end of input".to_owned(),
        Some(tok) => format!("syntax error at or near \"{}\"", tok.trim_matches('\'')),
        None => format!("syntax error: {text}"),
    };
    let _ = sql;
    SqlError::syntax(message).with_detail(text)
}

/// Postgres identifier folding: unquoted identifiers are lower-cased.
pub(crate) fn ident(i: &sq::Ident) -> String {
    if i.quote_style.is_some() { i.value.clone() } else { i.value.to_lowercase() }
}

pub(crate) fn object_name(name: &sq::ObjectName) -> SqlResult<ObjectName> {
    let mut parts = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        match part {
            sq::ObjectNamePart::Identifier(i) => parts.push(ident(i)),
            sq::ObjectNamePart::Function(_) => return Err(unsupported("function-valued name parts")),
        }
    }
    Ok(ObjectName(parts))
}

pub(crate) fn unsupported(what: &str) -> SqlError {
    SqlError::new(code::FEATURE_NOT_SUPPORTED, format!("{what} is not supported by pgmem"))
}

fn lower_statement(stmt: sq::Statement) -> SqlResult<Statement> {
    use sq::Statement as S;
    Ok(match stmt {
        S::Query(q) => Statement::Query(Box::new(query::lower_query(*q)?)),
        S::Insert(i) => Statement::Insert(Box::new(dml::lower_insert(i)?)),
        S::Update(u) => Statement::Update(Box::new(dml::lower_update(u)?)),
        S::Delete(d) => Statement::Delete(Box::new(dml::lower_delete(d)?)),
        S::CreateTable(ct) => Statement::CreateTable(Box::new(ddl::lower_create_table(ct)?)),
        S::CreateView(cv) => Statement::CreateView(Box::new(ddl::lower_create_view(cv)?)),
        S::CreateIndex(ci) => Statement::CreateIndex(Box::new(ddl::lower_create_index(ci)?)),
        S::AlterTable(at) => Statement::AlterTable(Box::new(ddl::lower_alter_table(at)?)),
        S::CreateSequence { name, if_not_exists, sequence_options, .. } => {
            Statement::CreateSequence(Box::new(ddl::lower_create_sequence(&name, if_not_exists, &sequence_options)?))
        }
        S::Drop { object_type, if_exists, names, cascade, .. } => {
            let object_type = match object_type {
                sq::ObjectType::Table => ObjectType::Table,
                sq::ObjectType::View | sq::ObjectType::MaterializedView => ObjectType::View,
                sq::ObjectType::Index => ObjectType::Index,
                sq::ObjectType::Sequence => ObjectType::Sequence,
                sq::ObjectType::Schema => ObjectType::Schema,
                sq::ObjectType::Type => ObjectType::Type,
                other => return Err(unsupported(&format!("DROP {other}"))),
            };
            let names = names.iter().map(object_name).collect::<SqlResult<Vec<_>>>()?;
            Statement::Drop(Box::new(pgmem_ir::Drop { object_type, names, if_exists, cascade }))
        }
        S::Truncate(t) => Statement::Truncate(Box::new(pgmem_ir::Truncate {
            tables: t.table_names.iter().map(|tt| object_name(&tt.name)).collect::<SqlResult<Vec<_>>>()?,
            restart_identity: matches!(t.identity, Some(sq::TruncateIdentityOption::Restart)),
            cascade: matches!(t.cascade, Some(sq::CascadeOption::Cascade)),
        })),
        S::StartTransaction { modes, .. } => {
            let (isolation, read_only) = transaction_modes(&modes);
            Statement::Transaction(TransactionStmt::Begin { isolation, read_only })
        }
        S::Commit { chain, .. } => Statement::Transaction(TransactionStmt::Commit { chain }),
        S::Rollback { chain, savepoint } => match savepoint {
            Some(name) => Statement::Transaction(TransactionStmt::RollbackTo(ident(&name))),
            None => Statement::Transaction(TransactionStmt::Rollback { chain }),
        },
        S::Savepoint { name } => Statement::Transaction(TransactionStmt::Savepoint(ident(&name))),
        S::ReleaseSavepoint { name } => Statement::Transaction(TransactionStmt::Release(ident(&name))),
        S::Set(set) => lower_set(set)?,
        S::Reset(r) => match r.reset {
            sq::Reset::ALL => Statement::Reset { name: None },
            sq::Reset::ConfigurationParameter(name) => Statement::Reset { name: Some(object_name(&name)?.to_string()) },
            sq::Reset::SessionAuthorization => Statement::NoOp { tag: "RESET".into() },
        },
        S::ShowVariable { variable } => {
            Statement::Show { name: variable.iter().map(ident).collect::<Vec<_>>().join(" ").to_ascii_lowercase() }
        }
        S::Discard { object_type } => Statement::NoOp {
            tag: match object_type {
                sq::DiscardObject::ALL => "DISCARD ALL",
                sq::DiscardObject::PLANS => "DISCARD PLANS",
                sq::DiscardObject::SEQUENCES => "DISCARD SEQUENCES",
                sq::DiscardObject::TEMP => "DISCARD TEMP",
            }
            .into(),
        },
        S::Comment { .. } => Statement::NoOp { tag: "COMMENT".into() },
        S::CreateExtension(_) => Statement::NoOp { tag: "CREATE EXTENSION".into() },
        S::DropExtension(_) => Statement::NoOp { tag: "DROP EXTENSION".into() },
        S::CreateSchema { .. } => Statement::NoOp { tag: "CREATE SCHEMA".into() },
        S::Analyze(_) => Statement::NoOp { tag: "ANALYZE".into() },
        S::Vacuum(_) => Statement::NoOp { tag: "VACUUM".into() },
        S::Grant(_) | S::GrantRole(_) => Statement::NoOp { tag: "GRANT".into() },
        S::Revoke(_) | S::RevokeRole(_) => Statement::NoOp { tag: "REVOKE".into() },
        other => {
            let text = other.to_string();
            let head: String = text.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            return Err(unsupported(&format!("statement {head}")));
        }
    })
}

fn transaction_modes(modes: &[sq::TransactionMode]) -> (Option<IsolationLevel>, Option<bool>) {
    let mut isolation = None;
    let mut read_only = None;
    for m in modes {
        match m {
            sq::TransactionMode::IsolationLevel(level) => {
                isolation = Some(match level {
                    sq::TransactionIsolationLevel::ReadUncommitted => IsolationLevel::ReadUncommitted,
                    sq::TransactionIsolationLevel::ReadCommitted => IsolationLevel::ReadCommitted,
                    sq::TransactionIsolationLevel::RepeatableRead | sq::TransactionIsolationLevel::Snapshot => {
                        IsolationLevel::RepeatableRead
                    }
                    sq::TransactionIsolationLevel::Serializable => IsolationLevel::Serializable,
                });
            }
            sq::TransactionMode::AccessMode(mode) => read_only = Some(matches!(mode, sq::TransactionAccessMode::ReadOnly)),
        }
    }
    (isolation, read_only)
}

/// The text form of a SET value: identifiers and strings by value, everything else as written.
fn set_value(e: &sq::Expr) -> String {
    match e {
        sq::Expr::Identifier(i) => ident(i),
        sq::Expr::Value(v) => match &v.value {
            sq::Value::SingleQuotedString(s) | sq::Value::EscapedStringLiteral(s) => s.clone(),
            sq::Value::Number(n, _) => n.clone(),
            sq::Value::Boolean(b) => if *b { "on" } else { "off" }.to_owned(),
            other => other.to_string(),
        },
        other => other.to_string(),
    }
}

fn lower_set(set: sq::Set) -> SqlResult<Statement> {
    Ok(match set {
        sq::Set::SingleAssignment { scope, variable, values, .. } => Statement::Set {
            name: object_name(&variable)?.to_string().to_ascii_lowercase(),
            value: values.iter().map(set_value).collect(),
            local: matches!(scope, Some(sq::ContextModifier::Local)),
        },
        sq::Set::SetTimeZone { local, value } => Statement::Set { name: "timezone".into(), value: vec![set_value(&value)], local },
        sq::Set::SetNames { charset_name, .. } => {
            Statement::Set { name: "client_encoding".into(), value: vec![ident(&charset_name)], local: false }
        }
        sq::Set::SetTransaction { modes, .. } => {
            let (isolation, read_only) = transaction_modes(&modes);
            Statement::Transaction(TransactionStmt::SetTransaction { isolation, read_only })
        }
        _ => Statement::NoOp { tag: "SET".into() },
    })
}

#[cfg(test)]
mod go_port;
#[cfg(test)]
mod tests;
