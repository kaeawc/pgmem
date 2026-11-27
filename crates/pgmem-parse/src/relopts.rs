//! Storage-parameter syntax sqlparser lacks or accepts wrongly: `WITH (...)` on PRIMARY KEY /
//! UNIQUE / EXCLUDE constraints, `PARTITION BY ... WITH (...)` clause order, empty option
//! lists, and the integer literal forms a storage parameter value may take.

use crate::{Tokens, is_word, matching_paren};
use pgmem_ir::RelOption;
use pgmem_types::{SqlError, SqlResult};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};
use std::cell::RefCell;
use std::collections::HashMap;

/// The constraint options the rewrite stripped: (`p`rimary key / `u`nique / e`x`clude, key
/// columns, options), in statement order.
type Recorded = (char, Vec<String>, Vec<RelOption>);

thread_local! {
    static CONSTRAINT_OPTIONS: RefCell<Vec<Recorded>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn clear() {
    CONSTRAINT_OPTIONS.with(|c| c.borrow_mut().clear());
}

/// The `WITH (...)` options recorded for the constraint of `kind` over `columns` (empty for an
/// EXCLUDE: the next one), consumed.
pub(crate) fn take(kind: char, columns: &[String]) -> Vec<RelOption> {
    CONSTRAINT_OPTIONS.with(|c| {
        let mut c = c.borrow_mut();
        let found = if kind == 'x' {
            c.iter().position(|(k, _, _)| *k == 'x')
        } else {
            c.iter().position(|(k, cols, _)| *k == kind && cols == columns)
        };
        found.map(|i| c.remove(i).2).unwrap_or_default()
    })
}

/// An integer literal as Postgres' lexer reads it (decimal, `0x`, `0o`, `0b`, `_` separators):
/// its decimal text. Anything else, or a value beyond int4 (a numeric literal there), as written.
pub(crate) fn int_literal(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let (radix, digits) = match lower.get(..2) {
        Some("0x") => (16, &lower[2..]),
        Some("0o") => (8, &lower[2..]),
        Some("0b") => (2, &lower[2..]),
        _ => (10, lower.as_str()),
    };
    let separated = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.chars().all(|c| c == '_' || c.is_digit(radix));
    if !separated {
        return text.to_owned();
    }
    match i64::from_str_radix(&digits.replace('_', ""), radix) {
        Ok(v) if v <= i64::from(i32::MAX) => v.to_string(),
        _ => text.to_owned(),
    }
}

fn fold(w: &sqlparser::tokenizer::Word) -> String {
    if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() }
}

/// The option list inside `sig[open + 1..close]` (`name [= value], ...`), or None when it is
/// not a plain one.
fn parse_options(tokens: &Tokens, sig: &[usize], open: usize, close: usize) -> Option<Vec<RelOption>> {
    let mut out = Vec::new();
    for item in sig[open + 1..close].split(|&j| tokens[j] == Token::Comma) {
        let mut parts: Vec<String> = Vec::new();
        let mut rest = item;
        while let [name, dot, tail @ ..] = rest
            && tokens[*dot] == Token::Period
            && let Token::Word(w) = &tokens[*name]
        {
            parts.push(fold(w));
            rest = tail;
        }
        let (name, tail) = match rest {
            [name, tail @ ..] => (name, tail),
            [] => return None,
        };
        let Token::Word(w) = &tokens[*name] else { return None };
        parts.push(fold(w));
        let value = match tail {
            [] => None,
            [eq, v @ ..] if tokens[*eq] == Token::Eq => Some(match v {
                [v] => value_text(&tokens[*v])?,
                [minus, v] if tokens[*minus] == Token::Minus => format!("-{}", value_text(&tokens[*v])?),
                _ => return None,
            }),
            _ => return None,
        };
        let (namespace, name) = match <[String; 1]>::try_from(parts) {
            Ok([n]) => (None, n),
            Err(parts) => match <[String; 2]>::try_from(parts) {
                Ok([ns, n]) => (Some(ns), n),
                Err(_) => return None,
            },
        };
        out.push(RelOption { namespace, name, value });
    }
    Some(out)
}

fn value_text(t: &Token) -> Option<String> {
    match t {
        Token::Number(n, _) => Some(int_literal(n)),
        Token::SingleQuotedString(s) => Some(s.clone()),
        Token::Word(w) => Some(fold(w)),
        _ => None,
    }
}

