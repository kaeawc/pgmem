//! `COPY`, parsed following gram.y's `CopyStmt`: both the generic option list
//! (`WITH (FORMAT csv, HEADER)`) and the legacy options (`WITH CSV HEADER`), which are mapped
//! to their generic names. Options are validated when the statement runs, as in Postgres.

use crate::{ident, object_name};
use pgmem_ir::{CopyOption, CopyOptionValue, CopyTarget, Statement};
use pgmem_types::SqlResult;
use sqlparser::ast as sq;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

pub(crate) struct RawCopy {
    table: Option<sq::ObjectName>,
    columns: Vec<sq::Ident>,
    query: Option<sq::Statement>,
    from: bool,
    target: CopyTarget,
    options: Vec<CopyOption>,
    where_clause: Option<sq::Expr>,
}

pub(crate) fn lower(c: RawCopy) -> SqlResult<Statement> {
    Ok(Statement::Copy(Box::new(pgmem_ir::Copy {
        table: c.table.as_ref().map(object_name).transpose()?,
        columns: c.columns.iter().map(ident).collect(),
        query: c.query.map(crate::lower_statement).transpose()?.map(Box::new),
        from: c.from,
        target: c.target,
        options: c.options,
        where_clause: c.where_clause.map(crate::expr::lower_expr).transpose()?,
    })))
}

fn word(p: &Parser, n: usize) -> Option<String> {
    match &p.peek_nth_token(n).token {
        Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    }
}

fn eat(p: &mut Parser, w: &str) -> bool {
    if word(p, 0).as_deref() == Some(w) {
        p.next_token();
        true
    } else {
        false
    }
}

/// After `COPY`.
pub(crate) fn parse(p: &mut Parser) -> Result<RawCopy, ParserError> {
    let (table, columns, query) = if p.consume_token(&Token::LParen) {
        let q = p.parse_statement()?;
        p.expect_token(&Token::RParen)?;
        (None, Vec::new(), Some(q))
    } else {
        let name = p.parse_object_name(false)?;
        let cols = if p.peek_token().token == Token::LParen {
            p.parse_parenthesized_column_list(sqlparser::parser::IsOptional::Mandatory, false)?
        } else {
            Vec::new()
        };
        (Some(name), cols, None)
    };
    let from = match word(p, 0).as_deref() {
        Some("FROM") => true,
        Some("TO") => false,
        _ => return p.expected("FROM or TO", p.peek_token()),
    };
    p.next_token();
    let target = if eat(p, "STDIN") || eat(p, "STDOUT") {
        CopyTarget::Stdio
    } else if eat(p, "PROGRAM") {
        CopyTarget::Program(p.parse_literal_string()?)
    } else {
        CopyTarget::File(p.parse_literal_string()?)
    };
    let mut options = Vec::new();
    eat(p, "WITH");
    if p.consume_token(&Token::LParen) {
        loop {
            options.push(generic_option(p)?);
            if !p.consume_token(&Token::Comma) {
                break;
            }
        }
        p.expect_token(&Token::RParen)?;
    } else {
        while let Some(o) = legacy_option(p)? {
            options.push(o);
        }
    }
    let where_clause = if eat(p, "WHERE") { Some(p.parse_expr()?) } else { None };
    Ok(RawCopy { table, columns, query, from, target, options, where_clause })
}

fn opt(name: &str, value: CopyOptionValue) -> CopyOption {
    CopyOption { name: name.to_owned(), value }
}

fn column_list(p: &mut Parser) -> Result<CopyOptionValue, ParserError> {
    if p.consume_token(&Token::Mul) {
        return Ok(CopyOptionValue::Star);
    }
    let cols = p.parse_parenthesized_column_list(sqlparser::parser::IsOptional::Mandatory, false)?;
    Ok(CopyOptionValue::List(cols.iter().map(ident).collect()))
}

/// `name [argument]` in a parenthesized option list.
fn generic_option(p: &mut Parser) -> Result<CopyOption, ParserError> {
    let name = match p.next_token().token {
        Token::Word(w) => {
            if w.quote_style.is_some() {
                w.value
            } else {
                w.value.to_lowercase()
            }
        }
        _ => return p.expected("an option name", p.peek_token()),
    };
    let value = match &p.peek_token().token {
        Token::Comma | Token::RParen => CopyOptionValue::None,
        Token::Mul | Token::LParen => column_list(p)?,
        Token::SingleQuotedString(_) | Token::EscapedStringLiteral(_) | Token::NationalStringLiteral(_) => {
            CopyOptionValue::Str(p.parse_literal_string()?)
        }
        Token::Number(..) => {
            let Token::Number(n, _) = p.next_token().token else { unreachable!() };
            CopyOptionValue::Str(n)
        }
        Token::Minus => {
            p.next_token();
            match p.next_token().token {
                Token::Number(n, _) => CopyOptionValue::Str(format!("-{n}")),
                _ => return p.expected("a number", p.peek_token()),
            }
        }
        Token::Word(_) => {
            let Token::Word(w) = p.next_token().token else { unreachable!() };
            CopyOptionValue::Str(if w.quote_style.is_some() { w.value } else { w.value.to_lowercase() })
        }
        _ => return p.expected("an option argument", p.peek_token()),
    };
    Ok(CopyOption { name, value })
}

/// One legacy (`copy_opt_item`) option, if one comes next.
fn legacy_option(p: &mut Parser) -> Result<Option<CopyOption>, ParserError> {
    let Some(w) = word(p, 0) else { return Ok(None) };
    let string_arg = |p: &mut Parser, name: &str| -> Result<CopyOption, ParserError> {
        p.next_token();
        eat(p, "AS");
        Ok(opt(name, CopyOptionValue::Str(p.parse_literal_string()?)))
    };
    Ok(Some(match w.as_str() {
        "BINARY" | "CSV" => {
            p.next_token();
            opt("format", CopyOptionValue::Str(w.to_lowercase()))
        }
        "FREEZE" | "HEADER" => {
            p.next_token();
            opt(&w.to_lowercase(), CopyOptionValue::Str("true".into()))
        }
        "DELIMITER" | "NULL" | "QUOTE" | "ESCAPE" | "ENCODING" => string_arg(p, &w.to_lowercase())?,
        "FORCE" => {
            p.next_token();
            let name = if eat(p, "QUOTE") {
                "force_quote"
            } else if eat(p, "NOT") {
                if !eat(p, "NULL") {
                    return p.expected("NULL", p.peek_token());
                }
                "force_not_null"
            } else if eat(p, "NULL") {
                "force_null"
            } else {
                return p.expected("QUOTE, NOT NULL or NULL", p.peek_token());
            };
            let value = if p.consume_token(&Token::Mul) {
                CopyOptionValue::Star
            } else {
                let mut cols = vec![ident(&p.parse_identifier()?)];
                while p.consume_token(&Token::Comma) {
                    cols.push(ident(&p.parse_identifier()?));
                }
                CopyOptionValue::List(cols)
            };
            opt(name, value)
        }
        _ => return Ok(None),
    }))
}
