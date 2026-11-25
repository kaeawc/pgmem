//! Utility statements whose grammar sqlparser covers only partly, parsed here following
//! Postgres' gram.y: `FETCH` / `MOVE` (`fetch_args`), `VACUUM` / `ANALYZE` with option lists,
//! `REINDEX` and `CLUSTER`.

use crate::object_name;
use pgmem_ir::{FetchDirection, ObjectName, Statement};
use pgmem_types::SqlResult;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

/// A statement parsed here (tables still as sqlparser names).
pub(crate) enum Utility {
    Fetch { name: sqlparser::ast::Ident, direction: FetchDirection, is_move: bool },
    Maintenance { tag: &'static str, tables: Vec<sqlparser::ast::ObjectName>, in_block_ok: bool },
}

pub(crate) fn lower(u: Utility) -> SqlResult<Statement> {
    Ok(match u {
        Utility::Fetch { name, direction, is_move } => Statement::Fetch { name: crate::ident(&name), direction, is_move },
        Utility::Maintenance { tag, tables, in_block_ok } => Statement::Maintenance {
            tag: tag.to_owned(),
            tables: tables.iter().map(object_name).collect::<SqlResult<Vec<ObjectName>>>()?,
            in_block_ok,
        },
    })
}

fn word_at(p: &Parser, n: usize, w: &str) -> bool {
    matches!(&p.peek_nth_token(n).token, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w))
}

fn eat_word(p: &mut Parser, w: &str) -> bool {
    if word_at(p, 0, w) {
        p.next_token();
        true
    } else {
        false
    }
}

pub(crate) fn try_parse(p: &mut Parser) -> Result<Option<Utility>, ParserError> {
    if word_at(p, 0, "FETCH") || word_at(p, 0, "MOVE") {
        let is_move = word_at(p, 0, "MOVE");
        p.next_token();
        return fetch(p, is_move).map(Some);
    }
    if word_at(p, 0, "VACUUM") {
        p.next_token();
        return vacuum(p, "VACUUM").map(Some);
    }
    if word_at(p, 0, "ANALYZE") || word_at(p, 0, "ANALYSE") {
        p.next_token();
        return vacuum(p, "ANALYZE").map(Some);
    }
    if word_at(p, 0, "REINDEX") {
        p.next_token();
        return reindex(p).map(Some);
    }
    if word_at(p, 0, "CHECKPOINT") {
        p.next_token();
        return Ok(Some(Utility::Maintenance { tag: "CHECKPOINT", tables: Vec::new(), in_block_ok: true }));
    }
    if word_at(p, 0, "CLUSTER") {
        p.next_token();
        return cluster(p).map(Some);
    }
    Ok(None)
}