/// The statement's first significant token and whether it is `CREATE ... TABLE` (true) or
/// `ALTER TABLE` (false); None for any other statement.
fn table_statement(tokens: &Tokens, sig: &[usize], start: usize) -> Option<bool> {
    if is_word(tokens, sig, start, "ALTER") && is_word(tokens, sig, start + 1, "TABLE") {
        return Some(false);
    }
    (is_word(tokens, sig, start, "CREATE") && (1..=4).any(|o| is_word(tokens, sig, start + o, "TABLE"))).then_some(true)
}

/// `PRIMARY KEY | UNIQUE | EXCLUDE ... WITH (options)`: records the options by constraint and
/// strips the clause.
pub(crate) fn rewrite_constraint_with(tokens: &Tokens, sig: &[usize], k: usize, drop: &mut [bool]) {
    if !(is_word(tokens, sig, k, "WITH") && sig.get(k + 1).is_some_and(|&j| tokens[j] == Token::LParen)) {
        return;
    }
    let start = sig[..k].iter().rposition(|&j| tokens[j] == Token::SemiColon).map_or(0, |p| p + 1);
    let Some(create) = table_statement(tokens, sig, start) else { return };
    // Constraints sit inside CREATE TABLE's element list, at the top level of ALTER TABLE.
    let depth = sig[start..k].iter().fold(0i32, |d, &j| match tokens[j] {
        Token::LParen => d + 1,
        Token::RParen => d - 1,
        _ => d,
    });
    if depth != i32::from(create) {
        return;
    }
    // Back to the start of the element, noting the nearest constraint keyword.
    let (mut kind, mut kw, mut nested) = (None, 0, 0i32);
    let mut elem = start;
    for n in (start..k).rev() {
        match &tokens[sig[n]] {
            Token::RParen => nested += 1,
            Token::LParen if nested == 0 => {
                elem = n + 1;
                break;
            }
            Token::LParen => nested -= 1,
            Token::Comma if nested == 0 => {
                elem = n + 1;
                break;
            }
            _ if nested == 0 && !create && is_word(tokens, sig, n, "ADD") => {
                elem = n + 1;
                break;
            }
            _ if nested == 0 && kind.is_none() => {
                for (word, k) in [("PRIMARY", 'p'), ("UNIQUE", 'u'), ("EXCLUDE", 'x')] {
                    if is_word(tokens, sig, n, word) {
                        (kind, kw) = (Some(k), n);
                    }
                }
            }
            _ => {}
        }
    }
    let Some(kind) = kind else { return };
    let Some(close) = matching_paren(tokens, sig, k + 1) else { return };
    let Some(options) = parse_options(tokens, sig, k + 1, close) else { return };
    let columns = if kind == 'x' { Vec::new() } else { key_columns(tokens, sig, kw, elem, create) };
    if kind != 'x' && columns.is_empty() {
        return;
    }
    for &j in &sig[k..=close] {
        drop[j] = true;
    }
    // `USING INDEX TABLESPACE name` has no meaning here.
    if is_word(tokens, sig, close + 1, "USING") && is_word(tokens, sig, close + 2, "INDEX") && is_word(tokens, sig, close + 3, "TABLESPACE")
    {
        for &j in sig.iter().skip(close + 1).take(4) {
            drop[j] = true;
        }
    }
    CONSTRAINT_OPTIONS.with(|c| c.borrow_mut().push((kind, columns, options)));
}

/// The columns of the PRIMARY KEY / UNIQUE whose keyword is at `sig[kw]`: its parenthesized list
/// (a table constraint), else the column the constraint is written on (starting at `sig[elem]`).
fn key_columns(tokens: &Tokens, sig: &[usize], kw: usize, elem: usize, create: bool) -> Vec<String> {
    let mut n = kw + 1;
    if is_word(tokens, sig, n, "KEY") {
        n += 1;
    }
    if is_word(tokens, sig, n, "NULLS") {
        n += 1 + usize::from(is_word(tokens, sig, n + 1, "NOT")) + 1;
    }
    let first_word = |at: usize| match sig.get(at).map(|&j| &tokens[j]) {
        Some(Token::Word(w)) => Some(fold(w)),
        _ => None,
    };
    if sig.get(n).is_some_and(|&j| tokens[j] == Token::LParen) {
        let Some(close) = matching_paren(tokens, sig, n) else { return Vec::new() };
        return sig[n + 1..close]
            .split(|&j| tokens[j] == Token::Comma)
            .filter_map(|g| g.first().and_then(|&j| first_word_at(tokens, j)))
            .collect();
    }
    let mut at = elem;
    if !create {
        if is_word(tokens, sig, at, "COLUMN") {
            at += 1;
        }
        if is_word(tokens, sig, at, "IF") {
            at += 3;
        }
    }
    first_word(at).into_iter().collect()
}

