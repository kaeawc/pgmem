//! `CREATE [OR REPLACE] FUNCTION / PROCEDURE`, `DROP FUNCTION / PROCEDURE / ROUTINE`, `CALL`,
//! `DO`, `CREATE [OR REPLACE] TRIGGER` and `DROP TRIGGER`, parsed here following Postgres'
//! grammar (gram.y `CreateFunctionStmt`, `DoStmt`, `CreateTrigStmt`); names, types and
//! expressions come from sqlparser's parser.

use crate::expr::{lower_expr, lower_type};
use crate::{ident, object_name};
use pgmem_ir::{CreateFunction, CreateTrigger, FunctionArg, FunctionArgMode, FunctionReturns, RoutineKind, Statement, TriggerTiming};
use pgmem_types::SqlResult;
use sqlparser::ast as sq;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

/// A statement parsed here (pieces still sqlparser values).
pub(crate) enum FuncDdl {
    CreateFunction(Box<RawFunction>),
    DropFunction {
        items: Vec<(sq::ObjectName, Option<Vec<sq::DataType>>)>,
        if_exists: bool,
        cascade: bool,
        kind: RoutineKind,
    },
    Call {
        name: sq::ObjectName,
        args: Vec<sq::Expr>,
    },
    Do {
        language: String,
        body: String,
    },
    CreateTrigger(Box<RawTrigger>),
    RenameTrigger {
        name: sq::Ident,
        table: sq::ObjectName,
        new_name: sq::Ident,
    },
    DropTrigger {
        name: sq::Ident,
        table: sq::ObjectName,
        if_exists: bool,
        cascade: bool,
    },
    /// `COMMENT ON {FUNCTION | PROCEDURE | ROUTINE | CONSTRAINT | TRIGGER} ...`.
    Comment {
        target: CommentOn,
        name: sq::ObjectName,
        text: Option<String>,
    },
}

pub(crate) enum CommentOn {
    Function(Option<Vec<sq::DataType>>),
    Constraint(sq::ObjectName),
    Trigger(sq::ObjectName),
}

pub(crate) struct RawFunction {
    name: sq::ObjectName,
    or_replace: bool,
    procedure: bool,
    args: Vec<(FunctionArgMode, Option<sq::Ident>, sq::DataType, Option<sq::Expr>)>,
    returns: Option<RawReturns>,
    language: Option<String>,
    body: Option<String>,
    /// `RETURN expr` (SQL-standard body).
    return_expr: Option<sq::Expr>,
    /// `BEGIN ATOMIC ... END`.
    atomic: bool,
    volatility: Option<char>,
    strict: bool,
    security_definer: bool,
}

enum RawReturns {
    Type(sq::DataType),
    SetOf(sq::DataType),
    Table(Vec<(sq::Ident, sq::DataType)>),
}

