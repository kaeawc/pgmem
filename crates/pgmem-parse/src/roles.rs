//! Role and privilege statements (gram.y `CreateRoleStmt`, `AlterRoleStmt`, `GrantStmt`,
//! `GrantRoleStmt`, `AlterDefaultPrivilegesStmt`, `DropOwnedStmt`, `ReassignOwnedStmt`,
//! `AlterOwnerStmt`), parsed here: sqlparser models them only partly.

use crate::expr::lower_type;
use crate::object_name;
use pgmem_ir::{AlterRoleAction, GrantStmt, GrantTarget, OwnedObject, RoleOptions, Statement, TypeName};
use pgmem_types::SqlResult;
use sqlparser::ast as sq;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

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

/// A role name: an identifier, or CURRENT_USER / SESSION_USER / CURRENT_ROLE / PUBLIC
/// (returned in lowercase, unquoted).
fn role_name(p: &mut Parser) -> Result<String, ParserError> {
    match p.next_token().token {
        Token::Word(w) if w.quote_style.is_some() => Ok(w.value),
        Token::Word(w) => Ok(w.value.to_lowercase()),
        Token::SingleQuotedString(s) => Ok(s),
        other => p.expected("a role name", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
    }
}

fn role_list(p: &mut Parser) -> Result<Vec<String>, ParserError> {
    let mut out = vec![role_name(p)?];
    while p.consume_token(&Token::Comma) {
        out.push(role_name(p)?);
    }
    Ok(out)
}

fn ident_list(p: &mut Parser) -> Result<Vec<String>, ParserError> {
    let mut out = vec![crate::ident(&p.parse_identifier()?)];
    while p.consume_token(&Token::Comma) {
        out.push(crate::ident(&p.parse_identifier()?));
    }
    Ok(out)
}

fn string(p: &mut Parser) -> Result<String, ParserError> {
    match p.next_token().token {
        Token::SingleQuotedString(s) | Token::EscapedStringLiteral(s) => Ok(s),
        other => p.expected("a string constant", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
    }
}

/// A statement parsed here; types still sqlparser types.
pub(crate) enum RoleStmt {
    Lowered(Statement),
    GrantFunctions { stmt: GrantStmt, functions: FunctionRefs },
}

/// Role options (`OptRoleElem`) until the statement ends.
fn role_options(p: &mut Parser) -> Result<RoleOptions, ParserError> {
    let mut o = RoleOptions::default();
    let _ = eat(p, "WITH");
    loop {
        let flag = |p: &mut Parser, on: &str, off: &str| -> Option<bool> {
            if eat(p, on) {
                Some(true)
            } else if eat(p, off) {
                Some(false)
            } else {
                None
            }
        };
        if let Some(v) = flag(p, "SUPERUSER", "NOSUPERUSER") {
            o.superuser = Some(v);
        } else if let Some(v) = flag(p, "CREATEDB", "NOCREATEDB") {
            o.createdb = Some(v);
        } else if let Some(v) = flag(p, "CREATEROLE", "NOCREATEROLE") {
            o.createrole = Some(v);
        } else if let Some(v) = flag(p, "INHERIT", "NOINHERIT") {
            o.inherit = Some(v);
        } else if let Some(v) = flag(p, "LOGIN", "NOLOGIN") {
            o.login = Some(v);
        } else if let Some(v) = flag(p, "REPLICATION", "NOREPLICATION") {
            o.replication = Some(v);
        } else if let Some(v) = flag(p, "BYPASSRLS", "NOBYPASSRLS") {
            o.bypassrls = Some(v);
        } else if eat(p, "CONNECTION") {
            expect(p, "LIMIT")?;
            let neg = p.consume_token(&Token::Minus);
            let n = match p.next_token().token {
                Token::Number(n, _) => n.parse::<i32>().unwrap_or(-1),
                other => return p.expected("a number", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
            };
            o.connlimit = Some(if neg { -n } else { n });
        } else if eat(p, "ENCRYPTED") || eat(p, "UNENCRYPTED") || word_at(p, 0, "PASSWORD") {
            expect(p, "PASSWORD")?;
            o.password = Some(if eat(p, "NULL") { None } else { Some(string(p)?) });
        } else if eat(p, "VALID") {
            expect(p, "UNTIL")?;
            o.valid_until = Some(string(p)?);
        } else if word_at(p, 0, "IN") && (word_at(p, 1, "ROLE") || word_at(p, 1, "GROUP")) {
            p.next_token();
            p.next_token();
            o.in_roles.extend(role_list(p)?);
        } else if eat(p, "ROLE") || eat(p, "USER") {
            o.members.extend(role_list(p)?);
        } else if eat(p, "ADMIN") {
            o.admins.extend(role_list(p)?);
        } else if eat(p, "SYSID") {
            p.next_token();
        } else {
            return Ok(o);
        }
    }
}

/// Privilege keywords up to ON (None: ALL [PRIVILEGES]) and their column lists (one per
/// privilege; empty when none has a list).
type Privileges = (Option<Vec<String>>, Vec<Vec<String>>);

fn privileges(p: &mut Parser) -> Result<Privileges, ParserError> {
    if eat(p, "ALL") {
        let _ = eat(p, "PRIVILEGES");
        let cols = column_list(p)?;
        return Ok((None, if cols.is_empty() { Vec::new() } else { vec![cols] }));
    }
    let mut out = Vec::new();
    let mut columns = Vec::new();
    loop {
        let w = match p.next_token().token {
            Token::Word(w) => w.value.to_lowercase(),
            other => return p.expected("a privilege", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
        };
        out.push(if w == "temp" { "temporary".to_owned() } else { w });
        columns.push(column_list(p)?);
        if !p.consume_token(&Token::Comma) {
            break;
        }
    }
    if columns.iter().all(Vec::is_empty) {
        columns.clear();
    }
    Ok((Some(out), columns))
}

/// An optional `(col, ...)` after a privilege.
fn column_list(p: &mut Parser) -> Result<Vec<String>, ParserError> {
    if !p.consume_token(&Token::LParen) {
        return Ok(Vec::new());
    }
    let out = ident_list(p)?;
    p.expect_token(&Token::RParen)?;
    Ok(out)
}

fn name_list(p: &mut Parser) -> Result<Vec<sq::ObjectName>, ParserError> {
    let mut out = vec![p.parse_object_name(false)?];
    while p.consume_token(&Token::Comma) {
        out.push(p.parse_object_name(false)?);
    }
    Ok(out)
}

/// Functions a GRANT names, with their argument types (lowered later).
type FunctionRefs = Vec<(sq::ObjectName, Option<Vec<sq::DataType>>)>;

/// `ON ...` targets; function signatures are returned for lowering.
fn target(p: &mut Parser) -> Result<(GrantTarget, FunctionRefs), ParserError> {
    let lower = |ns: Vec<sq::ObjectName>| -> Result<Vec<pgmem_ir::ObjectName>, ParserError> {
        ns.iter().map(|n| object_name(n).map_err(|e| ParserError::ParserError(e.message))).collect()
    };
    if eat(p, "ALL") {
        let kind = match p.next_token().token {
            Token::Word(w) => w.value.to_lowercase(),
            other => return p.expected("TABLES", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
        };
        expect(p, "IN")?;
        expect(p, "SCHEMA")?;
        let schemas = ident_list(p)?;
        return Ok((
            match kind.as_str() {
                "tables" => GrantTarget::AllTablesInSchema(schemas),
                "sequences" => GrantTarget::AllSequencesInSchema(schemas),
                "functions" | "procedures" | "routines" => GrantTarget::AllFunctionsInSchema(schemas),
                _ => GrantTarget::Other,
            },
            Vec::new(),
        ));
    }
    if eat(p, "SCHEMA") {
        return Ok((GrantTarget::Schemas(ident_list(p)?), Vec::new()));
    }
    if eat(p, "DATABASE") {
        return Ok((GrantTarget::Databases(ident_list(p)?), Vec::new()));
    }
    if eat(p, "SEQUENCE") {
        return Ok((GrantTarget::Sequences(lower(name_list(p)?)?), Vec::new()));
    }
    if eat(p, "TYPE") || eat(p, "DOMAIN") {
        return Ok((GrantTarget::Types(lower(name_list(p)?)?), Vec::new()));
    }
    if eat(p, "FUNCTION") || eat(p, "PROCEDURE") || eat(p, "ROUTINE") {
        let mut fns = Vec::new();
        loop {
            let name = p.parse_object_name(false)?;
            let args = if p.consume_token(&Token::LParen) {
                let mut ts = Vec::new();
                while !p.consume_token(&Token::RParen) {
                    // [mode] [name] type
                    for m in ["IN", "OUT", "INOUT", "VARIADIC"] {
                        let _ = eat(p, m);
                    }
                    if matches!(p.peek_token().token, Token::Word(_)) && matches!(p.peek_nth_token(1).token, Token::Word(_)) {
                        p.next_token();
                    }
                    ts.push(p.parse_data_type()?);
                    let _ = p.consume_token(&Token::Comma);
                }
                Some(ts)
            } else {
                None
            };
            fns.push((name, args));
            if !p.consume_token(&Token::Comma) {
                break;
            }
        }
        return Ok((GrantTarget::Functions(Vec::new()), fns));
    }
    if word_at(p, 0, "LANGUAGE")
        || word_at(p, 0, "FOREIGN")
        || word_at(p, 0, "LARGE")
        || word_at(p, 0, "TABLESPACE")
        || word_at(p, 0, "PARAMETER")
    {
        while !matches!(p.peek_token().token, Token::EOF | Token::SemiColon) && !word_at(p, 0, "TO") && !word_at(p, 0, "FROM") {
            p.next_token();
        }
        return Ok((GrantTarget::Other, Vec::new()));
    }
    let _ = eat(p, "TABLE");
    Ok((GrantTarget::Tables(lower(name_list(p)?)?), Vec::new()))
}

fn grant_or_revoke(p: &mut Parser, revoke: bool) -> Result<RoleStmt, ParserError> {
    let grant_option = revoke && word_at(p, 0, "GRANT") && word_at(p, 1, "OPTION") && {
        p.next_token();
        p.next_token();
        expect(p, "FOR")?;
        true
    };
    let admin_for = revoke && word_at(p, 0, "ADMIN") && word_at(p, 1, "OPTION") && {
        p.next_token();
        p.next_token();
        expect(p, "FOR")?;
        true
    };
    // GRANT role TO member: no ON clause.
    let start = p.index();
    let has_on = {
        let mut i = 0;
        loop {
            match &p.peek_nth_token(i).token {
                Token::EOF | Token::SemiColon => break false,
                Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("on") => break true,
                Token::Word(w)
                    if w.quote_style.is_none() && (w.value.eq_ignore_ascii_case("to") || w.value.eq_ignore_ascii_case("from")) =>
                {
                    break false;
                }
                _ => i += 1,
            }
        }
    };
    let _ = start;
    if !has_on {
        let roles = role_list(p)?;
        if revoke {
            expect(p, "FROM")?;
        } else {
            expect(p, "TO")?;
        }
        let members = role_list(p)?;
        let mut admin = admin_for;
        while !matches!(p.peek_token().token, Token::EOF | Token::SemiColon) {
            if eat(p, "WITH") {
                admin |= eat(p, "ADMIN");
                let _ = eat(p, "INHERIT") || eat(p, "SET");
                let _ = eat(p, "OPTION") || eat(p, "TRUE");
            } else {
                p.next_token();
            }
        }
        return Ok(RoleStmt::Lowered(Statement::GrantRole { roles, members, admin, revoke }));
    }
    let (privileges, columns) = privileges(p)?;
    expect(p, "ON")?;
    let (target, functions) = target(p)?;
    if revoke {
        expect(p, "FROM")?;
    } else {
        expect(p, "TO")?;
    }
    let mut grantees = Vec::new();
    loop {
        let _ = eat(p, "GROUP");
        grantees.push(role_name(p)?);
        if !p.consume_token(&Token::Comma) {
            break;
        }
    }
    let mut stmt = GrantStmt { privileges, columns, target, grantees, grant_option, revoke, cascade: false };
    while !matches!(p.peek_token().token, Token::EOF | Token::SemiColon) {
        if eat(p, "WITH") {
            expect(p, "GRANT")?;
            expect(p, "OPTION")?;
            stmt.grant_option = true;
        } else if eat(p, "CASCADE") {
            stmt.cascade = true;
        } else if eat(p, "RESTRICT") {
        } else if eat(p, "GRANTED") {
            expect(p, "BY")?;
            role_name(p)?;
        } else {
            return p.expected("end of statement", p.peek_token());
        }
    }
    Ok(if functions.is_empty() {
        RoleStmt::Lowered(Statement::Grant(Box::new(stmt)))
    } else {
        RoleStmt::GrantFunctions { stmt, functions }
    })
}

pub(crate) fn try_parse(p: &mut Parser) -> Result<Option<RoleStmt>, ParserError> {
    let role_word = |p: &Parser, n: usize| word_at(p, n, "ROLE") || word_at(p, n, "USER") || word_at(p, n, "GROUP");
    if word_at(p, 0, "CREATE") && role_word(p, 1) {
        p.next_token();
        let user = eat(p, "USER");
        if !user {
            p.next_token();
        }
        let if_not_exists = word_at(p, 0, "IF") && {
            p.next_token();
            expect(p, "NOT")?;
            expect(p, "EXISTS")?;
            true
        };
        let name = role_name(p)?;
        let mut options = role_options(p)?;
        // CREATE USER is CREATE ROLE ... LOGIN.
        if user && options.login.is_none() {
            options.login = Some(true);
        }
        return Ok(Some(RoleStmt::Lowered(Statement::CreateRole { name, options: Box::new(options), if_not_exists })));
    }
    if word_at(p, 0, "ALTER") && role_word(p, 1) {
        p.next_token();
        p.next_token();
        let name = role_name(p)?;
        let action = if eat(p, "RENAME") {
            expect(p, "TO")?;
            AlterRoleAction::Rename(role_name(p)?)
        } else if word_at(p, 0, "IN") && word_at(p, 1, "DATABASE") || word_at(p, 0, "SET") || word_at(p, 0, "RESET") {
            if eat(p, "IN") {
                p.next_token();
                p.parse_identifier()?;
            }
            if eat(p, "RESET") {
                AlterRoleAction::Reset(if eat(p, "ALL") { None } else { Some(crate::ident(&p.parse_identifier()?)) })
            } else {
                expect(p, "SET")?;
                let param = crate::ident(&p.parse_identifier()?);
                if !(eat(p, "TO") || p.consume_token(&Token::Eq)) {
                    return p.expected("TO or =", p.peek_token());
                }
                let mut parts = Vec::new();
                while !matches!(p.peek_token().token, Token::EOF | Token::SemiColon) {
                    parts.push(match p.next_token().token {
                        Token::SingleQuotedString(s) => s,
                        Token::Comma => ",".to_owned(),
                        t => t.to_string(),
                    });
                }
                let value = parts.join(" ").replace(" , ", ", ");
                AlterRoleAction::Set(param, (!value.eq_ignore_ascii_case("default")).then_some(value))
            }
        } else {
            AlterRoleAction::Options(role_options(p)?)
        };
        return Ok(Some(RoleStmt::Lowered(Statement::AlterRole { name, action })));
    }
    if word_at(p, 0, "DROP") && role_word(p, 1) {
        p.next_token();
        p.next_token();
        let if_exists = eat(p, "IF") && {
            expect(p, "EXISTS")?;
            true
        };
        let names = role_list(p)?;
        return Ok(Some(RoleStmt::Lowered(Statement::DropRole { names, if_exists })));
    }
    if word_at(p, 0, "GRANT") || word_at(p, 0, "REVOKE") {
        let revoke = word_at(p, 0, "REVOKE");
        p.next_token();
        return grant_or_revoke(p, revoke).map(Some);
    }
    if word_at(p, 0, "ALTER") && word_at(p, 1, "DEFAULT") && word_at(p, 2, "PRIVILEGES") {
        for _ in 0..3 {
            p.next_token();
        }
        let mut schemas = Vec::new();
        loop {
            if eat(p, "FOR") {
                let _ = eat(p, "ROLE") || eat(p, "USER");
                role_list(p)?;
            } else if eat(p, "IN") {
                expect(p, "SCHEMA")?;
                schemas.extend(ident_list(p)?);
            } else {
                break;
            }
        }
        let revoke = eat(p, "REVOKE");
        if !revoke {
            expect(p, "GRANT")?;
        }
        let grant_option = revoke && eat(p, "GRANT") && {
            expect(p, "OPTION")?;
            expect(p, "FOR")?;
            true
        };
        let (privileges, columns) = privileges(p)?;
        expect(p, "ON")?;
        let kind = match p.next_token().token {
            Token::Word(w) => match w.value.to_lowercase().as_str() {
                "tables" => 'r',
                "sequences" => 'S',
                "functions" | "routines" => 'f',
                "types" => 'T',
                "schemas" => 'n',
                _ => return p.expected("TABLES, SEQUENCES, FUNCTIONS, TYPES or SCHEMAS", p.peek_token()),
            },
            other => return p.expected("TABLES", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
        };
        if revoke {
            expect(p, "FROM")?;
        } else {
            expect(p, "TO")?;
        }
        let grantees = role_list(p)?;
        let mut stmt = GrantStmt { privileges, columns, target: GrantTarget::Other, grantees, grant_option, revoke, cascade: false };
        while !matches!(p.peek_token().token, Token::EOF | Token::SemiColon) {
            if eat(p, "WITH") {
                expect(p, "GRANT")?;
                expect(p, "OPTION")?;
                stmt.grant_option = true;
            } else if eat(p, "CASCADE") {
                stmt.cascade = true;
            } else {
                let _ = eat(p, "RESTRICT");
                break;
            }
        }
        return Ok(Some(RoleStmt::Lowered(Statement::AlterDefaultPrivileges { schemas, kind, grant: Box::new(stmt) })));
    }
    if word_at(p, 0, "DROP") && word_at(p, 1, "OWNED") {
        p.next_token();
        p.next_token();
        expect(p, "BY")?;
        let roles = role_list(p)?;
        let cascade = eat(p, "CASCADE");
        let _ = eat(p, "RESTRICT");
        return Ok(Some(RoleStmt::Lowered(Statement::DropOwned { roles, cascade })));
    }
    if word_at(p, 0, "REASSIGN") && word_at(p, 1, "OWNED") {
        p.next_token();
        p.next_token();
        expect(p, "BY")?;
        let roles = role_list(p)?;
        expect(p, "TO")?;
        let to = role_name(p)?;
        return Ok(Some(RoleStmt::Lowered(Statement::ReassignOwned { roles, to })));
    }
    // ALTER {SCHEMA | SEQUENCE | VIEW | MATERIALIZED VIEW | FUNCTION | PROCEDURE | TYPE | DOMAIN | DATABASE} name OWNER TO role
    if word_at(p, 0, "ALTER")
        && let Some((object, skip)) = owner_object(p)
    {
        // Find OWNER TO at the top level before the end of the statement.
        let mut i = skip;
        let mut depth = 0i32;
        let found = loop {
            match &p.peek_nth_token(i).token {
                Token::EOF | Token::SemiColon => break false,
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                Token::Word(w) if depth == 0 && w.quote_style.is_none() && w.value.eq_ignore_ascii_case("owner") => break true,
                _ => {}
            }
            i += 1;
        };
        if !found {
            return Ok(None);
        }
        for _ in 0..skip {
            p.next_token();
        }
        let name = p.parse_object_name(false)?;
        if p.consume_token(&Token::LParen) {
            let mut depth = 1;
            while depth > 0 {
                match p.next_token().token {
                    Token::LParen => depth += 1,
                    Token::RParen => depth -= 1,
                    Token::EOF => break,
                    _ => {}
                }
            }
        }
        expect(p, "OWNER")?;
        expect(p, "TO")?;
        let owner = role_name(p)?;
        let name = object_name(&name).map_err(|e| ParserError::ParserError(e.message))?;
        return Ok(Some(RoleStmt::Lowered(Statement::AlterOwner { object, name, owner })));
    }
    Ok(None)
}

fn owner_object(p: &Parser) -> Option<(OwnedObject, usize)> {
    Some(if word_at(p, 1, "SCHEMA") {
        (OwnedObject::Schema, 2)
    } else if word_at(p, 1, "SEQUENCE") {
        (OwnedObject::Sequence, 2)
    } else if word_at(p, 1, "VIEW") {
        (OwnedObject::View, 2)
    } else if word_at(p, 1, "MATERIALIZED") && word_at(p, 2, "VIEW") {
        (OwnedObject::MaterializedView, 3)
    } else if word_at(p, 1, "FUNCTION") {
        (OwnedObject::Function, 2)
    } else if word_at(p, 1, "PROCEDURE") || word_at(p, 1, "ROUTINE") {
        (OwnedObject::Procedure, 2)
    } else if word_at(p, 1, "TYPE") {
        (OwnedObject::Type, 2)
    } else if word_at(p, 1, "DOMAIN") {
        (OwnedObject::Domain, 2)
    } else if word_at(p, 1, "DATABASE") {
        (OwnedObject::Database, 2)
    } else {
        return None;
    })
}

pub(crate) fn lower(r: RoleStmt) -> SqlResult<Statement> {
    Ok(match r {
        RoleStmt::Lowered(s) => s,
        RoleStmt::GrantFunctions { mut stmt, functions } => {
            let fns = functions
                .into_iter()
                .map(|(n, ts)| {
                    Ok((object_name(&n)?, ts.map(|ts| ts.iter().map(lower_type).collect::<SqlResult<Vec<TypeName>>>()).transpose()?))
                })
                .collect::<SqlResult<Vec<_>>>()?;
            stmt.target = GrantTarget::Functions(fns);
            Statement::Grant(Box::new(stmt))
        }
    })
}
