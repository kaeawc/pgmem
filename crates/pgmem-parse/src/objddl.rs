//! `CREATE / ALTER / DROP STATISTICS` and `CREATE / DROP CAST` (plus `COMMENT ON` them), parsed
//! here following Postgres' grammar (gram.y `CreateStatsStmt`, `AlterStatsStmt`, `CreateCastStmt`);
//! names, types and expressions come from sqlparser's parser.

use crate::expr::{lower_expr, lower_type};
use crate::{ident, object_name};
use pgmem_ir::{AlterStatisticsAction, CommentTarget, CreateCast, CreateStatistics, Expr, Statement, StatisticsKey, TypeName};
use pgmem_types::SqlResult;
use sqlparser::ast as sq;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

pub(crate) enum ObjDdl {
    CreateStatistics {
        name: Option<sq::ObjectName>,
        if_not_exists: bool,
        kinds: Vec<String>,
        keys: Vec<sq::Expr>,
        tables: Vec<sq::ObjectName>,
    },
    AlterStatistics {
        name: sq::ObjectName,
        if_exists: bool,
        action: RawAlter,
    },
    DropStatistics {
        names: Vec<sq::ObjectName>,
        if_exists: bool,
        cascade: bool,
    },
    CreateCast {
        source: sq::DataType,
        target: sq::DataType,
        function: Option<(sq::ObjectName, Vec<sq::DataType>)>,
        inout: bool,
        context: char,
    },
    DropCast {
        source: sq::DataType,
        target: sq::DataType,
        if_exists: bool,
        cascade: bool,
    },
    CommentStatistics {
        name: sq::ObjectName,
        text: Option<String>,
    },
    CommentCast {
        source: sq::DataType,
        target: sq::DataType,
        text: Option<String>,
    },
}

pub(crate) enum RawAlter {
    Rename(sq::Ident),
    SetSchema(sq::Ident),
    SetStatistics(i64),
    Owner(String),
}

fn word_at(p: &Parser, n: usize, w: &str) -> bool {
    matches!(&p.peek_nth_token(n).token, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w))
}

fn eat(p: &mut Parser, w: &str) -> bool {
    if word_at(p, 0, w) {
        p.next_token();
        true
    } else {
        false
    }
}

fn expect(p: &mut Parser, w: &str) -> Result<(), ParserError> {
    if eat(p, w) { Ok(()) } else { p.expected(w, p.peek_token()) }
}

