//! `ALTER EXTENSION name {UPDATE [TO version] | SET SCHEMA schema | ADD member | DROP member}`
//! (gram.y `AlterExtensionStmt`, `AlterExtensionContentsStmt`), which sqlparser does not model.

use crate::expr::lower_type;
use crate::object_name;
use pgmem_ir::{AlterExtensionAction, ExtensionMember, FunctionArgMode, RoutineKind, Statement};
use pgmem_types::SqlResult;
use sqlparser::ast as sq;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

fn word_at(p: &Parser, n: usize, w: &str) -> bool {
    matches!(&p.peek_nth_token(n).token, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w))
}

fn eat(p: &mut Parser, w: &str) -> bool {
    let found = word_at(p, 0, w);
    if found {
        p.next_token();
    }
    found
}

fn expect(p: &mut Parser, w: &str) -> Result<(), ParserError> {
    if eat(p, w) { Ok(()) } else { p.expected(w, p.peek_token()) }
}

/// A statement parsed here (types still sqlparser values).
pub(crate) struct ExtDdl {
    name: String,
    action: RawAction,
}

enum RawAction {
    Update(Option<String>),
    SetSchema(String),
    Member { add: bool, member: RawMember },
}

enum RawMember {
    Plain(fn(pgmem_ir::ObjectName) -> ExtensionMember, sq::ObjectName),
    Function { name: sq::ObjectName, args: Option<Vec<sq::DataType>>, kind: RoutineKind },
    Schema(String),
}

pub(crate) fn try_parse(p: &mut Parser) -> Result<Option<ExtDdl>, ParserError> {
    if !(word_at(p, 0, "ALTER") && word_at(p, 1, "EXTENSION")) {
        return Ok(None);
    }
    p.next_token();
    p.next_token();
    let name = crate::ident(&p.parse_identifier()?);
    let action = if eat(p, "UPDATE") {
        let version = if eat(p, "TO") { Some(version_string(p)?) } else { None };
        RawAction::Update(version)
    } else if eat(p, "SET") {
        expect(p, "SCHEMA")?;
        RawAction::SetSchema(crate::ident(&p.parse_identifier()?))
    } else if word_at(p, 0, "ADD") || word_at(p, 0, "DROP") {
        let add = word_at(p, 0, "ADD");
        p.next_token();
        RawAction::Member { add, member: member(p)? }
    } else {
        return p.expected("UPDATE, SET, ADD or DROP", p.peek_token());
    };
    Ok(Some(ExtDdl { name, action }))
}

/// `NonReservedWord_or_Sconst`: an identifier or a string.
fn version_string(p: &mut Parser) -> Result<String, ParserError> {
    match p.next_token().token {
        Token::SingleQuotedString(s) => Ok(s),
        Token::Word(w) => Ok(w.value),
        other => p.expected("a version", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
    }
}

fn member(p: &mut Parser) -> Result<RawMember, ParserError> {
    let plain = |p: &mut Parser, make: fn(pgmem_ir::ObjectName) -> ExtensionMember| Ok(RawMember::Plain(make, p.parse_object_name(false)?));
    if eat(p, "TABLE") {
        return plain(p, ExtensionMember::Table);
    }
    if eat(p, "VIEW") {
        return plain(p, ExtensionMember::View);
    }
    if eat(p, "MATERIALIZED") {
        expect(p, "VIEW")?;
        return plain(p, ExtensionMember::MaterializedView);
    }
    if eat(p, "SEQUENCE") {
        return plain(p, ExtensionMember::Sequence);
    }
    if eat(p, "TYPE") {
        return plain(p, ExtensionMember::Type);
    }
    if eat(p, "DOMAIN") {
        return plain(p, ExtensionMember::Domain);
    }
    if eat(p, "SCHEMA") {
        return Ok(RawMember::Schema(crate::ident(&p.parse_identifier()?)));
    }
    let kind = if eat(p, "FUNCTION") {
        RoutineKind::Function
    } else if eat(p, "PROCEDURE") {
        RoutineKind::Procedure
    } else if eat(p, "ROUTINE") {
        RoutineKind::Routine
    } else {
        return p.expected("an object type", p.peek_token());
    };
    let name = p.parse_object_name(false)?;
    let args = if p.consume_token(&Token::LParen) {
        let mut types = Vec::new();
        if !p.consume_token(&Token::RParen) {
            loop {
                let (mode, _, ty, _) = crate::funcddl::function_arg(p)?;
                // OUT arguments are not part of the signature.
                if mode != FunctionArgMode::Out {
                    types.push(ty);
                }
                if p.consume_token(&Token::RParen) {
                    break;
                }
                p.expect_token(&Token::Comma)?;
            }
        }
        Some(types)
    } else {
        None
    };
    Ok(RawMember::Function { name, args, kind })
}

pub(crate) fn lower(e: ExtDdl) -> SqlResult<Statement> {
    let action = match e.action {
        RawAction::Update(version) => AlterExtensionAction::Update { version },
        RawAction::SetSchema(s) => AlterExtensionAction::SetSchema(s),
        RawAction::Member { add, member } => {
            let member = match member {
                RawMember::Plain(make, name) => make(object_name(&name)?),
                RawMember::Schema(s) => ExtensionMember::Schema(s),
                RawMember::Function { name, args, kind } => ExtensionMember::Function {
                    name: object_name(&name)?,
                    args: args
                        .map(|ts| crate::quietly(crate::QUIET_TYPES, || ts.iter().map(lower_type).collect::<SqlResult<Vec<_>>>()))
                        .transpose()?,
                    kind,
                },
            };
            AlterExtensionAction::Member { add, member }
        }
    };
    Ok(Statement::AlterExtension { name: e.name, action })
}
