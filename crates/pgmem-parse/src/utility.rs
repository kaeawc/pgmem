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
    Fetch {
        name: sqlparser::ast::Ident,
        direction: FetchDirection,
        is_move: bool,
    },
    Maintenance {
        tag: &'static str,
        tables: Vec<sqlparser::ast::ObjectName>,
        in_block_ok: bool,
    },
    SetConstraints {
        names: Vec<sqlparser::ast::ObjectName>,
        deferred: bool,
    },
    AlterConstraint {
        table: sqlparser::ast::ObjectName,
        if_exists: bool,
        name: sqlparser::ast::Ident,
        timing: pgmem_ir::ConstraintTiming,
    },
    RenameIndex {
        name: sqlparser::ast::ObjectName,
        new_name: sqlparser::ast::Ident,
        if_exists: bool,
    },
    AttachIndex {
        index: sqlparser::ast::ObjectName,
        partition: sqlparser::ast::ObjectName,
    },
    AlterSequence {
        name: sqlparser::ast::ObjectName,
        if_exists: bool,
        options: RawSeqOptions,
        rename_to: Option<sqlparser::ast::Ident>,
    },
    AlterColumn {
        table: sqlparser::ast::ObjectName,
        if_exists: bool,
        column: sqlparser::ast::Ident,
        action: ColumnAction,
    },
    Copy(Box<crate::copy::RawCopy>),
    /// `ALTER TABLE t` with column / table attribute actions only (`attr_action`).
    AlterAttrs {
        table: sqlparser::ast::ObjectName,
        if_exists: bool,
        actions: Vec<pgmem_ir::AlterTableAction>,
    },
    RelOptions {
        object: pgmem_ir::ObjectType,
        name: sqlparser::ast::ObjectName,
        if_exists: bool,
        reset: bool,
        options: Vec<sqlparser::ast::SqlOption>,
    },
}

/// Sequence options as parsed (the type still a sqlparser type).
#[derive(Default)]
pub(crate) struct RawSeqOptions {
    pub options: pgmem_ir::SequenceOptions,
    pub data_type: Option<sqlparser::ast::DataType>,
    pub owned_by: Option<Option<sqlparser::ast::ObjectName>>,
}

/// The ALTER COLUMN actions sqlparser lacks.
pub(crate) enum ColumnAction {
    SetIdentity { always: Option<bool>, options: RawSeqOptions },
    DropIdentity { if_exists: bool },
    SetExpression(sqlparser::ast::Expr),
    DropExpression { if_exists: bool },
}

fn lower_seq_options(raw: RawSeqOptions) -> SqlResult<pgmem_ir::SequenceOptions> {
    let mut o = raw.options;
    o.data_type = raw.data_type.as_ref().map(crate::expr::lower_type).transpose()?;
    o.owned_by = raw.owned_by.map(|n| n.as_ref().map(object_name).transpose()).transpose()?;
    Ok(o)
}