pub(crate) struct RawTrigger {
    name: sq::Ident,
    or_replace: bool,
    timing: TriggerTiming,
    insert: bool,
    delete: bool,
    truncate: bool,
    update: Option<Vec<sq::Ident>>,
    table: sq::ObjectName,
    for_each_row: bool,
    when: Option<sq::Expr>,
    function: sq::ObjectName,
    args: Vec<String>,
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

fn at_end(p: &Parser) -> bool {
    matches!(p.peek_token().token, Token::SemiColon | Token::EOF)
}

/// A string constant: `'...'` (with `E'...'` escapes) or `$tag$...$tag$`.
fn string(p: &mut Parser) -> Result<String, ParserError> {
    match p.next_token().token {
        Token::SingleQuotedString(s) | Token::EscapedStringLiteral(s) => Ok(s),
        Token::DollarQuotedString(d) => Ok(d.value),
        other => p.expected("a string constant", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
    }
}

pub(crate) fn try_parse(p: &mut Parser) -> Result<Option<FuncDdl>, ParserError> {
    if word_at(p, 0, "CREATE") {
        let (or_replace, at) = if word_at(p, 1, "OR") && word_at(p, 2, "REPLACE") { (true, 3) } else { (false, 1) };
        if word_at(p, at, "FUNCTION") || word_at(p, at, "PROCEDURE") {
            for _ in 0..at {
                p.next_token();
            }
            let procedure = eat(p, "PROCEDURE");
            if !procedure {
                p.next_token();
            }
            return create_function(p, or_replace, procedure).map(|f| Some(FuncDdl::CreateFunction(Box::new(f))));
        }
        let constraint = word_at(p, at, "CONSTRAINT");
        if word_at(p, at + usize::from(constraint), "TRIGGER") {
            for _ in 0..=at + usize::from(constraint) {
                p.next_token();
            }
            return create_trigger(p, or_replace).map(|t| Some(FuncDdl::CreateTrigger(Box::new(t))));
        }
        return Ok(None);
    }
    if word_at(p, 0, "DROP") && (word_at(p, 1, "FUNCTION") || word_at(p, 1, "PROCEDURE") || word_at(p, 1, "ROUTINE")) {
        p.next_token();
        let kind = if eat(p, "FUNCTION") {
            RoutineKind::Function
        } else if eat(p, "PROCEDURE") {
            RoutineKind::Procedure
        } else {
            p.next_token();
            RoutineKind::Routine
        };
        return drop_function(p, kind).map(Some);
    }
    if word_at(p, 0, "ALTER") && word_at(p, 1, "TRIGGER") {
        p.next_token();
        p.next_token();
        let name = p.parse_identifier()?;
        expect(p, "ON")?;
        let table = p.parse_object_name(false)?;
        expect(p, "RENAME")?;
        expect(p, "TO")?;
        let new_name = p.parse_identifier()?;
        return Ok(Some(FuncDdl::RenameTrigger { name, table, new_name }));
    }
    if word_at(p, 0, "DROP") && word_at(p, 1, "TRIGGER") {
        p.next_token();
        p.next_token();
        let if_exists = eat(p, "IF") && eat(p, "EXISTS");
        let name = p.parse_identifier()?;
        expect(p, "ON")?;
        let table = p.parse_object_name(false)?;
        let cascade = eat(p, "CASCADE");
        let _ = eat(p, "RESTRICT");
        return Ok(Some(FuncDdl::DropTrigger { name, table, if_exists, cascade }));
    }
    if word_at(p, 0, "COMMENT")
        && word_at(p, 1, "ON")
        && ["FUNCTION", "PROCEDURE", "ROUTINE", "CONSTRAINT", "TRIGGER"].iter().any(|w| word_at(p, 2, w))
    {
        p.next_token();
        p.next_token();
        let routine = !(word_at(p, 0, "CONSTRAINT") || word_at(p, 0, "TRIGGER"));
        let trigger = word_at(p, 0, "TRIGGER");
        p.next_token();
        let name = p.parse_object_name(false)?;
        let target = if routine {
            let args = if p.consume_token(&Token::LParen) {
                let mut ts = Vec::new();
                if !p.consume_token(&Token::RParen) {
                    loop {
                        let (mode, _, ty, _) = function_arg(p)?;
                        if mode != FunctionArgMode::Out {
                            ts.push(ty);
                        }
                        if p.consume_token(&Token::RParen) {
                            break;
                        }
                        p.expect_token(&Token::Comma)?;
                    }
                }
                Some(ts)
            } else {
                None
            };
            CommentOn::Function(args)
        } else {
            expect(p, "ON")?;
            let _ = eat(p, "DOMAIN");
            let table = p.parse_object_name(false)?;
            if trigger { CommentOn::Trigger(table) } else { CommentOn::Constraint(table) }
        };
        expect(p, "IS")?;
        let text = if eat(p, "NULL") { None } else { Some(string(p)?) };
        return Ok(Some(FuncDdl::Comment { target, name, text }));
    }
    if word_at(p, 0, "CALL") {
        p.next_token();
        let name = p.parse_object_name(false)?;
        p.expect_token(&Token::LParen)?;
        let mut args = Vec::new();
        if !p.consume_token(&Token::RParen) {
            loop {
                args.push(p.parse_expr()?);
                if p.consume_token(&Token::RParen) {
                    break;
                }
                p.expect_token(&Token::Comma)?;
            }
        }
        return Ok(Some(FuncDdl::Call { name, args }));
    }
    if word_at(p, 0, "DO")
        && matches!(p.peek_nth_token(1).token, Token::SingleQuotedString(_) | Token::DollarQuotedString(_) | Token::EscapedStringLiteral(_))
        || word_at(p, 0, "DO") && word_at(p, 1, "LANGUAGE")
    {
        p.next_token();
        let mut language = "plpgsql".to_owned();
        if eat(p, "LANGUAGE") {
            language = p.parse_identifier()?.value.to_lowercase();
        }
        let body = string(p)?;
        if eat(p, "LANGUAGE") {
            language = p.parse_identifier()?.value.to_lowercase();
        }
        return Ok(Some(FuncDdl::Do { language, body }));
    }
    Ok(None)
}

fn arg_mode(p: &mut Parser) -> FunctionArgMode {
    if eat(p, "INOUT") {
        FunctionArgMode::InOut
    } else if eat(p, "IN") {
        if eat(p, "OUT") { FunctionArgMode::InOut } else { FunctionArgMode::In }
    } else if eat(p, "OUT") {
        FunctionArgMode::Out
    } else if eat(p, "VARIADIC") {
        FunctionArgMode::Variadic
    } else {
        FunctionArgMode::In
    }
}

/// `[mode] [name] type [{DEFAULT | =} expr]`: a name is present when a type follows it.
pub(crate) fn function_arg(p: &mut Parser) -> Result<(FunctionArgMode, Option<sq::Ident>, sq::DataType, Option<sq::Expr>), ParserError> {
    let mode = arg_mode(p);
    // `name type`: the second token starts a type (a word, not `,` `)` `DEFAULT` `=` `[` `(`).
    let named = matches!(p.peek_token().token, Token::Word(_))
        && matches!(&p.peek_nth_token(1).token, Token::Word(w) if !w.value.eq_ignore_ascii_case("default"))
        && !(word_at(p, 0, "double") && word_at(p, 1, "precision"))
        && !(word_at(p, 0, "character") && word_at(p, 1, "varying"))
        && !(word_at(p, 0, "bit") && word_at(p, 1, "varying"))
        && !((word_at(p, 0, "timestamp") || word_at(p, 0, "time")) && (word_at(p, 1, "with") || word_at(p, 1, "without")))
        && !matches!(p.peek_nth_token(1).token, Token::Period);
    let name = if named { Some(p.parse_identifier()?) } else { None };
    let ty = p.parse_data_type()?;
    let default = if eat(p, "DEFAULT") || p.consume_token(&Token::Eq) { Some(p.parse_expr()?) } else { None };
    Ok((mode, name, ty, default))
}

fn create_function(p: &mut Parser, or_replace: bool, procedure: bool) -> Result<RawFunction, ParserError> {
    let name = p.parse_object_name(false)?;
    p.expect_token(&Token::LParen)?;
    let mut args = Vec::new();
    if !p.consume_token(&Token::RParen) {
        loop {
            args.push(function_arg(p)?);
            if p.consume_token(&Token::RParen) {
                break;
            }
            p.expect_token(&Token::Comma)?;
        }
    }
    let mut f = RawFunction {
        name,
        or_replace,
        procedure,
        args,
        returns: None,
        language: None,
        body: None,
        return_expr: None,
        atomic: false,
        volatility: None,
        strict: false,
        security_definer: false,
    };
    if eat(p, "RETURNS") {
        f.returns = Some(if eat(p, "SETOF") {
            RawReturns::SetOf(p.parse_data_type()?)
        } else if word_at(p, 0, "TABLE") && p.peek_nth_token(1).token == Token::LParen {
            p.next_token();
            p.next_token();
            let mut cols = Vec::new();
            loop {
                let n = p.parse_identifier()?;
                cols.push((n, p.parse_data_type()?));
                if p.consume_token(&Token::RParen) {
                    break;
                }
                p.expect_token(&Token::Comma)?;
            }
            RawReturns::Table(cols)
        } else {
            RawReturns::Type(p.parse_data_type()?)
        });
    }
    function_options(p, &mut f)?;
    Ok(f)
}

fn function_options(p: &mut Parser, f: &mut RawFunction) -> Result<(), ParserError> {
    while !at_end(p) {
        if eat(p, "LANGUAGE") {
            f.language = Some(match p.next_token().token {
                Token::Word(w) => w.value.to_lowercase(),
                Token::SingleQuotedString(s) => s.to_lowercase(),
                other => return p.expected("a language name", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
            });
        } else if eat(p, "IMMUTABLE") {
            f.volatility = Some('i');
        } else if eat(p, "STABLE") {
            f.volatility = Some('s');
        } else if eat(p, "VOLATILE") {
            f.volatility = Some('v');
        } else if eat(p, "STRICT") {
            f.strict = true;
        } else if word_at(p, 0, "RETURNS") && word_at(p, 1, "NULL") {
            for _ in 0..5 {
                p.next_token();
            }
            f.strict = true;
        } else if eat(p, "CALLED") {
            for _ in 0..3 {
                p.next_token();
            }
            f.strict = false;
        } else if eat(p, "EXTERNAL") || eat(p, "SECURITY") {
            let _ = eat(p, "SECURITY");
            f.security_definer = eat(p, "DEFINER");
            let _ = eat(p, "INVOKER");
        } else if eat(p, "NOT") || eat(p, "LEAKPROOF") || eat(p, "WINDOW") {
            let _ = eat(p, "LEAKPROOF");
        } else if eat(p, "PARALLEL") || eat(p, "COST") || eat(p, "ROWS") || eat(p, "SUPPORT") {
            p.next_token();
        } else if eat(p, "SET") {
            // SET param {TO | =} value | FROM CURRENT
            p.next_token();
            if eat(p, "FROM") {
                p.next_token();
            } else {
                p.next_token();
                p.next_token();
            }
        } else if eat(p, "AS") {
            f.body = Some(string(p)?);
            if p.consume_token(&Token::Comma) {
                string(p)?;
            }
        } else if eat(p, "RETURN") {
            f.return_expr = Some(p.parse_expr()?);
        } else if word_at(p, 0, "BEGIN") && word_at(p, 1, "ATOMIC") {
            // SQL-standard body: statements up to the matching END.
            p.next_token();
            p.next_token();
            let mut text = String::new();
            let mut depth = 0usize;
            loop {
                let t = p.next_token();
                match &t.token {
                    Token::EOF => return p.expected("END", t),
                    Token::Word(w) if w.value.eq_ignore_ascii_case("end") && depth == 0 => break,
                    Token::Word(w) if w.value.eq_ignore_ascii_case("case") => depth += 1,
                    Token::Word(w) if w.value.eq_ignore_ascii_case("end") => depth -= 1,
                    _ => {}
                }
                text.push_str(&t.token.to_string());
                text.push(' ');
            }
            f.body = Some(text);
            f.atomic = true;
        } else {
            return p.expected("a function option", p.peek_token());
        }
    }
    Ok(())
}

fn drop_function(p: &mut Parser, kind: RoutineKind) -> Result<FuncDdl, ParserError> {
    let if_exists = eat(p, "IF") && eat(p, "EXISTS");
    let mut items = Vec::new();
    loop {
        let name = p.parse_object_name(false)?;
        let types = if p.consume_token(&Token::LParen) {
            let mut ts = Vec::new();
            if !p.consume_token(&Token::RParen) {
                loop {
                    let (mode, _, ty, _) = function_arg(p)?;
                    // OUT arguments are not part of the signature.
                    if mode != FunctionArgMode::Out {
                        ts.push(ty);
                    }
                    if p.consume_token(&Token::RParen) {
                        break;
                    }
                    p.expect_token(&Token::Comma)?;
                }
            }
            Some(ts)
        } else {
            None
        };
        items.push((name, types));
        if !p.consume_token(&Token::Comma) {
            break;
        }
    }
    let cascade = eat(p, "CASCADE");
    let _ = eat(p, "RESTRICT");
    Ok(FuncDdl::DropFunction { items, if_exists, cascade, kind })
}

fn create_trigger(p: &mut Parser, or_replace: bool) -> Result<RawTrigger, ParserError> {
    let name = p.parse_identifier()?;
    let timing = if eat(p, "BEFORE") {
        TriggerTiming::Before
    } else if eat(p, "AFTER") {
        TriggerTiming::After
    } else if eat(p, "INSTEAD") {
        expect(p, "OF")?;
        TriggerTiming::InsteadOf
    } else {
        return p.expected("BEFORE, AFTER or INSTEAD OF", p.peek_token());
    };
    let mut t = RawTrigger {
        name,
        or_replace,
        timing,
        insert: false,
        delete: false,
        truncate: false,
        update: None,
        table: sq::ObjectName(Vec::new()),
        for_each_row: false,
        when: None,
        function: sq::ObjectName(Vec::new()),
        args: Vec::new(),
    };
    loop {
        if eat(p, "INSERT") {
            t.insert = true;
        } else if eat(p, "DELETE") {
            t.delete = true;
        } else if eat(p, "TRUNCATE") {
            t.truncate = true;
        } else if eat(p, "UPDATE") {
            let mut cols = Vec::new();
            if eat(p, "OF") {
                loop {
                    cols.push(p.parse_identifier()?);
                    if !p.consume_token(&Token::Comma) {
                        break;
                    }
                }
            }
            t.update = Some(cols);
        } else {
            return p.expected("INSERT, UPDATE, DELETE or TRUNCATE", p.peek_token());
        }
        if !eat(p, "OR") {
            break;
        }
    }
    expect(p, "ON")?;
    t.table = p.parse_object_name(false)?;
    loop {
        if eat(p, "FROM") {
            p.parse_object_name(false)?;
        } else if eat(p, "NOT") {
            expect(p, "DEFERRABLE")?;
        } else if eat(p, "DEFERRABLE") {
        } else if eat(p, "INITIALLY") {
            p.next_token();
        } else if eat(p, "REFERENCING") {
            while eat(p, "OLD") || eat(p, "NEW") {
                expect(p, "TABLE")?;
                let _ = eat(p, "AS");
                p.parse_identifier()?;
            }
        } else if eat(p, "FOR") {
            let _ = eat(p, "EACH");
            t.for_each_row = eat(p, "ROW");
            if !t.for_each_row {
                expect(p, "STATEMENT")?;
            }
        } else if eat(p, "WHEN") {
            p.expect_token(&Token::LParen)?;
            t.when = Some(p.parse_expr()?);
            p.expect_token(&Token::RParen)?;
        } else {
            break;
        }
    }
    expect(p, "EXECUTE")?;
    if !eat(p, "FUNCTION") {
        expect(p, "PROCEDURE")?;
    }
    t.function = p.parse_object_name(false)?;
    p.expect_token(&Token::LParen)?;
    if !p.consume_token(&Token::RParen) {
        loop {
            t.args.push(match p.next_token().token {
                Token::SingleQuotedString(s) => s,
                Token::Number(n, _) => n,
                Token::Word(w) => w.value,
                other => return p.expected("a trigger argument", sqlparser::tokenizer::TokenWithSpan::wrap(other)),
            });
            if p.consume_token(&Token::RParen) {
                break;
            }
            p.expect_token(&Token::Comma)?;
        }
    }
    Ok(t)
}

fn lower_returns(r: RawReturns) -> SqlResult<FunctionReturns> {
    Ok(match r {
        RawReturns::Type(t) => FunctionReturns::Type(lower_type(&t)?),
        RawReturns::SetOf(t) => FunctionReturns::SetOf(lower_type(&t)?),
        RawReturns::Table(cols) => {
            FunctionReturns::Table(cols.into_iter().map(|(n, t)| Ok((ident(&n), lower_type(&t)?))).collect::<SqlResult<Vec<_>>>()?)
        }
    })
}

pub(crate) fn lower(d: FuncDdl) -> SqlResult<Statement> {
    Ok(match d {
        FuncDdl::CreateFunction(f) => {
            let f = *f;
            let args = f
                .args
                .into_iter()
                .map(|(mode, name, ty, default)| {
                    Ok(FunctionArg {
                        mode,
                        name: name.as_ref().map(ident),
                        ty: lower_type(&ty)?,
                        default: default.map(lower_expr).transpose()?,
                    })
                })
                .collect::<SqlResult<Vec<_>>>()?;
            // A SQL-standard body (`RETURN expr` / `BEGIN ATOMIC`) is SQL.
            let standard = f.return_expr.is_some() || f.atomic;
            let body = match f.return_expr {
                Some(e) => Some(format!("SELECT {e}")),
                None => f.body,
            };
            let language = f.language.or_else(|| standard.then(|| "sql".to_owned()));
            Statement::CreateFunction(Box::new(CreateFunction {
                name: object_name(&f.name)?,
                or_replace: f.or_replace,
                procedure: f.procedure,
                args,
                returns: f.returns.map(lower_returns).transpose()?,
                language,
                body,
                volatility: f.volatility,
                strict: f.strict,
                security_definer: f.security_definer,
            }))
        }
        FuncDdl::DropFunction { items, if_exists, cascade, kind } => Statement::DropFunction {
            items: items
                .into_iter()
                .map(|(n, ts)| Ok((object_name(&n)?, ts.map(|ts| ts.iter().map(lower_type).collect::<SqlResult<Vec<_>>>()).transpose()?)))
                .collect::<SqlResult<Vec<_>>>()?,
            if_exists,
            cascade,
            kind,
        },
        FuncDdl::Call { name, args } => {
            Statement::Call { name: object_name(&name)?, args: args.into_iter().map(lower_expr).collect::<SqlResult<Vec<_>>>()? }
        }
        FuncDdl::Do { language, body } => Statement::Do { language, body },
        FuncDdl::CreateTrigger(t) => {
            let t = *t;
            Statement::CreateTrigger(Box::new(CreateTrigger {
                name: ident(&t.name),
                or_replace: t.or_replace,
                timing: t.timing,
                insert: t.insert,
                delete: t.delete,
                truncate: t.truncate,
                update: t.update.map(|cols| cols.iter().map(ident).collect()),
                table: object_name(&t.table)?,
                for_each_row: t.for_each_row,
                when: t.when.map(|e| e.to_string()),
                function: object_name(&t.function)?,
                args: t.args,
            }))
        }
        FuncDdl::Comment { target, name, text } => {
            let target = match target {
                CommentOn::Function(args) => {
                    pgmem_ir::CommentTarget::Function(args.map(|ts| ts.iter().map(lower_type).collect::<SqlResult<Vec<_>>>()).transpose()?)
                }
                CommentOn::Constraint(t) => pgmem_ir::CommentTarget::Constraint(object_name(&t)?),
                CommentOn::Trigger(t) => pgmem_ir::CommentTarget::Trigger(object_name(&t)?),
            };
            Statement::Comment { target, name: object_name(&name)?, text }
        }
        FuncDdl::RenameTrigger { name, table, new_name } => {
            Statement::RenameTrigger { name: ident(&name), table: object_name(&table)?, new_name: ident(&new_name) }
        }
        FuncDdl::DropTrigger { name, table, if_exists, cascade } => {
            Statement::DropTrigger { name: ident(&name), table: object_name(&table)?, if_exists, cascade }
        }
    })
}
