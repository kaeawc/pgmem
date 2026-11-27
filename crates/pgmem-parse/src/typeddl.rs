//! `CREATE DOMAIN`, `ALTER DOMAIN`, `DROP DOMAIN` and `ALTER TYPE`, parsed here because
//! sqlparser covers them only partly (no `NOT NULL` domain constraints, no `ALTER DOMAIN`, no
//! `ALTER TYPE ... ATTRIBUTE`). The statement structure follows Postgres' grammar (gram.y
//! `CreateDomainStmt`, `AlterDomainStmt`, `AlterEnumStmt`, `AlterCompositeTypeStmt`); the
//! pieces (names, data types, expressions) come from sqlparser's own parser and are lowered
//! with the shared lowering functions.

use crate::expr::{lower_expr, lower_type};
use crate::{ident, object_name};
use pgmem_ir::{
    AlterDomain, AlterDomainAction, AlterType, AlterTypeAction, CreateDomain, Drop, ObjectType, Statement, TypeAttributeAction,
};
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
        collation: Option<sq::ObjectName>,
        not_null: bool,
        checks: Vec<(Option<sq::Ident>, sq::Expr)>,
        /// A grammar-level conflict Postgres reports after parsing (42601).
        conflict: Option<(&'static str, sqlparser::tokenizer::Location)>,
        /// Where the base type starts.
        ty_at: sqlparser::tokenizer::Location,
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
    /// `ALTER {TABLE|VIEW|SEQUENCE|TYPE|DOMAIN} [IF EXISTS] name SET SCHEMA new_schema`.
    SetSchema {
        object_type: pgmem_ir::ObjectType,
        name: sq::ObjectName,
        if_exists: bool,
        schema: sq::Ident,
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
    Attributes(Vec<AttributeAction>),
    RenameAttribute { from: sq::Ident, to: sq::Ident },
    NoOp,
}

pub(crate) enum AttributeAction {
    Add { name: sq::Ident, ty: sq::DataType, collation: Option<sq::ObjectName> },
    Drop { name: sq::Ident, if_exists: bool, cascade: bool },
    AlterType { name: sq::Ident, ty: sq::DataType, collation: Option<sq::ObjectName> },
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

/// Whether the next tokens start an `ADD | DROP | ALTER ATTRIBUTE` action.
fn is_attribute_action(p: &Parser) -> bool {
    [Keyword::ADD, Keyword::DROP, Keyword::ALTER].iter().any(|kw| {
        is_word(p, 0, *kw)
            && matches!(&p.peek_nth_token(1).token, Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("attribute"))
    })
}

fn attribute_action(p: &mut Parser) -> Result<AttributeAction, ParserError> {
    fn collate(p: &mut Parser) -> Result<Option<sq::ObjectName>, ParserError> {
        if p.parse_keyword(Keyword::COLLATE) { p.parse_object_name(false).map(Some) } else { Ok(None) }
    }
    fn cascade(p: &mut Parser) -> bool {
        matches!(p.parse_one_of_keywords(&[Keyword::RESTRICT, Keyword::CASCADE]), Some(Keyword::CASCADE))
    }
    if parse_attribute_action(p, Keyword::ADD) {
        let name = p.parse_identifier()?;
        let ty = p.parse_data_type()?;
        let collation = collate(p)?;
        cascade(p);
        Ok(AttributeAction::Add { name, ty, collation })
    } else if parse_attribute_action(p, Keyword::DROP) {
        let if_exists = p.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
        let name = p.parse_identifier()?;
        Ok(AttributeAction::Drop { name, if_exists, cascade: cascade(p) })
    } else {
        parse_attribute_action(p, Keyword::ALTER);
        let name = p.parse_identifier()?;
        let _ = p.parse_keywords(&[Keyword::SET, Keyword::DATA]);
        p.expect_keyword(Keyword::TYPE)?;
        let ty = p.parse_data_type()?;
        let collation = collate(p)?;
        cascade(p);
        Ok(AttributeAction::AlterType { name, ty, collation })
    }
}

/// Parses one of this module's statements if the parser is positioned at one.
/// Whether the statement ahead is `ALTER <kind> [IF EXISTS] [ONLY] name SET SCHEMA ...`;
/// returns the object type and the number of tokens before the name.
fn set_schema_ahead(p: &Parser) -> Option<(pgmem_ir::ObjectType, usize)> {
    use pgmem_ir::ObjectType as O;
    if !is_word(p, 0, Keyword::ALTER) {
        return None;
    }
    let (kind, mut i) = match &p.peek_nth_token(1).token {
        Token::Word(w) if w.keyword == Keyword::TABLE => (O::Table, 2),
        Token::Word(w) if w.keyword == Keyword::VIEW => (O::View, 2),
        Token::Word(w) if w.keyword == Keyword::SEQUENCE => (O::Sequence, 2),
        Token::Word(w) if w.keyword == Keyword::TYPE => (O::Type, 2),
        Token::Word(w) if w.keyword == Keyword::DOMAIN => (O::Domain, 2),
        Token::Word(w) if w.keyword == Keyword::MATERIALIZED && is_word(p, 2, Keyword::VIEW) => (O::View, 3),
        _ => return None,
    };
    let start = i;
    if is_word(p, i, Keyword::IF) && is_word(p, i + 1, Keyword::EXISTS) {
        i += 2;
    }
    if is_word(p, i, Keyword::ONLY) {
        i += 1;
    }
    // name [. name [. name]]
    loop {
        if !matches!(&p.peek_nth_token(i).token, Token::Word(_)) {
            return None;
        }
        i += 1;
        if p.peek_nth_token(i).token == Token::Period {
            i += 1;
        } else {
            break;
        }
    }
    (is_word(p, i, Keyword::SET) && is_word(p, i + 1, Keyword::SCHEMA)).then_some((kind, start))
}

pub(crate) fn try_parse(p: &mut Parser) -> Result<Option<TypeDdl>, ParserError> {
    if let Some((object_type, skip)) = set_schema_ahead(p) {
        for _ in 0..skip {
            p.next_token();
        }
        let if_exists = p.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
        let _ = p.parse_keyword(Keyword::ONLY);
        let name = p.parse_object_name(false)?;
        p.expect_keywords(&[Keyword::SET, Keyword::SCHEMA])?;
        let schema = p.parse_identifier()?;
        return Ok(Some(TypeDdl::SetSchema { object_type, name, if_exists, schema }));
    }
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
    let ty_at = p.peek_token().span.start;
    let ty = p.parse_data_type()?;
    let mut default = None;
    let mut collation = None;
    let mut not_null = false;
    let mut checks = Vec::new();
    let mut conflict = None;
    let (mut saw_null, mut saw_not_null) = (false, false);
    loop {
        let at = p.peek_token().span.start;
        if p.parse_keyword(Keyword::COLLATE) {
            if collation.is_some() {
                conflict = conflict.or(Some(("multiple COLLATE clauses not allowed", at)));
            }
            collation = Some(p.parse_object_name(false)?);
            continue;
        }
        if is_word(p, 0, Keyword::DEFAULT) {
            if default.is_some() {
                conflict = conflict.or(Some(("multiple default expressions", at)));
            }
            default = Some(default_expr(p)?);
            continue;
        }
        let constraint_name = if p.parse_keyword(Keyword::CONSTRAINT) { Some(p.parse_identifier()?) } else { None };
        let at = p.peek_token().span.start;
        if p.parse_keywords(&[Keyword::NOT, Keyword::NULL]) {
            not_null = true;
            saw_not_null = true;
            if saw_null {
                conflict = conflict.or(Some(("conflicting NULL/NOT NULL constraints", at)));
            }
        } else if p.parse_keyword(Keyword::NULL) {
            saw_null = true;
            if saw_not_null {
                conflict = conflict.or(Some(("conflicting NULL/NOT NULL constraints", at)));
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
    Ok(TypeDdl::CreateDomain { name, ty, default, collation, not_null, checks, conflict, ty_at })
}

/// `DEFAULT b_expr`: the expression stops before a following `NOT NULL`, `NULL` or `COLLATE`
/// (Postgres' grammar uses `b_expr` there, so `DEFAULT 1 NOT NULL` is a default and a
/// constraint, not `1 IS NOT NULL`). sqlparser parses column options that way; the parser is
/// positioned at `DEFAULT`.
fn default_expr(p: &mut Parser) -> Result<sq::Expr, ParserError> {
    match p.parse_optional_column_option()? {
        Some(sq::ColumnOption::Default(e)) => Ok(e),
        _ => p.expected("a DEFAULT expression", p.peek_token()),
    }
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
    } else if is_attribute_action(p) {
        // gram.y `alter_type_cmds`: comma-separated, each with its own RESTRICT / CASCADE.
        let mut actions = Vec::new();
        loop {
            actions.push(attribute_action(p)?);
            if !p.consume_token(&Token::Comma) {
                break;
            }
        }
        TypeAction::Attributes(actions)
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
        TypeDdl::SetSchema { object_type, name, if_exists, schema } => {
            Statement::SetSchema { object_type, name: object_name(&name)?, if_exists, schema: ident(&schema) }
        }
        TypeDdl::CreateDomain { conflict: Some((message, at)), .. } => {
            let e = pgmem_types::SqlError::syntax(message);
            return Err(match crate::position_in_parsed(at) {
                Some(p) => e.with_position(p),
                None => e,
            });
        }
        TypeDdl::CreateDomain { name, ty, default, collation, not_null, checks, conflict: None, ty_at } => {
            let ty = lower_type(&ty)?;
            // The base type, for errors about the domain as a whole (`COLLATE` on a type
            // without collations).
            crate::note_ref(pgmem_types::ErrorRef::Type(ty.schema.iter().chain([&ty.name]).cloned().collect::<Vec<_>>().join(".")), ty_at);
            Statement::CreateDomain(Box::new(CreateDomain {
                name: object_name(&name)?,
                ty,
                default: default.map(lower_expr).transpose()?,
                collation: collation.as_ref().map(|c| object_name(c).map(|o| o.base().to_owned())).transpose()?,
                not_null,
                checks: checks
                    .into_iter()
                    .map(|(n, e)| Ok((n.as_ref().map(ident), crate::quietly(crate::QUIET_EXPRS, || lower_expr(e))?)))
                    .collect::<SqlResult<Vec<_>>>()?,
            }))
        }
        TypeDdl::AlterDomain { name, action } => {
            let action = match action {
                DomainAction::SetDefault(e) => AlterDomainAction::SetDefault(crate::quietly(crate::QUIET_EXPRS, || lower_expr(e))?),
                DomainAction::DropDefault => AlterDomainAction::DropDefault,
                DomainAction::SetNotNull | DomainAction::AddNotNull => AlterDomainAction::SetNotNull,
                DomainAction::DropNotNull => AlterDomainAction::DropNotNull,
                DomainAction::AddCheck { name, expr, not_valid } => AlterDomainAction::AddCheck {
                    name: name.as_ref().map(ident),
                    expr: crate::quietly(crate::QUIET_EXPRS, || lower_expr(expr))?,
                    not_valid,
                },
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
            concurrently: false,
        })),
        TypeDdl::AlterType { name, action } => {
            let action = match action {
                TypeAction::AddValue { value, if_not_exists, neighbor } => AlterTypeAction::AddValue { value, if_not_exists, neighbor },
                TypeAction::RenameValue { from, to } => AlterTypeAction::RenameValue { from, to },
                TypeAction::Rename(n) => AlterTypeAction::Rename(ident(&n)),
                TypeAction::Attributes(actions) => AlterTypeAction::Attributes(
                    actions
                        .into_iter()
                        .map(|a| {
                            let collation = |c: Option<sq::ObjectName>| c.map(|c| object_name(&c).map(|o| o.base().to_owned())).transpose();
                            Ok(match a {
                                AttributeAction::Add { name, ty, collation: c } => {
                                    TypeAttributeAction::Add { name: ident(&name), ty: lower_type(&ty)?, collation: collation(c)? }
                                }
                                AttributeAction::Drop { name, if_exists, cascade } => {
                                    TypeAttributeAction::Drop { name: ident(&name), if_exists, cascade }
                                }
                                AttributeAction::AlterType { name, ty, collation: c } => {
                                    TypeAttributeAction::AlterType { name: ident(&name), ty: lower_type(&ty)?, collation: collation(c)? }
                                }
                            })
                        })
                        .collect::<SqlResult<Vec<_>>>()?,
                ),
                TypeAction::RenameAttribute { from, to } => AlterTypeAction::RenameAttribute { from: ident(&from), to: ident(&to) },
                TypeAction::NoOp => AlterTypeAction::NoOp,
            };
            Statement::AlterType(Box::new(AlterType { name: object_name(&name)?, action }))
        }
    })
}