pub(crate) fn lower(u: Utility) -> SqlResult<Statement> {
    Ok(match u {
        Utility::Copy(c) => return crate::copy::lower(*c),
        Utility::Fetch { name, direction, is_move } => Statement::Fetch { name: crate::ident(&name), direction, is_move },
        Utility::Maintenance { tag, tables, in_block_ok } => Statement::Maintenance {
            tag: tag.to_owned(),
            tables: tables.iter().map(object_name).collect::<SqlResult<Vec<ObjectName>>>()?,
            in_block_ok,
        },
        Utility::SetConstraints { names, deferred } => {
            Statement::SetConstraints { names: names.iter().map(object_name).collect::<SqlResult<Vec<_>>>()?, deferred }
        }
        Utility::AlterSequence { name, if_exists, options, rename_to } => Statement::AlterSequence {
            name: object_name(&name)?,
            if_exists,
            options: Box::new(lower_seq_options(options)?),
            rename_to: rename_to.as_ref().map(crate::ident),
        },
        Utility::AlterColumn { table, if_exists, column, action } => {
            let column = crate::ident(&column);
            let action = match action {
                ColumnAction::SetIdentity { always, options } => {
                    pgmem_ir::AlterTableAction::SetIdentity { column, always, options: lower_seq_options(options)? }
                }
                ColumnAction::DropIdentity { if_exists } => pgmem_ir::AlterTableAction::DropIdentity { column, if_exists },
                ColumnAction::SetExpression(e) => pgmem_ir::AlterTableAction::SetExpression { column, expr: crate::expr::lower_expr(e)? },
                ColumnAction::DropExpression { if_exists } => pgmem_ir::AlterTableAction::DropExpression { column, if_exists },
            };
            Statement::AlterTable(Box::new(pgmem_ir::AlterTable { name: object_name(&table)?, if_exists, actions: vec![action] }))
        }
        Utility::AttachIndex { index, partition } => {
            Statement::AttachIndexPartition { index: object_name(&index)?, partition: object_name(&partition)? }
        }
        Utility::AlterAttrs { table, if_exists, actions } => {
            Statement::AlterTable(Box::new(pgmem_ir::AlterTable { name: object_name(&table)?, if_exists, actions }))
        }
        Utility::RelOptions { object, name, if_exists, reset, options } => Statement::AlterRelOptions {
            object,
            name: object_name(&name)?,
            if_exists,
            reset,
            options: options.iter().map(crate::ddl::rel_option).collect::<SqlResult<Vec<_>>>()?,
        },
        Utility::RenameIndex { name, new_name, if_exists } => {
            Statement::RenameIndex { name: object_name(&name)?, new_name: crate::ident(&new_name), if_exists }
        }
        Utility::AlterConstraint { table, if_exists, name, timing } => Statement::AlterTable(Box::new(pgmem_ir::AlterTable {
            name: object_name(&table)?,
            if_exists,
            actions: vec![pgmem_ir::AlterTableAction::AlterConstraint { name: crate::ident(&name), timing }],
        })),
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
    if word_at(p, 0, "COPY") {
        p.next_token();
        return crate::copy::parse(p).map(|c| Some(Utility::Copy(Box::new(c))));
    }
    if word_at(p, 0, "SET") && word_at(p, 1, "CONSTRAINTS") {
        p.next_token();
        p.next_token();
        return set_constraints(p).map(Some);
    }
    if word_at(p, 0, "ALTER") && word_at(p, 1, "TABLE") && alter_constraint_ahead(p) {
        p.next_token();
        p.next_token();
        return alter_constraint(p).map(Some);
    }
    // `ALTER SEQUENCE name OWNER TO role` is parsed with the other OWNER TO forms.
    if word_at(p, 0, "ALTER") && word_at(p, 1, "SEQUENCE") && !sequence_owner_to_ahead(p) {
        p.next_token();
        p.next_token();
        return alter_sequence(p).map(Some);
    }
    if word_at(p, 0, "ALTER") && word_at(p, 1, "TABLE") && alter_attrs_ahead(p) {
        p.next_token();
        p.next_token();
        return alter_attrs(p).map(Some);
    }
    if word_at(p, 0, "ALTER") && word_at(p, 1, "TABLE") && alter_column_ahead(p) {
        p.next_token();
        p.next_token();
        return alter_column(p).map(Some);
    }
    if let Some(u) = alter_rel_options(p)? {
        return Ok(Some(u));
    }
    if word_at(p, 0, "ALTER") && word_at(p, 1, "INDEX") {
        let n = if word_at(p, 2, "IF") { 4 } else { 2 };
        // ALTER INDEX [IF EXISTS] name [. name] RENAME TO new
        let mut k = n + 1;
        while p.peek_nth_token(k).token == Token::Period {
            k += 2;
        }
        if word_at(p, k, "RENAME") && word_at(p, k + 1, "TO") {
            p.next_token();
            p.next_token();
            let if_exists = eat_word(p, "IF") && eat_word(p, "EXISTS");
            let name = p.parse_object_name(false)?;
            p.next_token();
            p.next_token();
            let new_name = p.parse_identifier()?;
            return Ok(Some(Utility::RenameIndex { name, new_name, if_exists }));
        }
        // ALTER INDEX name [. name] ATTACH PARTITION index (no IF EXISTS in gram.y)
        if n == 2 && word_at(p, k, "ATTACH") && word_at(p, k + 1, "PARTITION") {
            p.next_token();
            p.next_token();
            let index = p.parse_object_name(false)?;
            p.next_token();
            p.next_token();
            let partition = p.parse_object_name(false)?;
            return Ok(Some(Utility::AttachIndex { index, partition }));
        }
    }
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

/// `ALTER {INDEX | VIEW | MATERIALIZED VIEW} [IF EXISTS] name {SET | RESET} (option, ...)`.
fn alter_rel_options(p: &mut Parser) -> Result<Option<Utility>, ParserError> {
    use pgmem_ir::ObjectType as O;
    if !word_at(p, 0, "ALTER") {
        return Ok(None);
    }
    let (object, mut n) = if word_at(p, 1, "INDEX") {
        (O::Index, 2)
    } else if word_at(p, 1, "VIEW") {
        (O::View, 2)
    } else if word_at(p, 1, "MATERIALIZED") && word_at(p, 2, "VIEW") {
        (O::MaterializedView, 3)
    } else if word_at(p, 1, "TABLE") {
        // sqlparser reads `RESET (name, ...)` of a table, but not `RESET (name = value)`, which
        // Postgres rejects at execution.
        (O::Table, 2)
    } else {
        return Ok(None);
    };
    if word_at(p, n, "IF") && word_at(p, n + 1, "EXISTS") {
        n += 2;
    }
    n += 1;
    while p.peek_nth_token(n).token == Token::Period {
        n += 2;
    }
    if !((word_at(p, n, "SET") || word_at(p, n, "RESET")) && p.peek_nth_token(n + 1).token == Token::LParen) {
        return Ok(None);
    }
    if object == O::Table {
        // Only a statement of just `RESET (...)`: combined actions go through sqlparser.
        let mut depth = 0usize;
        let mut m = n + 1;
        loop {
            match p.peek_nth_token(m).token {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                Token::EOF => return Ok(None),
                _ => {}
            }
            m += 1;
        }
        if !word_at(p, n, "RESET") || !matches!(p.peek_nth_token(m + 1).token, Token::SemiColon | Token::EOF) {
            return Ok(None);
        }
    }
    for _ in 0..(if object == O::MaterializedView { 3 } else { 2 }) {
        p.next_token();
    }
    let if_exists = eat_word(p, "IF") && eat_word(p, "EXISTS");
    let name = p.parse_object_name(false)?;
    let reset = word_at(p, 0, "RESET");
    p.next_token();
    p.expect_token(&Token::LParen)?;
    let options = p.parse_comma_separated(|p| p.parse_sql_option())?;
    p.expect_token(&Token::RParen)?;
    Ok(Some(Utility::RelOptions { object, name, if_exists, reset, options }))
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

/// `SET CONSTRAINTS {ALL | name [, ...]} {DEFERRED | IMMEDIATE}`.
fn set_constraints(p: &mut Parser) -> Result<Utility, ParserError> {
    let mut names = Vec::new();
    if !eat_word(p, "ALL") {
        loop {
            names.push(p.parse_object_name(false)?);
            if !p.consume_token(&Token::Comma) {
                break;
            }
        }
    }
    let deferred = if eat_word(p, "DEFERRED") {
        true
    } else if eat_word(p, "IMMEDIATE") {
        false
    } else {
        return p.expected("DEFERRED or IMMEDIATE", p.peek_token());
    };
    Ok(Utility::SetConstraints { names, deferred })
}

/// Whether `ALTER TABLE [IF EXISTS] [ONLY] name ALTER CONSTRAINT` comes next.
/// `ALTER SEQUENCE [IF EXISTS] name OWNER TO ...`.
fn sequence_owner_to_ahead(p: &Parser) -> bool {
    let mut n = if word_at(p, 2, "IF") && word_at(p, 3, "EXISTS") { 5 } else { 3 };
    while p.peek_nth_token(n).token == Token::Period {
        n += 2;
    }
    word_at(p, n, "OWNER") && word_at(p, n + 1, "TO")
}

fn alter_constraint_ahead(p: &Parser) -> bool {
    let mut n = 2;
    if word_at(p, n, "IF") && word_at(p, n + 1, "EXISTS") {
        n += 2;
    }
    if word_at(p, n, "ONLY") {
        n += 1;
    }
    // The (possibly qualified) table name.
    n += 1;
    while p.peek_nth_token(n).token == Token::Period {
        n += 2;
    }
    word_at(p, n, "ALTER") && word_at(p, n + 1, "CONSTRAINT")
}

/// `... name ALTER CONSTRAINT cname [[NOT] DEFERRABLE] [INITIALLY {DEFERRED | IMMEDIATE}]`.
fn alter_constraint(p: &mut Parser) -> Result<Utility, ParserError> {
    let if_exists = eat_word(p, "IF") && eat_word(p, "EXISTS");
    let _ = eat_word(p, "ONLY");
    let table = p.parse_object_name(false)?;
    let _ = eat_word(p, "ALTER") && eat_word(p, "CONSTRAINT");
    let name = p.parse_identifier()?;
    let mut timing = pgmem_ir::ConstraintTiming::default();
    loop {
        if eat_word(p, "NOT") {
            if !eat_word(p, "DEFERRABLE") {
                return p.expected("DEFERRABLE", p.peek_token());
            }
            timing.deferrable = false;
        } else if eat_word(p, "DEFERRABLE") {
            timing.deferrable = true;
        } else if eat_word(p, "INITIALLY") {
            timing.initially_deferred = eat_word(p, "DEFERRED");
            if !timing.initially_deferred && !eat_word(p, "IMMEDIATE") {
                return p.expected("DEFERRED or IMMEDIATE", p.peek_token());
            }
        } else {
            break;
        }
    }
    Ok(Utility::AlterConstraint { table, if_exists, name, timing })
}

/// `ALTER SEQUENCE [IF EXISTS] name {options | RENAME TO new}` (after ALTER SEQUENCE).
fn alter_sequence(p: &mut Parser) -> Result<Utility, ParserError> {
    let if_exists = eat_word(p, "IF") && eat_word(p, "EXISTS");
    let name = p.parse_object_name(false)?;
    if eat_word(p, "RENAME") {
        if !eat_word(p, "TO") {
            return p.expected("TO", p.peek_token());
        }
        let new = p.parse_identifier()?;
        return Ok(Utility::AlterSequence { name, if_exists, options: RawSeqOptions::default(), rename_to: Some(new) });
    }
    let mut options = RawSeqOptions::default();
    while seq_option(p, &mut options)? {}
    if !matches!(p.peek_token().token, Token::SemiColon | Token::EOF) {
        return p.expected("end of statement", p.peek_token());
    }
    Ok(Utility::AlterSequence { name, if_exists, options, rename_to: None })
}

fn required_int(p: &mut Parser) -> Result<i64, ParserError> {
    match signed_int(p)? {
        Some(v) => Ok(v),
        None => p.expected("an integer", p.peek_token()),
    }
}

/// One sequence option (`SeqOptElem`, plus RESTART); false when none comes next.
fn seq_option(p: &mut Parser, o: &mut RawSeqOptions) -> Result<bool, ParserError> {
    let opts = &mut o.options;
    if eat_word(p, "AS") {
        o.data_type = Some(p.parse_data_type()?);
    } else if eat_word(p, "INCREMENT") {
        let _ = eat_word(p, "BY");
        opts.increment = Some(required_int(p)?);
    } else if eat_word(p, "MINVALUE") {
        opts.min = Some(Some(required_int(p)?));
    } else if eat_word(p, "MAXVALUE") {
        opts.max = Some(Some(required_int(p)?));
    } else if word_at(p, 0, "NO") && (word_at(p, 1, "MINVALUE") || word_at(p, 1, "MAXVALUE") || word_at(p, 1, "CYCLE")) {
        p.next_token();
        if eat_word(p, "MINVALUE") {
            opts.min = Some(None);
        } else if eat_word(p, "MAXVALUE") {
            opts.max = Some(None);
        } else {
            p.next_token();
            opts.cycle = Some(false);
        }
    } else if eat_word(p, "START") {
        let _ = eat_word(p, "WITH");
        opts.start = Some(required_int(p)?);
    } else if eat_word(p, "RESTART") {
        let _ = eat_word(p, "WITH");
        opts.restart = Some(signed_int(p)?);
    } else if eat_word(p, "CACHE") {
        opts.cache = Some(required_int(p)?);
    } else if eat_word(p, "CYCLE") {
        opts.cycle = Some(true);
    } else if word_at(p, 0, "OWNED") && word_at(p, 1, "BY") {
        p.next_token();
        p.next_token();
        o.owned_by = Some(if eat_word(p, "NONE") { None } else { Some(p.parse_object_name(false)?) });
    } else {
        return Ok(false);
    }
    Ok(true)
}

/// `ALTER TABLE [IF EXISTS] [ONLY] t ALTER [COLUMN] c` followed by an identity or generation
/// expression action.
fn alter_column_ahead(p: &Parser) -> bool {
    let mut n = 2;
    if word_at(p, n, "IF") && word_at(p, n + 1, "EXISTS") {
        n += 2;
    }
    if word_at(p, n, "ONLY") {
        n += 1;
    }
    n += 1;
    while p.peek_nth_token(n).token == Token::Period {
        n += 2;
    }
    if !word_at(p, n, "ALTER") {
        return false;
    }
    n += 1;
    if word_at(p, n, "COLUMN") {
        n += 1;
    }
    n += 1;
    let set_opt = ["GENERATED", "INCREMENT", "START", "MINVALUE", "MAXVALUE", "CYCLE", "NO", "CACHE", "EXPRESSION"];
    (word_at(p, n, "SET") && set_opt.iter().any(|w| word_at(p, n + 1, w)))
        || word_at(p, n, "RESTART")
        || (word_at(p, n, "DROP") && (word_at(p, n + 1, "IDENTITY") || word_at(p, n + 1, "EXPRESSION")))
}

fn alter_column(p: &mut Parser) -> Result<Utility, ParserError> {
    let if_exists = eat_word(p, "IF") && eat_word(p, "EXISTS");
    let _ = eat_word(p, "ONLY");
    let table = p.parse_object_name(false)?;
    let _ = eat_word(p, "ALTER");
    let _ = eat_word(p, "COLUMN");
    let column = p.parse_identifier()?;
    let action = if eat_word(p, "DROP") {
        let identity = eat_word(p, "IDENTITY");
        if !identity {
            let _ = eat_word(p, "EXPRESSION");
        }
        let if_exists = eat_word(p, "IF") && eat_word(p, "EXISTS");
        if identity { ColumnAction::DropIdentity { if_exists } } else { ColumnAction::DropExpression { if_exists } }
    } else if word_at(p, 0, "SET") && word_at(p, 1, "EXPRESSION") {
        p.next_token();
        p.next_token();
        if !eat_word(p, "AS") {
            return p.expected("AS", p.peek_token());
        }
        p.expect_token(&Token::LParen)?;
        let e = p.parse_expr()?;
        p.expect_token(&Token::RParen)?;
        ColumnAction::SetExpression(e)
    } else {
        let mut always = None;
        let mut options = RawSeqOptions::default();
        loop {
            if word_at(p, 0, "SET") && word_at(p, 1, "GENERATED") {
                p.next_token();
                p.next_token();
                if eat_word(p, "ALWAYS") {
                    always = Some(true);
                } else if eat_word(p, "BY") && eat_word(p, "DEFAULT") {
                    always = Some(false);
                } else {
                    return p.expected("ALWAYS or BY DEFAULT", p.peek_token());
                }
            } else if word_at(p, 0, "SET") {
                p.next_token();
                if !seq_option(p, &mut options)? {
                    return p.expected("a sequence option", p.peek_token());
                }
            } else if word_at(p, 0, "RESTART") {
                seq_option(p, &mut options)?;
            } else {
                break;
            }
        }
        ColumnAction::SetIdentity { always, options }
    };
    if !matches!(p.peek_token().token, Token::SemiColon | Token::EOF) {
        return p.expected("end of statement", p.peek_token());
    }
    Ok(Utility::AlterColumn { table, if_exists, column, action })
}

/// The token index after `ALTER TABLE [IF EXISTS] [ONLY] name`.
fn after_table_name(p: &Parser) -> usize {
    let mut n = 2;
    if word_at(p, n, "IF") && word_at(p, n + 1, "EXISTS") {
        n += 2;
    }
    if word_at(p, n, "ONLY") {
        n += 1;
    }
    n += 1;
    while p.peek_nth_token(n).token == Token::Period {
        n += 2;
    }
    n
}

/// Whether the first action of `ALTER TABLE name ...` is a column / table attribute one
/// (`SET STATISTICS | STORAGE | COMPRESSION`, `REPLICA IDENTITY`, `CLUSTER ON`,
/// `SET WITHOUT CLUSTER`, `SET [UN]LOGGED`).
fn alter_attrs_ahead(p: &Parser) -> bool {
    let mut n = after_table_name(p);
    if attr_action_at(p, n) {
        return true;
    }
    // A later comma-separated action may be an attribute one (`SET (fillfactor = 70), ALTER COLUMN ...`).
    let mut depth = 0usize;
    loop {
        match p.peek_nth_token(n).token {
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 && attr_action_at(p, n + 1) => return true,
            Token::SemiColon | Token::EOF => return false,
            _ => {}
        }
        n += 1;
    }
}

fn attr_action_at(p: &Parser, mut n: usize) -> bool {
    if word_at(p, n, "ALTER") {
        n += 1;
        if word_at(p, n, "COLUMN") {
            n += 1;
        }
        // The column name (or number).
        n += 1;
        return word_at(p, n, "SET") && ["STATISTICS", "STORAGE", "COMPRESSION"].iter().any(|w| word_at(p, n + 1, w));
    }
    word_at(p, n, "REPLICA")
        || (word_at(p, n, "CLUSTER") && word_at(p, n + 1, "ON"))
        || (word_at(p, n, "SET")
            && (word_at(p, n + 1, "LOGGED")
                || word_at(p, n + 1, "UNLOGGED")
                || (word_at(p, n + 1, "WITHOUT") && word_at(p, n + 2, "CLUSTER"))))
}

/// `ALTER TABLE [IF EXISTS] [ONLY] name action [, ...]` for the attribute actions.
fn alter_attrs(p: &mut Parser) -> Result<Utility, ParserError> {
    let if_exists = eat_word(p, "IF") && eat_word(p, "EXISTS");
    let _ = eat_word(p, "ONLY");
    let table = p.parse_object_name(false)?;
    let mut actions = Vec::new();
    loop {
        actions.push(attr_action(p)?);
        if p.consume_token(&Token::Comma) {
            continue;
        }
        break;
    }
    if !matches!(p.peek_token().token, Token::SemiColon | Token::EOF) {
        return p.expected("end of statement", p.peek_token());
    }
    Ok(Utility::AlterAttrs { table, if_exists, actions })
}

fn attr_action(p: &mut Parser) -> Result<pgmem_ir::AlterTableAction, ParserError> {
    use pgmem_ir::AlterTableAction as A;
    if eat_word(p, "ALTER") {
        let _ = eat_word(p, "COLUMN");
        let column = crate::ident(&p.parse_identifier()?);
        if !eat_word(p, "SET") {
            return p.expected("SET", p.peek_token());
        }
        if eat_word(p, "STATISTICS") {
            let Some(target) = signed_int(p)? else { return p.expected("an integer", p.peek_token()) };
            return Ok(A::SetStatistics { column, target });
        }
        let storage = eat_word(p, "STORAGE");
        if !storage && !eat_word(p, "COMPRESSION") {
            return p.expected("STATISTICS, STORAGE or COMPRESSION", p.peek_token());
        }
        let value = crate::ident(&p.parse_identifier()?);
        return Ok(if storage { A::SetStorage { column, storage: value } } else { A::SetCompression { column, method: value } });
    }
    if eat_word(p, "REPLICA") {
        if !eat_word(p, "IDENTITY") {
            return p.expected("IDENTITY", p.peek_token());
        }
        if eat_word(p, "USING") {
            if !eat_word(p, "INDEX") {
                return p.expected("INDEX", p.peek_token());
            }
            let index = crate::ident(&p.parse_identifier()?);
            return Ok(A::ReplicaIdentity { kind: 'i', index: Some(index) });
        }
        let kind = if eat_word(p, "DEFAULT") {
            'd'
        } else if eat_word(p, "FULL") {
            'f'
        } else if eat_word(p, "NOTHING") {
            'n'
        } else {
            return p.expected("DEFAULT, FULL, NOTHING or USING INDEX", p.peek_token());
        };
        return Ok(A::ReplicaIdentity { kind, index: None });
    }
    if eat_word(p, "CLUSTER") {
        if !eat_word(p, "ON") {
            return p.expected("ON", p.peek_token());
        }
        return Ok(A::ClusterOn(Some(crate::ident(&p.parse_identifier()?))));
    }
    if word_at(p, 0, "RESET") || (word_at(p, 0, "SET") && p.peek_nth_token(1).token == Token::LParen) {
        let reset = eat_word(p, "RESET");
        if !reset {
            p.next_token();
        }
        p.expect_token(&Token::LParen)?;
        let options = p.parse_comma_separated(|p| p.parse_sql_option())?;
        p.expect_token(&Token::RParen)?;
        let options =
            options.iter().map(crate::ddl::rel_option).collect::<SqlResult<Vec<_>>>().map_err(|e| ParserError::ParserError(e.message))?;
        return Ok(A::RelOptions { reset, options });
    }
    if !eat_word(p, "SET") {
        return p.expected("an ALTER TABLE action", p.peek_token());
    }
    if eat_word(p, "WITHOUT") {
        if !eat_word(p, "CLUSTER") {
            return p.expected("CLUSTER", p.peek_token());
        }
        return Ok(A::ClusterOn(None));
    }
    let unlogged = eat_word(p, "UNLOGGED");
    if !unlogged && !eat_word(p, "LOGGED") {
        return p.expected("LOGGED or UNLOGGED", p.peek_token());
    }
    Ok(A::SetUnlogged(unlogged))
}