/// `SignedIconst`, if one comes next.
fn signed_int(p: &mut Parser) -> Result<Option<i64>, ParserError> {
    let neg = match p.peek_token().token {
        Token::Minus => true,
        Token::Plus => false,
        Token::Number(..) => {
            let Token::Number(n, _) = p.next_token().token else { unreachable!() };
            return n.parse().map(Some).map_err(|_| ParserError::ParserError(format!("invalid integer {n}")));
        }
        _ => return Ok(None),
    };
    p.next_token();
    match p.next_token().token {
        Token::Number(n, _) => {
            let v: i64 = n.parse().map_err(|_| ParserError::ParserError(format!("invalid integer {n}")))?;
            Ok(Some(if neg { -v } else { v }))
        }
        other => p.expected("an integer", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
    }
}

/// gram.y `fetch_args`.
fn fetch(p: &mut Parser, is_move: bool) -> Result<Utility, ParserError> {
    use FetchDirection as D;
    let opt_from_in = |p: &mut Parser| {
        let _ = eat_word(p, "FROM") || eat_word(p, "IN");
    };
    let direction = if eat_word(p, "NEXT") {
        D::Forward(Some(1))
    } else if eat_word(p, "PRIOR") {
        D::Backward(Some(1))
    } else if eat_word(p, "FIRST") {
        D::Absolute(1)
    } else if eat_word(p, "LAST") {
        D::Absolute(-1)
    } else if eat_word(p, "ABSOLUTE") {
        match signed_int(p)? {
            Some(n) => D::Absolute(n),
            None => return p.expected("an integer", p.peek_token()),
        }
    } else if eat_word(p, "RELATIVE") {
        match signed_int(p)? {
            Some(n) => D::Relative(n),
            None => return p.expected("an integer", p.peek_token()),
        }
    } else if eat_word(p, "ALL") {
        D::Forward(None)
    } else if eat_word(p, "FORWARD") {
        if eat_word(p, "ALL") { D::Forward(None) } else { D::Forward(Some(signed_int(p)?.unwrap_or(1))) }
    } else if eat_word(p, "BACKWARD") {
        if eat_word(p, "ALL") { D::Backward(None) } else { D::Backward(Some(signed_int(p)?.unwrap_or(1))) }
    } else if let Some(n) = signed_int(p)? {
        D::Forward(Some(n))
    } else {
        D::Forward(Some(1))
    };
    opt_from_in(p);
    let name = p.parse_identifier()?;
    Ok(Utility::Fetch { name, direction, is_move })
}

/// Skips a parenthesized option list `( option [value], ... )`.
fn skip_parens(p: &mut Parser) -> Result<(), ParserError> {
    if p.peek_token().token != Token::LParen {
        return Ok(());
    }
    let mut depth = 0usize;
    loop {
        match p.next_token().token {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            Token::EOF => return p.expected(")", p.peek_token()),
            _ => {}
        }
    }
}

/// `table [(column, ...)], ...`, possibly empty.
fn table_list(p: &mut Parser) -> Result<Vec<sqlparser::ast::ObjectName>, ParserError> {
    let mut tables = Vec::new();
    while matches!(p.peek_token().token, Token::Word(_)) {
        tables.push(p.parse_object_name(false)?);
        skip_parens(p)?;
        if !p.consume_token(&Token::Comma) {
            break;
        }
    }
    Ok(tables)
}

/// `VACUUM [FULL] [FREEZE] [VERBOSE] [ANALYZE] [(options)] [table [(cols)], ...]` and
/// `ANALYZE [VERBOSE] [(options)] [table [(cols)], ...]`.
fn vacuum(p: &mut Parser, tag: &'static str) -> Result<Utility, ParserError> {
    while ["FULL", "FREEZE", "VERBOSE", "ANALYZE", "ANALYSE"].iter().any(|w| word_at(p, 0, w)) {
        p.next_token();
    }
    skip_parens(p)?;
    let tables = table_list(p)?;
    Ok(Utility::Maintenance { tag, tables, in_block_ok: tag == "ANALYZE" })
}

/// `REINDEX [(options)] {INDEX | TABLE | SCHEMA | DATABASE | SYSTEM} [CONCURRENTLY] [name]`.
fn reindex(p: &mut Parser) -> Result<Utility, ParserError> {
    skip_parens(p)?;
    let kind = match p.next_token().token {
        Token::Word(w) => w.value.to_ascii_uppercase(),
        other => return p.expected("INDEX, TABLE, SCHEMA, DATABASE or SYSTEM", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
    };
    let concurrently = eat_word(p, "CONCURRENTLY");
    let name = if matches!(p.peek_token().token, Token::Word(_)) { Some(p.parse_object_name(false)?) } else { None };
    let (tables, in_block_ok) = match (kind.as_str(), name) {
        ("INDEX" | "TABLE", Some(n)) => (vec![n], !concurrently),
        ("SCHEMA" | "DATABASE" | "SYSTEM", _) => (Vec::new(), false),
        _ => return p.expected("a name", p.peek_token()),
    };
    Ok(Utility::Maintenance { tag: "REINDEX", tables, in_block_ok })
}

/// `CLUSTER [VERBOSE] [(options)] [table [USING index]]`.
fn cluster(p: &mut Parser) -> Result<Utility, ParserError> {
    let _ = eat_word(p, "VERBOSE");
    skip_parens(p)?;
    let mut tables = Vec::new();
    if matches!(p.peek_token().token, Token::Word(_)) {
        tables.push(p.parse_object_name(false)?);
        if eat_word(p, "USING") {
            let _ = p.parse_identifier()?;
        }
    }
    let in_block_ok = !tables.is_empty();
    Ok(Utility::Maintenance { tag: "CLUSTER", tables, in_block_ok })
}
