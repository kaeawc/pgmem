//! The `hstore` extension's text and binary forms (`hstore_io.c`).

use crate::{SqlError, SqlResult, code};
use std::cmp::Ordering;

/// hstore's key order: shorter keys first, then bytewise.
fn key_order(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

/// Sorts pairs into hstore order, keeping the first value of a duplicated key.
pub fn normalize(mut pairs: Vec<(String, Option<String>)>) -> Vec<(String, Option<String>)> {
    // A stable sort keeps the first occurrence of each key first.
    pairs.sort_by(|a, b| key_order(&a.0, &b.0));
    pairs.dedup_by(|b, a| a.0 == b.0);
    pairs
}

/// Merges `right` into `left` (`||`: right's values win).
pub fn concat(left: &[(String, Option<String>)], right: &[(String, Option<String>)]) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = right.to_vec();
    out.extend(left.iter().cloned());
    normalize(out)
}

fn syntax(s: &str) -> SqlError {
    SqlError::new(code::SYNTAX_ERROR, "syntax error in hstore: unexpected end of string").with_detail(s.to_owned())
}

/// One token: a double-quoted string (with `\` escapes) or a bare word up to a delimiter.
fn token(chars: &[char], i: &mut usize) -> Option<(String, bool)> {
    while *i < chars.len() && chars[*i].is_whitespace() {
        *i += 1;
    }
    if *i >= chars.len() {
        return None;
    }
    let mut out = String::new();
    if chars[*i] == '"' {
        *i += 1;
        while *i < chars.len() && chars[*i] != '"' {
            if chars[*i] == '\\' && *i + 1 < chars.len() {
                *i += 1;
            }
            out.push(chars[*i]);
            *i += 1;
        }
        *i += 1;
        return Some((out, true));
    }
    while *i < chars.len() && !chars[*i].is_whitespace() && chars[*i] != ',' && chars[*i] != '=' {
        if chars[*i] == '\\' && *i + 1 < chars.len() {
            *i += 1;
        }
        out.push(chars[*i]);
        *i += 1;
    }
    Some((out, false))
}

/// `hstore_in`: `key => value, ...` (keys and values optionally double-quoted, `NULL`
/// unquoted for a NULL value).
pub fn parse(s: &str) -> SqlResult<Vec<(String, Option<String>)>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut pairs = Vec::new();
    let bad = || {
        SqlError::new(
            code::SYNTAX_ERROR,
            format!("syntax error in hstore, near \"{}\" at position {}", s.chars().last().unwrap_or(' '), s.len()),
        )
    };
    while let Some((key, _)) = token(&chars, &mut i) {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if !(chars.get(i) == Some(&'=') && chars.get(i + 1) == Some(&'>')) {
            return Err(bad());
        }
        i += 2;
        let (value, quoted) = token(&chars, &mut i).ok_or_else(|| syntax(s))?;
        let value = if !quoted && value.eq_ignore_ascii_case("null") { None } else { Some(value) };
        pairs.push((key, value));
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        match chars.get(i) {
            Some(',') => i += 1,
            None => break,
            Some(_) => return Err(bad()),
        }
    }
    Ok(normalize(pairs))
}

fn quote(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `hstore_out`: `"a"=>"1", "b"=>NULL`.
pub fn format(pairs: &[(String, Option<String>)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}=>{}", quote(k), v.as_deref().map_or_else(|| "NULL".to_owned(), quote)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `hstore_send`.
pub fn to_binary(pairs: &[(String, Option<String>)]) -> Vec<u8> {
    let len = |n: usize| i32::try_from(n).unwrap_or(i32::MAX).to_be_bytes();
    let mut out = len(pairs.len()).to_vec();
    for (k, v) in pairs {
        out.extend_from_slice(&len(k.len()));
        out.extend_from_slice(k.as_bytes());
        match v {
            Some(v) => {
                out.extend_from_slice(&len(v.len()));
                out.extend_from_slice(v.as_bytes());
            }
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
        }
    }
    out
}

pub fn from_binary(b: &[u8]) -> SqlResult<Vec<(String, Option<String>)>> {
    let bad = || SqlError::new(code::INVALID_BINARY_REPRESENTATION, "invalid hstore value");
    let n = i32::from_be_bytes(b.get(..4).ok_or_else(bad)?.try_into().map_err(|_| bad())?);
    let mut pairs = Vec::new();
    let mut p = 4usize;
    for _ in 0..n {
        let kl = usize::try_from(i32::from_be_bytes(b.get(p..p + 4).ok_or_else(bad)?.try_into().map_err(|_| bad())?)).map_err(|_| bad())?;
        p += 4;
        let k = String::from_utf8(b.get(p..p + kl).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
        p += kl;
        let vl = i32::from_be_bytes(b.get(p..p + 4).ok_or_else(bad)?.try_into().map_err(|_| bad())?);
        p += 4;
        let v = if vl < 0 {
            None
        } else {
            let vl = usize::try_from(vl).map_err(|_| bad())?;
            let v = String::from_utf8(b.get(p..p + vl).ok_or_else(bad)?.to_vec()).map_err(|_| bad())?;
            p += vl;
            Some(v)
        };
        pairs.push((k, v));
    }
    Ok(normalize(pairs))
}