fn string(p: &mut Parser) -> Result<String, ParserError> {
    match p.next_token().token {
        Token::SingleQuotedString(s) | Token::EscapedStringLiteral(s) => Ok(s),
        Token::DollarQuotedString(d) => Ok(d.value),
        other => p.expected("a string constant", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
    }
}

pub(crate) fn try_parse(p: &mut Parser) -> Result<Option<ObjDdl>, ParserError> {
    if word_at(p, 0, "CREATE") && word_at(p, 1, "STATISTICS") {
        p.next_token();
        p.next_token();
        return create_statistics(p).map(Some);
    }
    if word_at(p, 0, "CREATE") && word_at(p, 1, "CAST") {
        p.next_token();
        p.next_token();
        return create_cast(p).map(Some);
    }
    if word_at(p, 0, "ALTER") && word_at(p, 1, "STATISTICS") {
        p.next_token();
        p.next_token();
        return alter_statistics(p).map(Some);
    }
    if word_at(p, 0, "DROP") && word_at(p, 1, "STATISTICS") {
        p.next_token();
        p.next_token();
        let if_exists = eat(p, "IF") && eat(p, "EXISTS");
        let mut names = vec![p.parse_object_name(false)?];
        while p.consume_token(&Token::Comma) {
            names.push(p.parse_object_name(false)?);
        }
        let cascade = eat(p, "CASCADE");
        let _ = eat(p, "RESTRICT");
        return Ok(Some(ObjDdl::DropStatistics { names, if_exists, cascade }));
    }
    if word_at(p, 0, "DROP") && word_at(p, 1, "CAST") {
        p.next_token();
        p.next_token();
        let if_exists = eat(p, "IF") && eat(p, "EXISTS");
        let (source, target) = cast_signature(p)?;
        let cascade = eat(p, "CASCADE");
        let _ = eat(p, "RESTRICT");
        return Ok(Some(ObjDdl::DropCast { source, target, if_exists, cascade }));
    }
    if word_at(p, 0, "COMMENT") && word_at(p, 1, "ON") && word_at(p, 2, "STATISTICS") {
        for _ in 0..3 {
            p.next_token();
        }
        let name = p.parse_object_name(false)?;
        expect(p, "IS")?;
        let text = if eat(p, "NULL") { None } else { Some(string(p)?) };
        return Ok(Some(ObjDdl::CommentStatistics { name, text }));
    }
    if word_at(p, 0, "COMMENT") && word_at(p, 1, "ON") && word_at(p, 2, "CAST") {
        for _ in 0..3 {
            p.next_token();
        }
        let (source, target) = cast_signature(p)?;
        expect(p, "IS")?;
        let text = if eat(p, "NULL") { None } else { Some(string(p)?) };
        return Ok(Some(ObjDdl::CommentCast { source, target, text }));
    }
    Ok(None)
}

/// `(source AS target)`.
fn cast_signature(p: &mut Parser) -> Result<(sq::DataType, sq::DataType), ParserError> {
    p.expect_token(&Token::LParen)?;
    let source = p.parse_data_type()?;
    expect(p, "AS")?;
    let target = p.parse_data_type()?;
    p.expect_token(&Token::RParen)?;
    Ok((source, target))
}

fn create_statistics(p: &mut Parser) -> Result<ObjDdl, ParserError> {
    let if_not_exists = word_at(p, 0, "IF") && word_at(p, 1, "NOT") && word_at(p, 2, "EXISTS");
    if if_not_exists {
        for _ in 0..3 {
            p.next_token();
        }
    }
    let name = if word_at(p, 0, "ON") || p.peek_token().token == Token::LParen { None } else { Some(p.parse_object_name(false)?) };
    let mut kinds = Vec::new();
    if p.consume_token(&Token::LParen) {
        loop {
            kinds.push(p.parse_identifier()?.value);
            if !p.consume_token(&Token::Comma) {
                break;
            }
        }
        p.expect_token(&Token::RParen)?;
    }
    expect(p, "ON")?;
    let mut keys = Vec::new();
    loop {
        keys.push(p.parse_expr()?);
        if !p.consume_token(&Token::Comma) {
            break;
        }
    }
    expect(p, "FROM")?;
    let mut tables = Vec::new();
    loop {
        let _ = eat(p, "ONLY");
        tables.push(p.parse_object_name(false)?);
        let _ = p.consume_token(&Token::Mul);
        // An alias is accepted and ignored.
        if eat(p, "AS") {
            p.parse_identifier()?;
        } else if matches!(&p.peek_token().token, Token::Word(w) if w.quote_style.is_some()
            || !matches!(w.keyword, sqlparser::keywords::Keyword::NoKeyword))
        {
            // A keyword (e.g. TABLESAMPLE, WHERE, JOIN): left for the caller to reject.
        } else if matches!(&p.peek_token().token, Token::Word(_)) {
            p.parse_identifier()?;
        }
        if !p.consume_token(&Token::Comma) {
            break;
        }
    }
    Ok(ObjDdl::CreateStatistics { name, if_not_exists, kinds, keys, tables })
}

fn alter_statistics(p: &mut Parser) -> Result<ObjDdl, ParserError> {
    let if_exists = eat(p, "IF") && eat(p, "EXISTS");
    let name = p.parse_object_name(false)?;
    let action = if if_exists || word_at(p, 0, "SET") && word_at(p, 1, "STATISTICS") {
        expect(p, "SET")?;
        expect(p, "STATISTICS")?;
        let neg = p.consume_token(&Token::Minus);
        let n = p.parse_literal_uint()?;
        let n = i64::try_from(n).unwrap_or(i64::MAX);
        RawAlter::SetStatistics(if neg { -n } else { n })
    } else if eat(p, "RENAME") {
        expect(p, "TO")?;
        RawAlter::Rename(p.parse_identifier()?)
    } else if eat(p, "SET") {
        expect(p, "SCHEMA")?;
        RawAlter::SetSchema(p.parse_identifier()?)
    } else if eat(p, "OWNER") {
        expect(p, "TO")?;
        let owner = if eat(p, "CURRENT_USER") || eat(p, "CURRENT_ROLE") {
            "current_user".to_owned()
        } else if eat(p, "SESSION_USER") {
            "session_user".to_owned()
        } else {
            p.parse_identifier()?.value
        };
        RawAlter::Owner(owner)
    } else {
        return p.expected("RENAME, SET or OWNER", p.peek_token());
    };
    Ok(ObjDdl::AlterStatistics { name, if_exists, action })
}

fn create_cast(p: &mut Parser) -> Result<ObjDdl, ParserError> {
    let (source, target) = cast_signature(p)?;
    let mut function = None;
    let mut inout = false;
    if eat(p, "WITHOUT") {
        expect(p, "FUNCTION")?;
    } else {
        expect(p, "WITH")?;
        if eat(p, "INOUT") {
            inout = true;
        } else {
            expect(p, "FUNCTION")?;
            let name = p.parse_object_name(false)?;
            p.expect_token(&Token::LParen)?;
            let mut args = Vec::new();
            if !p.consume_token(&Token::RParen) {
                loop {
                    args.push(p.parse_data_type()?);
                    if p.consume_token(&Token::RParen) {
                        break;
                    }
                    p.expect_token(&Token::Comma)?;
                }
            }
            function = Some((name, args));
        }
    }
    let context = if word_at(p, 0, "AS") {
        p.next_token();
        if eat(p, "IMPLICIT") {
            'i'
        } else if eat(p, "ASSIGNMENT") {
            'a'
        } else {
            return p.expected("IMPLICIT or ASSIGNMENT", p.peek_token());
        }
    } else {
        'e'
    };
    Ok(ObjDdl::CreateCast { source, target, function, inout, context })
}

fn ty(t: &sq::DataType) -> SqlResult<TypeName> {
    lower_type(t)
}

pub(crate) fn lower(d: ObjDdl) -> SqlResult<Statement> {
    Ok(match d {
        ObjDdl::CreateStatistics { name, if_not_exists, kinds, keys, tables } => {
            let keys = keys
                .into_iter()
                .map(|e| {
                    Ok(match lower_expr(e)? {
                        Expr::Column(parts) if parts.len() == 1 => StatisticsKey::Column(parts[0].clone()),
                        other => StatisticsKey::Expr(other),
                    })
                })
                .collect::<SqlResult<Vec<_>>>()?;
            Statement::CreateStatistics(Box::new(CreateStatistics {
                name: name.map(|n| object_name(&n)).transpose()?,
                if_not_exists,
                kinds: kinds.iter().map(|k| k.to_ascii_lowercase()).collect(),
                keys,
                tables: tables.iter().map(object_name).collect::<SqlResult<Vec<_>>>()?,
            }))
        }
        ObjDdl::AlterStatistics { name, if_exists, action } => Statement::AlterStatistics {
            name: object_name(&name)?,
            if_exists,
            action: match action {
                RawAlter::Rename(n) => AlterStatisticsAction::RenameTo(ident(&n)),
                RawAlter::SetSchema(n) => AlterStatisticsAction::SetSchema(ident(&n)),
                RawAlter::SetStatistics(n) => AlterStatisticsAction::SetStatistics(n),
                RawAlter::Owner(o) => AlterStatisticsAction::OwnerTo(o),
            },
        },
        ObjDdl::DropStatistics { names, if_exists, cascade } => {
            Statement::DropStatistics { names: names.iter().map(object_name).collect::<SqlResult<Vec<_>>>()?, if_exists, cascade }
        }
        ObjDdl::CreateCast { source, target, function, inout, context } => Statement::CreateCast(Box::new(CreateCast {
            source: ty(&source)?,
            target: ty(&target)?,
            function: function.map(|(n, args)| Ok((object_name(&n)?, args.iter().map(ty).collect::<SqlResult<Vec<_>>>()?))).transpose()?,
            inout,
            context,
        })),
        ObjDdl::DropCast { source, target, if_exists, cascade } => {
            Statement::DropCast { source: ty(&source)?, target: ty(&target)?, if_exists, cascade }
        }
        ObjDdl::CommentStatistics { name, text } => {
            Statement::Comment { target: CommentTarget::Statistics, name: object_name(&name)?, text }
        }
        ObjDdl::CommentCast { source, target, text } => {
            Statement::Comment { target: CommentTarget::Cast(ty(&source)?, ty(&target)?), name: pgmem_ir::ObjectName(Vec::new()), text }
        }
    })
}
