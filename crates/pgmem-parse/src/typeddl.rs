//! `CREATE DOMAIN`, `ALTER DOMAIN`, `DROP DOMAIN` and `ALTER TYPE`, parsed here because
//! sqlparser covers them only partly (no `NOT NULL` domain constraints, no `ALTER DOMAIN`, no
//! `ALTER TYPE ... ATTRIBUTE`). The statement structure follows Postgres' grammar (gram.y
//! `CreateDomainStmt`, `AlterDomainStmt`, `AlterEnumStmt`, `AlterCompositeTypeStmt`); the
//! pieces (names, data types, expressions) come from sqlparser's own parser and are lowered
//! with the shared lowering functions.

use crate::expr::{lower_expr, lower_type};
use crate::{ident, object_name};
use pgmem_ir::{AlterDomain, AlterDomainAction, AlterType, AlterTypeAction, CreateDomain, Drop, ObjectType, Statement};
use pgmem_types::SqlResult;
use sqlparser::ast as sq;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

/// A statement parsed here, with its sqlparser pieces not yet lowered.
pub(crate) enum TypeDdl {
    CreateDomain {
        name: sq::ObjectName,
        ty: sq::DataType,
        default: Option<sq::Expr>,
        not_null: bool,
        checks: Vec<(Option<sq::Ident>, sq::Expr)>,
        /// A grammar-level conflict Postgres reports after parsing (42601).
        conflict: Option<&'static str>,
    },
    AlterDomain {
        name: sq::ObjectName,
        action: DomainAction,
    },
    DropDomain {
        names: Vec<sq::ObjectName>,
        if_exists: bool,
        cascade: bool,
    },
    AlterType {
        name: sq::ObjectName,
        action: TypeAction,
    },
}