fn first_word_at(tokens: &Tokens, j: usize) -> Option<String> {
    match &tokens[j] {
        Token::Word(w) => Some(fold(w)),
        _ => None,
    }
}

/// `CREATE TABLE ... PARTITION BY ... WITH (...)` (Postgres' clause order) moves the WITH list
/// before PARTITION BY, where sqlparser expects it.
pub(crate) fn rewrite_partition_with(
    tokens: &Tokens,
    sig: &[usize],
    k: usize,
    drop: &mut [bool],
    insert_after: &mut HashMap<usize, String>,
) {
    if !(is_word(tokens, sig, k, "PARTITION") && is_word(tokens, sig, k + 1, "BY") && k > 0) {
        return;
    }
    let start = sig[..k].iter().rposition(|&j| tokens[j] == Token::SemiColon).map_or(0, |p| p + 1);
    if table_statement(tokens, sig, start) != Some(true) {
        return;
    }
    let Some(open) = (k + 2..sig.len()).find(|&n| tokens[sig[n]] == Token::LParen) else { return };
    let Some(close) = matching_paren(tokens, sig, open) else { return };
    if !(is_word(tokens, sig, close + 1, "WITH") && sig.get(close + 2).is_some_and(|&j| tokens[j] == Token::LParen)) {
        return;
    }
    let Some(wclose) = matching_paren(tokens, sig, close + 2) else { return };
    let text: Vec<String> = sig[close + 1..=wclose].iter().map(|&j| tokens[j].to_string()).collect();
    for &j in &sig[close + 1..=wclose] {
        drop[j] = true;
    }
    insert_after.insert(sig[k - 1], format!(" {}", text.join(" ")));
}

/// Errors Postgres' grammar raises where sqlparser accepts or garbles the statement:
/// `WITH (...)` before `PARTITION BY`, and an empty `SET ()` / `RESET ()` / `WITH ()`.
pub(crate) fn precheck(sql: &str) -> SqlResult<()> {
    let lower = sql.to_ascii_lowercase();
    if !lower.contains("with") && !lower.contains("set") {
        return Ok(());
    }
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize() else { return Ok(()) };
    let sig: Vec<usize> = tokens.iter().enumerate().filter(|(_, t)| !matches!(t, Token::Whitespace(_))).map(|(i, _)| i).collect();
    let err = |j: usize| SqlError::syntax(format!("syntax error at or near \"{}\"", tokens[j]));
    for (k, &i) in sig.iter().enumerate() {
        let Token::Word(w) = &tokens[i] else { continue };
        if w.quote_style.is_some() {
            continue;
        }
        let start = sig[..k].iter().rposition(|&j| tokens[j] == Token::SemiColon).map_or(0, |p| p + 1);
        let empty_list =
            sig.get(k + 1).is_some_and(|&j| tokens[j] == Token::LParen) && sig.get(k + 2).is_some_and(|&j| tokens[j] == Token::RParen);
        let alter = is_word(&tokens, &sig, start, "ALTER");
        if empty_list
            && ((alter && (is_word(&tokens, &sig, k, "SET") || is_word(&tokens, &sig, k, "RESET"))) || is_word(&tokens, &sig, k, "WITH"))
        {
            return Err(err(sig[k + 2]));
        }
        // `CREATE TABLE ... WITH (...) PARTITION BY`: the clauses come the other way round.
        if is_word(&tokens, &sig, k, "WITH")
            && table_statement(&tokens, &sig, start) == Some(true)
            && sig.get(k + 1).is_some_and(|&j| tokens[j] == Token::LParen)
            && !is_word(&tokens, &sig, k.wrapping_sub(1), "VALUES")
            && let Some(close) = matching_paren(&tokens, &sig, k + 1)
            && is_word(&tokens, &sig, close + 1, "PARTITION")
            && is_word(&tokens, &sig, close + 2, "BY")
        {
            return Err(err(sig[close + 1]));
        }
    }
    Ok(())
}