pub(crate) enum DomainAction {
    SetDefault(sq::Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    AddCheck { name: Option<sq::Ident>, expr: sq::Expr, not_valid: bool },
    AddNotNull,
    DropConstraint { name: sq::Ident, if_exists: bool },
    RenameConstraint { from: sq::Ident, to: sq::Ident },
    ValidateConstraint(sq::Ident),
    Rename(sq::Ident),
    NoOp,
}

pub(crate) enum TypeAction {
    AddValue { value: String, if_not_exists: bool, neighbor: Option<(bool, String)> },
    RenameValue { from: String, to: String },
    Rename(sq::Ident),
    AddAttribute { name: sq::Ident, ty: sq::DataType },
    DropAttribute { name: sq::Ident, if_exists: bool },
    RenameAttribute { from: sq::Ident, to: sq::Ident },
    NoOp,
}

fn is_word(p: &Parser, n: usize, kw: Keyword) -> bool {
    matches!(&p.peek_nth_token(n).token, Token::Word(w) if w.keyword == kw)
}

/// Consumes `kw ATTRIBUTE` (sqlparser has no ATTRIBUTE keyword).
fn parse_attribute_action(p: &mut Parser, kw: Keyword) -> bool {
    let attr = matches!(&p.peek_nth_token(1).token, Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("attribute"));
    if is_word(p, 0, kw) && attr {
        p.next_token();
        p.next_token();
        return true;
    }
    false
}

/// Parses one of this module's statements if the parser is positioned at one.
pub(crate) fn try_parse(p: &mut Parser) -> Result<Option<TypeDdl>, ParserError> {
    let domain = is_word(p, 1, Keyword::DOMAIN);
    let stmt = if is_word(p, 0, Keyword::CREATE) && domain {
        p.next_token();
        p.next_token();
        create_domain(p)?
    } else if is_word(p, 0, Keyword::ALTER) && domain {
        p.next_token();
        p.next_token();
        alter_domain(p)?
    } else if is_word(p, 0, Keyword::DROP) && domain {
        p.next_token();
        p.next_token();
        drop_domain(p)?
    } else if is_word(p, 0, Keyword::ALTER) && is_word(p, 1, Keyword::TYPE) {
        p.next_token();
        p.next_token();
        alter_type(p)?
    } else {
        return Ok(None);
    };
    Ok(Some(stmt))
}

fn create_domain(p: &mut Parser) -> Result<TypeDdl, ParserError> {
    let name = p.parse_object_name(false)?;
    let _ = p.parse_keyword(Keyword::AS);
    let ty = p.parse_data_type()?;
    let mut default = None;
    let mut not_null = false;
    let mut checks = Vec::new();
    let mut conflict = None;
    let (mut saw_null, mut saw_not_null) = (false, false);
    loop {
        if p.parse_keyword(Keyword::COLLATE) {
            p.parse_object_name(false)?;
            continue;
        }
        if p.parse_keyword(Keyword::DEFAULT) {
            if default.is_some() {
                conflict = conflict.or(Some("multiple default expressions"));
            }
            default = Some(p.parse_expr()?);
            continue;
        }
        let constraint_name = if p.parse_keyword(Keyword::CONSTRAINT) { Some(p.parse_identifier()?) } else { None };
        if p.parse_keywords(&[Keyword::NOT, Keyword::NULL]) {
            not_null = true;
            saw_not_null = true;
            if saw_null {
                conflict = conflict.or(Some("conflicting NULL/NOT NULL constraints"));
            }
        } else if p.parse_keyword(Keyword::NULL) {
            saw_null = true;
            if saw_not_null {
                conflict = conflict.or(Some("conflicting NULL/NOT NULL constraints"));
            }
        } else if p.parse_keyword(Keyword::CHECK) {
            p.expect_token(&Token::LParen)?;
            let expr = p.parse_expr()?;
            p.expect_token(&Token::RParen)?;
            checks.push((constraint_name, expr));
        } else if constraint_name.is_some() {
            return p.expected("NOT NULL, NULL or CHECK", p.peek_token());
        } else {
            break;
        }
    }
    Ok(TypeDdl::CreateDomain { name, ty, default, not_null, checks, conflict })
}

fn alter_domain(p: &mut Parser) -> Result<TypeDdl, ParserError> {
    let name = p.parse_object_name(false)?;
    let action = if p.parse_keywords(&[Keyword::SET, Keyword::DEFAULT]) {
        DomainAction::SetDefault(p.parse_expr()?)
    } else if p.parse_keywords(&[Keyword::DROP, Keyword::DEFAULT]) {
        DomainAction::DropDefault
    } else if p.parse_keywords(&[Keyword::SET, Keyword::NOT, Keyword::NULL]) {
        DomainAction::SetNotNull
    } else if p.parse_keywords(&[Keyword::DROP, Keyword::NOT, Keyword::NULL]) {
        DomainAction::DropNotNull
    } else if p.parse_keyword(Keyword::ADD) {
        let cname = if p.parse_keyword(Keyword::CONSTRAINT) { Some(p.parse_identifier()?) } else { None };
        if p.parse_keywords(&[Keyword::NOT, Keyword::NULL]) {
            DomainAction::AddNotNull
        } else {
            p.expect_keyword(Keyword::CHECK)?;
            p.expect_token(&Token::LParen)?;
            let expr = p.parse_expr()?;
            p.expect_token(&Token::RParen)?;
            let not_valid = p.parse_keywords(&[Keyword::NOT, Keyword::VALID]);
            DomainAction::AddCheck { name: cname, expr, not_valid }
        }
    } else if p.parse_keywords(&[Keyword::DROP, Keyword::CONSTRAINT]) {
        let if_exists = p.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
        let cname = p.parse_identifier()?;
        let _ = p.parse_one_of_keywords(&[Keyword::RESTRICT, Keyword::CASCADE]);
        DomainAction::DropConstraint { name: cname, if_exists }
    } else if p.parse_keywords(&[Keyword::RENAME, Keyword::CONSTRAINT]) {
        let from = p.parse_identifier()?;
        p.expect_keyword(Keyword::TO)?;
        DomainAction::RenameConstraint { from, to: p.parse_identifier()? }
    } else if p.parse_keywords(&[Keyword::VALIDATE, Keyword::CONSTRAINT]) {
        DomainAction::ValidateConstraint(p.parse_identifier()?)
    } else if p.parse_keywords(&[Keyword::RENAME, Keyword::TO]) {
        DomainAction::Rename(p.parse_identifier()?)
    } else if p.parse_keywords(&[Keyword::OWNER, Keyword::TO]) || p.parse_keywords(&[Keyword::SET, Keyword::SCHEMA]) {
        p.parse_identifier()?;
        DomainAction::NoOp
    } else {
        return p.expected("an ALTER DOMAIN action", p.peek_token());
    };
    Ok(TypeDdl::AlterDomain { name, action })
}

fn drop_domain(p: &mut Parser) -> Result<TypeDdl, ParserError> {
    let if_exists = p.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
    let names = p.parse_comma_separated(|p| p.parse_object_name(false))?;
    let cascade = matches!(p.parse_one_of_keywords(&[Keyword::RESTRICT, Keyword::CASCADE]), Some(Keyword::CASCADE));
    Ok(TypeDdl::DropDomain { names, if_exists, cascade })
}

fn alter_type(p: &mut Parser) -> Result<TypeDdl, ParserError> {
    let name = p.parse_object_name(false)?;
    let action = if p.parse_keywords(&[Keyword::ADD, Keyword::VALUE]) {
        let if_not_exists = p.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
        let value = p.parse_literal_string()?;
        let neighbor = if p.parse_keyword(Keyword::BEFORE) {
            Some((true, p.parse_literal_string()?))
        } else if p.parse_keyword(Keyword::AFTER) {
            Some((false, p.parse_literal_string()?))
        } else {
            None
        };
        TypeAction::AddValue { value, if_not_exists, neighbor }
    } else if p.parse_keywords(&[Keyword::RENAME, Keyword::VALUE]) {
        let from = p.parse_literal_string()?;
        p.expect_keyword(Keyword::TO)?;
        TypeAction::RenameValue { from, to: p.parse_literal_string()? }
    } else if p.parse_keywords(&[Keyword::RENAME, Keyword::TO]) {
        TypeAction::Rename(p.parse_identifier()?)
    } else if parse_attribute_action(p, Keyword::RENAME) {
        let from = p.parse_identifier()?;
        p.expect_keyword(Keyword::TO)?;
        let to = p.parse_identifier()?;
        let _ = p.parse_one_of_keywords(&[Keyword::RESTRICT, Keyword::CASCADE]);
        TypeAction::RenameAttribute { from, to }
    } else if parse_attribute_action(p, Keyword::ADD) {
        let attr = p.parse_identifier()?;
        let ty = p.parse_data_type()?;
        let _ = p.parse_one_of_keywords(&[Keyword::RESTRICT, Keyword::CASCADE]);
        TypeAction::AddAttribute { name: attr, ty }
    } else if parse_attribute_action(p, Keyword::DROP) {
        let if_exists = p.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
        let attr = p.parse_identifier()?;
        let _ = p.parse_one_of_keywords(&[Keyword::RESTRICT, Keyword::CASCADE]);
        TypeAction::DropAttribute { name: attr, if_exists }
    } else if p.parse_keywords(&[Keyword::OWNER, Keyword::TO]) || p.parse_keywords(&[Keyword::SET, Keyword::SCHEMA]) {
        p.parse_identifier()?;
        TypeAction::NoOp
    } else {
        return p.expected("an ALTER TYPE action", p.peek_token());
    };
    Ok(TypeDdl::AlterType { name, action })
}

pub(crate) fn lower(stmt: TypeDdl) -> SqlResult<Statement> {
    Ok(match stmt {
        TypeDdl::CreateDomain { conflict: Some(message), .. } => return Err(pgmem_types::SqlError::syntax(message)),
        TypeDdl::CreateDomain { name, ty, default, not_null, checks, conflict: None } => Statement::CreateDomain(Box::new(CreateDomain {
            name: object_name(&name)?,
            ty: lower_type(&ty)?,
            default: default.map(lower_expr).transpose()?,
            not_null,
            checks: checks.into_iter().map(|(n, e)| Ok((n.as_ref().map(ident), lower_expr(e)?))).collect::<SqlResult<Vec<_>>>()?,
        })),
        TypeDdl::AlterDomain { name, action } => {
            let action = match action {
                DomainAction::SetDefault(e) => AlterDomainAction::SetDefault(lower_expr(e)?),
                DomainAction::DropDefault => AlterDomainAction::DropDefault,
                DomainAction::SetNotNull | DomainAction::AddNotNull => AlterDomainAction::SetNotNull,
                DomainAction::DropNotNull => AlterDomainAction::DropNotNull,
                DomainAction::AddCheck { name, expr, not_valid } => {
                    AlterDomainAction::AddCheck { name: name.as_ref().map(ident), expr: lower_expr(expr)?, not_valid }
                }
                DomainAction::DropConstraint { name, if_exists } => AlterDomainAction::DropConstraint { name: ident(&name), if_exists },
                DomainAction::RenameConstraint { from, to } => AlterDomainAction::RenameConstraint { from: ident(&from), to: ident(&to) },
                DomainAction::ValidateConstraint(n) => AlterDomainAction::ValidateConstraint(ident(&n)),
                DomainAction::Rename(n) => AlterDomainAction::Rename(ident(&n)),
                DomainAction::NoOp => AlterDomainAction::NoOp,
            };
            Statement::AlterDomain(Box::new(AlterDomain { name: object_name(&name)?, action }))
        }
        TypeDdl::DropDomain { names, if_exists, cascade } => Statement::Drop(Box::new(Drop {
            object_type: ObjectType::Domain,
            names: names.iter().map(object_name).collect::<SqlResult<Vec<_>>>()?,
            if_exists,
            cascade,
        })),
        TypeDdl::AlterType { name, action } => {
            let action = match action {
                TypeAction::AddValue { value, if_not_exists, neighbor } => AlterTypeAction::AddValue { value, if_not_exists, neighbor },
                TypeAction::RenameValue { from, to } => AlterTypeAction::RenameValue { from, to },
                TypeAction::Rename(n) => AlterTypeAction::Rename(ident(&n)),
                TypeAction::AddAttribute { name, ty } => AlterTypeAction::AddAttribute { name: ident(&name), ty: lower_type(&ty)? },
                TypeAction::DropAttribute { name, if_exists } => AlterTypeAction::DropAttribute { name: ident(&name), if_exists },
                TypeAction::RenameAttribute { from, to } => AlterTypeAction::RenameAttribute { from: ident(&from), to: ident(&to) },
                TypeAction::NoOp => AlterTypeAction::NoOp,
            };
            Statement::AlterType(Box::new(AlterType { name: object_name(&name)?, action }))
        }
    })
}
