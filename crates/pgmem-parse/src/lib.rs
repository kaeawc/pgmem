//! SQL text → [`pgmem_ir::Statement`]s.
//!
//! Parsing is delegated to `sqlparser`'s PostgreSQL dialect; this crate lowers its AST into the
//! dialect-neutral IR, folds identifiers the way Postgres does (unquoted → lower case), rewrites
//! special syntax into function calls the way Postgres' grammar does, and maps parse failures
//! to `42601 syntax_error` with Postgres-style messages.
//!
//! A handful of utility statements that `sqlparser` doesn't understand but that clients and
//! migration tools routinely send (`SET` variants, `COMMENT ON`, `CREATE EXTENSION`, ...) are
//! recognized by prefix as a fallback and become no-ops, so connection setup never fails.

mod copy;
mod ddl;
mod dml;
mod expr;
mod extddl;
mod funcddl;
mod query;
mod roles;
mod search_cycle;
mod sqljson;
mod typeddl;
mod utility;

use pgmem_ir::{IsolationLevel, ObjectName, ObjectType, Statement, TransactionStmt};
use pgmem_types::{ErrorRef, SourceRefs, SqlError, SqlResult, code};
use sqlparser::ast as sq;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};

/// Parses a query string that may contain several `;`-separated statements (the simple-query
/// protocol). An empty string yields `[Statement::Empty]`.
pub fn parse(sql: &str) -> SqlResult<Vec<Statement>> {
    Ok(parse_located(sql)?.into_iter().map(|(s, _)| s).collect())
}

/// [`parse`], with each statement's [`SourceRefs`]: where its column, relation, function,
/// operator and literal references start in `sql`, for [`SourceRefs::resolve`] to give binder
/// errors Postgres' `position`. Statements parsed from rewritten text get no references
/// (their offsets would not point into `sql`).
pub fn parse_located(sql: &str) -> SqlResult<Vec<(Statement, SourceRefs)>> {
    let original = sql;
    search_cycle::clear();
    FK_SET_COLUMNS.with(|c| c.borrow_mut().clear());
    VIEW_OPTIONS.with(|c| c.borrow_mut().clear());
    CTAS_COLUMNS.with(|c| c.borrow_mut().clear());
    TEMPORAL_KEYS.with(|c| c.borrow_mut().clear());
    CONFLICT_EXPRS.with(|c| c.borrow_mut().clear());
    if sql.trim().trim_matches(';').trim().is_empty() {
        return Ok(vec![(Statement::Empty, SourceRefs::default())]);
    }
    // SQL/JSON query functions are rewritten up front (sqlparser can't parse their clauses).
    let json_rewritten = sqljson::rewrite(sql);
    let sql = json_rewritten.as_deref().unwrap_or(sql);
    // So are CREATE TABLE's LIKE elements (sqlparser keeps one, without its position).
    let like_rewritten = rewrite_like(sql);
    let sql = like_rewritten.as_deref().unwrap_or(sql);
    let parsed = parse_statements(sql).map(|s| (s, sql.to_owned())).or_else(|err| match rewrite_unsupported(sql) {
        // Retry Postgres syntax sqlparser lacks after rewriting it to an equivalent form.
        Some(rewritten) => parse_statements(&rewritten).map(|s| (s, rewritten)).map_err(|_| err),
        None => Err(err),
    });
    match parsed {
        Ok((stmts, text)) => {
            let locate = text == original;
            PARSED_TEXT.with(|t| *t.borrow_mut() = text);
            PARSED_TOKENS.with(|t| *t.borrow_mut() = None);
            let lines = if locate { line_starts(original) } else { Vec::new() };
            let mut lowered = Vec::with_capacity(stmts.len());
            for p in stmts {
                REFS.with(|r| *r.borrow_mut() = locate.then(Vec::new));
                let stmt = lower_parsed(p);
                let refs = REFS.with(|r| r.borrow_mut().take()).unwrap_or_default();
                let mut stmt = stmt.map_err(|e| if locate { e } else { e.without_position() })?;
                // PREPARE and DECLARE remember the query string they came from, as Postgres does.
                match &mut stmt {
                    Statement::Prepare(p) => original.clone_into(&mut p.text),
                    Statement::DeclareCursor(d) => original.clone_into(&mut d.text),
                    _ => {}
                }
                let mut refs: Vec<(ErrorRef, u32)> = refs.into_iter().filter_map(|(r, at)| Some((r, char_position(&lines, at)?))).collect();
                refs.sort_by_key(|(_, p)| *p);
                refs.dedup();
                lowered.push((stmt, SourceRefs(refs)));
            }
            Ok(lowered)
        }
        Err(err) => {
            // Fall back to per-statement prefix recognition for the utility statements
            // sqlparser doesn't model; only when every piece is recognized do we accept it.
            let pieces: Vec<&str> = split_statements(sql);
            let fallback: Option<Vec<Statement>> = pieces.iter().map(|p| fallback_statement(p)).collect();
            match fallback {
                Some(stmts) => Ok(stmts.into_iter().map(|s| (s, SourceRefs::default())).collect()),
                None => {
                    let e = map_parser_error(&err, sql);
                    Err(if sql == original { e } else { e.without_position() })
                }
            }
        }
    }
}

/// The character offset at which each line of `sql` starts (line 1 at index 0).
fn line_starts(sql: &str) -> Vec<u32> {
    let mut starts = vec![0u32];
    for (n, c) in sql.chars().enumerate() {
        if c == '\n' {
            starts.push(u32::try_from(n + 1).unwrap_or(u32::MAX));
        }
    }
    starts
}

/// The 1-based character offset of a sqlparser location (1-based line and character column).
fn char_position(lines: &[u32], at: sqlparser::tokenizer::Location) -> Option<u32> {
    let line = usize::try_from(at.line).ok()?.checked_sub(1)?;
    let col = u32::try_from(at.column).ok().filter(|c| *c > 0)?;
    Some(lines.get(line)?.saturating_add(col))
}

/// Records where a reference an error may be about starts in the text being lowered (a no-op
/// when that text is not the query string as sent).
pub(crate) fn note_ref(r: ErrorRef, at: sqlparser::tokenizer::Location) {
    if at.line == 0 {
        return;
    }
    REFS.with(|refs| {
        if let Some(refs) = refs.borrow_mut().as_mut() {
            refs.push((r, at));
        }
    });
}

/// Notes `name` (lowered to `lowered`) as a reference of kind `kind` at its first part.
pub(crate) fn note_name(kind: fn(String) -> ErrorRef, name: &sq::ObjectName, lowered: &ObjectName) {
    if let Some(sq::ObjectNamePart::Identifier(first)) = name.0.first() {
        note_ref(kind(lowered.0.join(".")), first.span.start);
    }
}

/// Whether the statement being lowered records its references (see [`note_ref`]).
fn locating() -> bool {
    REFS.with(|r| r.borrow().is_some())
}

/// The position of `at` in the text being lowered, when it is the query string as sent.
fn position_in_parsed(at: sqlparser::tokenizer::Location) -> Option<u32> {
    if !locating() {
        return None;
    }
    PARSED_TEXT.with(|t| char_position(&line_starts(&t.borrow()), at))
}

/// The location of the first token at or after `after` in the text being lowered, when it is
/// spelled `text` (an operator between two operands).
pub(crate) fn token_location(after: sqlparser::tokenizer::Location, text: &str) -> Option<sqlparser::tokenizer::Location> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    if !locating() || after.line == 0 {
        return None;
    }
    PARSED_TOKENS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.is_none() {
            *cache = Some(PARSED_TEXT.with(|t| Tokenizer::new(&PostgreSqlDialect {}, &t.borrow()).tokenize_with_location()).ok()?);
        }
        cache
            .as_ref()?
            .iter()
            .filter(|t| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
            .find(|t| (t.span.start.line, t.span.start.column) >= (after.line, after.column))
            .filter(|t| t.token.to_string() == text)
            .map(|t| t.span.start)
    })
}

thread_local! {
    /// Trailing `WITH CHECK OPTION` / `WITH [NO] DATA` of CREATE VIEW statements, by view name:
    /// (check option, local check option, with data).
    static VIEW_OPTIONS: std::cell::RefCell<Vec<(String, bool, bool, bool)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The columns of `PRIMARY KEY / UNIQUE (..., c WITHOUT OVERLAPS)` keys, in statement order.
    static TEMPORAL_KEYS: std::cell::RefCell<Vec<Vec<String>>> = const { std::cell::RefCell::new(Vec::new()) };
    /// `CREATE TABLE name (a, b) AS ...` column name lists, by table name.
    static CTAS_COLUMNS: std::cell::RefCell<Vec<(String, Vec<String>)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// `ON DELETE SET NULL / SET DEFAULT [(cols)]` column lists in statement order (empty
    /// when there is none), stripped by the rewrite and taken by the lowering.
    static FK_SET_COLUMNS: std::cell::RefCell<std::collections::VecDeque<Vec<String>>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    /// Expression elements of `ON CONFLICT (...)` targets, by marker column number.
    static CONFLICT_EXPRS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The text the statements being lowered were parsed from (spans point into it).
    static PARSED_TEXT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// The references noted while lowering one statement (`None`: not recording).
    static REFS: std::cell::RefCell<Option<Vec<(ErrorRef, sqlparser::tokenizer::Location)>>> = const { std::cell::RefCell::new(None) };
    /// `PARSED_TEXT`'s tokens, for locating operators.
    static PARSED_TOKENS: std::cell::RefCell<Option<Vec<sqlparser::tokenizer::TokenWithSpan>>> = const { std::cell::RefCell::new(None) };
}

/// Postgres' `syntax error at or near "X"` for the first token at or after `at` in the text
/// being lowered (`at end of input` when there is none): for syntax sqlparser accepts but
/// Postgres' grammar rejects.
/// The options the rewrite stripped from CREATE VIEW `name`: (check option, with data).
pub(crate) fn take_view_options(name: &str) -> (bool, bool, bool) {
    VIEW_OPTIONS.with(|v| {
        let mut v = v.borrow_mut();
        match v.iter().position(|(n, _, _, _)| n == name) {
            Some(i) => {
                let (_, c, l, d) = v.remove(i);
                (c, l, d)
            }
            None => (false, false, true),
        }
    })
}

/// Marker column name prefix for a `LIKE` element among CREATE TABLE columns:
/// `"__pgmem_like source INCLUDING ..."` (see `rewrite_like`).
pub(crate) const LIKE_MARKER: &str = "__pgmem_like ";

/// `CREATE TABLE t (a int, LIKE s INCLUDING ALL, ...)`: each LIKE element becomes a marker
/// column `"__pgmem_like s INCLUDING ALL" int` the lowering turns back into a LIKE clause.
fn rewrite_like(sql: &str) -> Option<String> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    if !sql.to_ascii_lowercase().contains("like") {
        return None;
    }
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize().ok()?;
    let sig: Vec<usize> = tokens.iter().enumerate().filter(|(_, t)| !matches!(t, Token::Whitespace(_))).map(|(i, _)| i).collect();
    let mut replace: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut drop = vec![false; tokens.len()];
    let mut depth = 0usize;
    let mut in_create_table = false;
    for (k, &i) in sig.iter().enumerate() {
        match &tokens[i] {
            Token::SemiColon => in_create_table = false,
            Token::LParen => depth += 1,
            Token::RParen => depth = depth.saturating_sub(1),
            _ if is_word(&tokens, &sig, k, "CREATE") => {
                // CREATE [GLOBAL | LOCAL] [TEMP | TEMPORARY | UNLOGGED] TABLE
                in_create_table = (1..=3).any(|o| is_word(&tokens, &sig, k + o, "TABLE"));
            }
            _ if in_create_table
                && depth == 1
                && is_word(&tokens, &sig, k, "LIKE")
                && k > 0
                && matches!(tokens[sig[k - 1]], Token::LParen | Token::Comma) =>
            {
                let mut spec = String::new();
                let mut n = k + 1;
                while let Some(&j) = sig.get(n) {
                    match &tokens[j] {
                        Token::Comma | Token::RParen => break,
                        Token::Period => spec.push('.'),
                        Token::Word(w) => {
                            if !spec.is_empty() && !spec.ends_with('.') {
                                spec.push(' ');
                            }
                            spec.push_str(&if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() });
                        }
                        _ => return None,
                    }
                    drop[j] = true;
                    n += 1;
                }
                replace.insert(i, format!("\"{LIKE_MARKER}{}\" int", spec.replace('"', "\"\"")));
            }
            _ => {}
        }
    }
    if replace.is_empty() {
        return None;
    }
    Some(tokens.iter().enumerate().filter(|(i, _)| !drop[*i]).map(|(i, t)| replace.remove(&i).unwrap_or_else(|| t.to_string())).collect())
}

/// Whether the key over `columns` was written `WITHOUT OVERLAPS` (the record is consumed).
pub(crate) fn take_temporal_key(columns: &[String]) -> bool {
    TEMPORAL_KEYS.with(|t| {
        let mut t = t.borrow_mut();
        match t.iter().position(|c| c == columns) {
            Some(i) => {
                t.remove(i);
                true
            }
            None => false,
        }
    })
}

/// `PRIMARY KEY / UNIQUE (a, b WITHOUT OVERLAPS)`: records the key's columns and strips the
/// clause sqlparser lacks.
fn rewrite_without_overlaps(tokens: &Tokens, sig: &[usize], k: usize, drop: &mut [bool]) {
    use sqlparser::tokenizer::Token;
    if !(is_word(tokens, sig, k, "WITHOUT") && is_word(tokens, sig, k + 1, "OVERLAPS")) || k == 0 {
        return;
    }
    if !sig.get(k + 2).is_some_and(|&j| tokens[j] == Token::RParen) {
        return;
    }
    let Some(open) = sig[..k].iter().rposition(|&j| tokens[j] == Token::LParen) else { return };
    let mut cols = Vec::new();
    for &j in &sig[open + 1..k] {
        match &tokens[j] {
            Token::Word(w) => cols.push(if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() }),
            Token::Comma => {}
            _ => return,
        }
    }
    drop[sig[k]] = true;
    drop[sig[k + 1]] = true;
    TEMPORAL_KEYS.with(|t| t.borrow_mut().push(cols));
}

/// The column names the rewrite stripped from `CREATE TABLE name (a, b) AS`.
pub(crate) fn take_ctas_columns(name: &str) -> Vec<String> {
    CTAS_COLUMNS.with(|v| {
        let mut v = v.borrow_mut();
        v.iter().position(|(n, _)| n == name).map(|i| v.remove(i).1).unwrap_or_default()
    })
}

/// `CREATE TABLE name (a, b) AS query`: the name list is recorded and stripped.
fn rewrite_ctas_columns(tokens: &Tokens, sig: &[usize], k: usize, drop: &mut [bool]) {
    use sqlparser::tokenizer::Token;
    if tokens[sig[k]] != Token::LParen || k == 0 {
        return;
    }
    let Some(close) = matching_paren(tokens, sig, k) else { return };
    if !is_word(tokens, sig, close + 1, "AS") {
        return;
    }
    let start = sig[..k].iter().rposition(|&j| tokens[j] == Token::SemiColon).map_or(0, |p| p + 1);
    // The list follows the table name: `TABLE [IF NOT EXISTS] [schema.]name (`.
    let before_name = if k >= 2 && tokens[sig[k - 2]] == Token::Period { k.wrapping_sub(4) } else { k.wrapping_sub(2) };
    if !is_word(tokens, sig, start, "CREATE")
        || !(is_word(tokens, sig, before_name, "TABLE") || is_word(tokens, sig, before_name, "EXISTS"))
    {
        return;
    }
    let Token::Word(w) = &tokens[sig[k - 1]] else { return };
    let name = if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() };
    let mut cols = Vec::new();
    for (n, &j) in sig[k + 1..close].iter().enumerate() {
        match &tokens[j] {
            Token::Word(c) if n % 2 == 0 => cols.push(if c.quote_style.is_some() { c.value.clone() } else { c.value.to_lowercase() }),
            Token::Comma if n % 2 == 1 => {}
            _ => return,
        }
    }
    for &j in &sig[k..=close] {
        drop[j] = true;
    }
    CTAS_COLUMNS.with(|c| c.borrow_mut().push((name, cols)));
}

/// The next recorded `SET NULL / SET DEFAULT (cols)` list.
pub(crate) fn take_fk_set_columns() -> Vec<String> {
    FK_SET_COLUMNS.with(|c| c.borrow_mut().pop_front().unwrap_or_default())
}

pub(crate) fn syntax_error_at(at: sqlparser::tokenizer::Location) -> SqlError {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let next = PARSED_TEXT.with(|t| {
        let text = t.borrow();
        let tokens = Tokenizer::new(&PostgreSqlDialect {}, &text).tokenize_with_location().ok()?;
        tokens
            .into_iter()
            .filter(|t| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
            .find(|t| (t.span.start.line, t.span.start.column) >= (at.line, at.column))
            .map(|t| (t.token.to_string(), t.span.start))
    });
    match next {
        Some((tok, loc)) => {
            let e = SqlError::syntax(format!("syntax error at or near \"{tok}\""));
            match position_in_parsed(loc) {
                Some(p) => e.with_position(p),
                None => e,
            }
        }
        None => {
            let e = SqlError::syntax("syntax error at end of input");
            match locating().then(|| PARSED_TEXT.with(|t| u32::try_from(t.borrow().chars().count() + 1).ok())).flatten() {
                Some(p) => e.with_position(p),
                None => e,
            }
        }
    }
}

fn lower_parsed(p: Parsed) -> SqlResult<Statement> {
    match p {
        Parsed::Sql(s) => lower_statement(*s),
        Parsed::TypeDdl(t) => typeddl::lower(*t),
        Parsed::FuncDdl(f) => funcddl::lower(*f),
        Parsed::Role(r) => roles::lower(*r),
        Parsed::ExtDdl(e) => extddl::lower(*e),
        Parsed::Utility(u) => utility::lower(*u),
        Parsed::Schema(s, elements) => {
            let sq::Statement::CreateSchema { schema_name, if_not_exists, .. } = *s else {
                return Err(SqlError::internal("schema elements without CREATE SCHEMA"));
            };
            let elements = elements.into_iter().map(lower_parsed).collect::<SqlResult<Vec<_>>>()?;
            lower_create_schema(schema_name, if_not_exists, elements)
        }
    }
}

fn lower_create_schema(schema_name: sq::SchemaName, if_not_exists: bool, elements: Vec<Statement>) -> SqlResult<Statement> {
    let (name, authorization) = match schema_name {
        sq::SchemaName::Simple(n) => (Some(object_name(&n)?.base().to_owned()), None),
        sq::SchemaName::UnnamedAuthorization(a) => (None, Some(ident(&a))),
        sq::SchemaName::NamedAuthorization(n, a) => (Some(object_name(&n)?.base().to_owned()), Some(ident(&a))),
    };
    Ok(Statement::CreateSchema(Box::new(pgmem_ir::CreateSchema { name, if_not_exists, authorization, elements })))
}

/// The function name `(x).*` is rewritten to before parsing; the select-list lowering turns
/// it back into a field expansion.
pub(crate) const FIELD_STAR_MARKER: &str = "__pgmem_field_star";

/// The pseudo-function `RETURNING WITH (OLD AS o, NEW AS n)` becomes, as the first RETURNING
/// item: `__pgmem_returning_aliases('o', 'n')`.
pub(crate) const RETURNING_ALIASES_MARKER: &str = "__pgmem_returning_aliases";

/// A window frame's EXCLUDE clause, as the first PARTITION BY item: `__pgmem_frame_exclude('ties')`.
pub(crate) const FRAME_EXCLUDE_MARKER: &str = "__pgmem_frame_exclude";

/// The OF-list markers of `FOR NO KEY UPDATE` and `FOR KEY SHARE`.
pub(crate) const NO_KEY_UPDATE_MARKER: &str = "__pgmem_lock_no_key_update";
pub(crate) const KEY_SHARE_MARKER: &str = "__pgmem_lock_key_share";

/// `WHERE CURRENT OF c` is rewritten to `WHERE __pgmem_current_of('c', ctid, tableoid)`.
pub(crate) const CURRENT_OF_FN: &str = "__pgmem_current_of";

/// `SEARCH BREADTH FIRST`'s depth: field 0 of the search record.
pub(crate) const SEARCH_DEPTH_FN: &str = "__pgmem_search_depth";

/// The function `BETWEEN SYMMETRIC`'s low bound is wrapped in before parsing.
pub(crate) const SYMMETRIC_MARKER: &str = "__pgmem_symmetric";

enum Parsed {
    Sql(Box<sq::Statement>),
    TypeDdl(Box<typeddl::TypeDdl>),
    FuncDdl(Box<funcddl::FuncDdl>),
    Role(Box<roles::RoleStmt>),
    ExtDdl(Box<extddl::ExtDdl>),
    Utility(Box<utility::Utility>),
    /// `CREATE SCHEMA ...` followed by its schema elements.
    Schema(Box<sq::Statement>, Vec<Parsed>),
}

/// `Parser::parse_statements`, with the statements sqlparser models incompletely (domains,
/// `ALTER TYPE`) parsed by [`typeddl`].
fn parse_statements(sql: &str) -> Result<Vec<Parsed>, ParserError> {
    use sqlparser::tokenizer::Token;
    let dialect = PostgreSqlDialect {};
    let mut p = Parser::new(&dialect).try_with_sql(sql)?;
    let mut out = Vec::new();
    let mut expecting_delimiter = false;
    loop {
        while p.consume_token(&Token::SemiColon) {
            expecting_delimiter = false;
        }
        if p.peek_token().token == Token::EOF {
            break;
        }
        if expecting_delimiter {
            return p.expected("end of statement", p.peek_token());
        }
        if let Some(u) = utility::try_parse(&mut p)? {
            out.push(Parsed::Utility(Box::new(u)));
            expecting_delimiter = true;
            continue;
        }
        if let Some(f) = funcddl::try_parse(&mut p)? {
            out.push(Parsed::FuncDdl(Box::new(f)));
            expecting_delimiter = true;
            continue;
        }
        if let Some(e) = extddl::try_parse(&mut p)? {
            out.push(Parsed::ExtDdl(Box::new(e)));
            expecting_delimiter = true;
            continue;
        }
        if let Some(r) = roles::try_parse(&mut p)? {
            out.push(Parsed::Role(Box::new(r)));
            expecting_delimiter = true;
            continue;
        }
        match typeddl::try_parse(&mut p)? {
            Some(t) => out.push(Parsed::TypeDdl(Box::new(t))),
            None => {
                let stmt = p.parse_statement()?;
                if matches!(stmt, sq::Statement::CreateSchema { .. }) {
                    // Schema elements follow without a delimiter (CREATE TABLE/VIEW/INDEX/
                    // SEQUENCE/TRIGGER, GRANT).
                    let mut elements = Vec::new();
                    while matches!(&p.peek_token().token, Token::Word(w) if w.value.eq_ignore_ascii_case("CREATE") || w.value.eq_ignore_ascii_case("GRANT"))
                    {
                        match typeddl::try_parse(&mut p)? {
                            Some(t) => elements.push(Parsed::TypeDdl(Box::new(t))),
                            None => elements.push(Parsed::Sql(Box::new(p.parse_statement()?))),
                        }
                    }
                    out.push(Parsed::Schema(Box::new(stmt), elements));
                } else {
                    out.push(Parsed::Sql(Box::new(stmt)));
                }
            }
        }
        expecting_delimiter = true;
    }
    Ok(out)
}

/// Parses exactly one statement (the extended-query `Parse` message). Postgres rejects
/// multiple commands in a prepared statement with 42601.
pub fn parse_one(sql: &str) -> SqlResult<Statement> {
    Ok(parse_one_located(sql)?.0)
}

/// [`parse_one`], with the statement's [`SourceRefs`] (see [`parse_located`]).
pub fn parse_one_located(sql: &str) -> SqlResult<(Statement, SourceRefs)> {
    let mut stmts = parse_located(sql)?;
    if stmts.len() > 1 {
        return Err(SqlError::syntax("cannot insert multiple commands into a prepared statement"));
    }
    Ok(stmts.pop().unwrap_or((Statement::Empty, SourceRefs::default())))
}

/// Rewrites Postgres syntax that sqlparser does not accept into equivalent syntax it does,
/// for constructs pgmem treats identically: `FOR NO KEY UPDATE` / `FOR KEY SHARE` row locks
/// (row locks are no-ops), `DROP INDEX CONCURRENTLY` / `REINDEX`-style modifiers, `AT LOCAL`
/// (`AT TIME ZONE current_setting('TimeZone')`, as Postgres defines it), the `TABLE t`
/// statement (`SELECT * FROM t`) and `BETWEEN [A]SYMMETRIC` (the low bound wrapped in
/// [`SYMMETRIC_MARKER`]). Works on tokens, so string literals and identifiers are never
/// touched. Returns None if nothing changed.
/// Wraps the argument of `VARIADIC` in function calls (sqlparser lacks the keyword).
pub(crate) const VARIADIC_MARKER: &str = "__pgmem_variadic";

/// The index (into `sig`) of the parenthesis closing the one at `sig[k]`.
fn matching_paren(tokens: &[sqlparser::tokenizer::Token], sig: &[usize], k: usize) -> Option<usize> {
    use sqlparser::tokenizer::Token;
    let mut depth = 0usize;
    for (n, &j) in sig.iter().enumerate().skip(k) {
        match tokens[j] {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(n);
                }
            }
            _ => {}
        }
    }
    None
}

type Tokens = [sqlparser::tokenizer::Token];

fn is_word(tokens: &Tokens, sig: &[usize], k: usize, w: &str) -> bool {
    use sqlparser::tokenizer::Token;
    sig.get(k).is_some_and(|&j| matches!(&tokens[j], Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w)))
}

/// `RETURNING WITH (OLD AS o, NEW AS n) items` → `RETURNING __pgmem_returning_aliases('o', 'n'), items`.
fn rewrite_returning_with(
    tokens: &Tokens,
    sig: &[usize],
    k: usize,
    drop: &mut [bool],
    insert_after: &mut std::collections::HashMap<usize, String>,
) -> bool {
    use sqlparser::tokenizer::Token;
    if !(is_word(tokens, sig, k, "RETURNING")
        && is_word(tokens, sig, k + 1, "WITH")
        && sig.get(k + 2).is_some_and(|&j| tokens[j] == Token::LParen))
    {
        return false;
    }
    let Some(close) = matching_paren(tokens, sig, k + 2) else { return false };
    let (mut old, mut new) = ("old".to_owned(), "new".to_owned());
    for w in sig[k + 3..close].split(|&j| tokens[j] == Token::Comma) {
        if let [which, as_, name] = w
            && matches!(&tokens[*as_], Token::Word(x) if x.value.eq_ignore_ascii_case("AS"))
            && let Token::Word(n) = &tokens[*name]
        {
            let n = if n.quote_style.is_some() { n.value.clone() } else { n.value.to_lowercase() };
            match &tokens[*which] {
                Token::Word(x) if x.value.eq_ignore_ascii_case("OLD") => old = n,
                Token::Word(x) if x.value.eq_ignore_ascii_case("NEW") => new = n,
                _ => {}
            }
        }
    }
    insert_after.insert(sig[k], format!(" {RETURNING_ALIASES_MARKER}('{}', '{}'),", old.replace('\'', "''"), new.replace('\'', "''")));
    for &j in &sig[k + 1..=close] {
        drop[j] = true;
    }
    true
}

/// Puts a marker locking clause `FOR SHARE OF marker` before the one starting at `sig[k]`
/// (its FOR): the lowering gives the next clause the marker's strength.
fn lock_marker(insert_after: &mut std::collections::HashMap<usize, String>, sig: &[usize], k: usize, marker: &str) {
    if k > 0 {
        insert_after.insert(sig[k - 1], format!(" FOR SHARE OF {marker}"));
    }
}

/// CREATE [OR REPLACE] [MATERIALIZED] VIEW name ... `WITH [CASCADED | LOCAL] CHECK OPTION` /
/// `WITH [NO] DATA` at the end of the statement: recorded by view name and stripped.
fn rewrite_view_options(tokens: &Tokens, sig: &[usize], k: usize, drop: &mut [bool]) {
    use sqlparser::tokenizer::Token;
    if !is_word(tokens, sig, k, "WITH") {
        return;
    }
    let end = |o: usize| sig.get(k + o).is_none_or(|&j| matches!(tokens[j], Token::SemiColon | Token::EOF));
    let (len, check, local, data) = if is_word(tokens, sig, k + 1, "CHECK") && is_word(tokens, sig, k + 2, "OPTION") && end(3) {
        (3, true, false, true)
    } else if (is_word(tokens, sig, k + 1, "CASCADED") || is_word(tokens, sig, k + 1, "LOCAL"))
        && is_word(tokens, sig, k + 2, "CHECK")
        && is_word(tokens, sig, k + 3, "OPTION")
        && end(4)
    {
        (4, true, is_word(tokens, sig, k + 1, "LOCAL"), true)
    } else if is_word(tokens, sig, k + 1, "DATA") && end(2) {
        (2, false, false, true)
    } else if is_word(tokens, sig, k + 1, "NO") && is_word(tokens, sig, k + 2, "DATA") && end(3) {
        (3, false, false, false)
    } else {
        return;
    };
    // The statement's CREATE ... VIEW name.
    let start = sig[..k].iter().rposition(|&j| tokens[j] == Token::SemiColon).map_or(0, |p| p + 1);
    let Some(v) = (start..k).find(|&n| is_word(tokens, sig, n, "VIEW")) else { return };
    if !is_word(tokens, sig, start, "CREATE") {
        return;
    }
    let Some(Token::Word(w)) = sig.get(v + 1).map(|&j| &tokens[j]) else { return };
    let mut name = if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() };
    // A qualified name: the last part.
    let mut n = v + 2;
    while sig.get(n).is_some_and(|&j| tokens[j] == Token::Period) {
        if let Some(Token::Word(w)) = sig.get(n + 1).map(|&j| &tokens[j]) {
            name = if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() };
        }
        n += 2;
    }
    for o in 0..len {
        drop[sig[k + o]] = true;
    }
    VIEW_OPTIONS.with(|c| c.borrow_mut().push((name, check, local, data)));
}

/// `ON DELETE SET NULL (cols)` / `SET DEFAULT (cols)`: records every such action's column list
/// (empty without one) and strips the list sqlparser lacks.
fn rewrite_fk_set_columns(tokens: &Tokens, sig: &[usize], k: usize, drop: &mut [bool]) {
    use sqlparser::tokenizer::Token;
    if !(is_word(tokens, sig, k, "ON")
        && is_word(tokens, sig, k + 1, "DELETE")
        && is_word(tokens, sig, k + 2, "SET")
        && (is_word(tokens, sig, k + 3, "NULL") || is_word(tokens, sig, k + 3, "DEFAULT")))
    {
        return;
    }
    let mut cols = Vec::new();
    if sig.get(k + 4).is_some_and(|&j| tokens[j] == Token::LParen)
        && let Some(close) = matching_paren(tokens, sig, k + 4)
    {
        for &j in &sig[k + 4..=close] {
            if let Token::Word(w) = &tokens[j] {
                cols.push(if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() });
            }
            drop[j] = true;
        }
    }
    FK_SET_COLUMNS.with(|c| c.borrow_mut().push_back(cols));
}

/// Marker column standing for an expression in an ON CONFLICT target (`rewrite_conflict_exprs`).
pub(crate) const CONFLICT_EXPR_MARKER: &str = "__pgmem_conflict_expr_";

/// The expression text a conflict-target marker column stands for.
pub(crate) fn conflict_expr(column: &str) -> Option<String> {
    let n: usize = column.strip_prefix(CONFLICT_EXPR_MARKER)?.parse().ok()?;
    CONFLICT_EXPRS.with(|c| c.borrow().get(n).cloned())
}

/// `ON CONFLICT (lower(email), id)`: each element that is not a plain column becomes a marker
/// column (sqlparser takes only column names); the lowering parses the recorded text.
fn rewrite_conflict_exprs(
    tokens: &Tokens,
    sig: &[usize],
    k: usize,
    drop: &mut [bool],
    insert_after: &mut std::collections::HashMap<usize, String>,
) {
    use sqlparser::tokenizer::Token;
    if !(is_word(tokens, sig, k, "ON")
        && is_word(tokens, sig, k + 1, "CONFLICT")
        && sig.get(k + 2).is_some_and(|&j| tokens[j] == Token::LParen))
    {
        return;
    }
    let Some(close) = matching_paren(tokens, sig, k + 2) else { return };
    // Elements: runs of significant tokens between top-level commas.
    let mut elements: Vec<Vec<usize>> = vec![Vec::new()];
    let mut depth = 0usize;
    for n in k + 3..close {
        match tokens[sig[n]] {
            Token::Comma if depth == 0 => elements.push(Vec::new()),
            Token::LParen => {
                depth += 1;
                elements.last_mut().unwrap_or_else(|| unreachable!()).push(n);
            }
            Token::RParen => {
                depth = depth.saturating_sub(1);
                elements.last_mut().unwrap_or_else(|| unreachable!()).push(n);
            }
            _ => elements.last_mut().unwrap_or_else(|| unreachable!()).push(n),
        }
    }
    for el in elements {
        if el.len() <= 1 {
            continue;
        }
        let text: String = (sig[el[0]]..=sig[el[el.len() - 1]]).map(|j| tokens[j].to_string()).collect();
        let n = CONFLICT_EXPRS.with(|c| {
            let mut c = c.borrow_mut();
            c.push(text);
            c.len() - 1
        });
        for d in &mut drop[sig[el[0]]..=sig[el[el.len() - 1]]] {
            *d = true;
        }
        insert_after.insert(sig[el[0] - 1], format!(" {CONFLICT_EXPR_MARKER}{n}"));
    }
}

/// `WHERE CURRENT OF cursor` → `WHERE __pgmem_current_of('cursor', ctid, tableoid)`.
fn rewrite_current_of(
    tokens: &Tokens,
    sig: &[usize],
    k: usize,
    drop: &mut [bool],
    insert_after: &mut std::collections::HashMap<usize, String>,
) {
    use sqlparser::tokenizer::Token;
    if !(is_word(tokens, sig, k, "WHERE") && is_word(tokens, sig, k + 1, "CURRENT") && is_word(tokens, sig, k + 2, "OF")) {
        return;
    }
    let Some(Token::Word(name)) = sig.get(k + 3).map(|&j| &tokens[j]) else { return };
    let name = if name.quote_style.is_some() { name.value.clone() } else { name.value.to_lowercase() };
    for n in 1..=3 {
        drop[sig[k + n]] = true;
    }
    insert_after.insert(sig[k], format!(" {CURRENT_OF_FN}('{}', ctid, tableoid)", name.replace('\'', "''")));
}

/// `WITH RECURSIVE name AS (...) SEARCH ... / CYCLE ...` (recorded, then stripped).
fn rewrite_search_cycle(tokens: &Tokens, sig: &[usize], k: usize, drop: &mut [bool]) -> bool {
    use sqlparser::tokenizer::Token;
    if !(is_word(tokens, sig, k, "SEARCH") || is_word(tokens, sig, k, "CYCLE"))
        || k == 0
        || !(tokens[sig[k - 1]] == Token::RParen || drop[sig[k - 1]])
    {
        return false;
    }
    let Some(name) = cte_name_before(tokens, sig, k) else { return false };
    let Some(len) = search_cycle::strip(tokens, sig, k, &name) else { return false };
    for &j in &sig[k..k + len] {
        drop[j] = true;
    }
    true
}

/// A window frame's `EXCLUDE CURRENT ROW | GROUP | TIES | NO OTHERS` becomes a marker first in
/// the window's PARTITION BY: `__pgmem_frame_exclude('current row')`.
fn rewrite_frame_exclude(
    tokens: &Tokens,
    sig: &[usize],
    k: usize,
    drop: &mut [bool],
    insert_after: &mut std::collections::HashMap<usize, String>,
) {
    use sqlparser::tokenizer::Token;
    let at = |o: usize, w: &str| is_word(tokens, sig, k + o, w);
    if !at(0, "EXCLUDE") {
        return;
    }
    let kind = if at(1, "CURRENT") && at(2, "ROW") {
        ("current row", 2)
    } else if at(1, "GROUP") {
        ("group", 1)
    } else if at(1, "TIES") {
        ("ties", 1)
    } else if at(1, "NO") && at(2, "OTHERS") {
        ("no others", 2)
    } else {
        return;
    };
    let mut depth = 0usize;
    let Some(open) = sig[..k].iter().rposition(|&j| match tokens[j] {
        Token::RParen => {
            depth += 1;
            false
        }
        Token::LParen if depth > 0 => {
            depth -= 1;
            false
        }
        Token::LParen => true,
        _ => false,
    }) else {
        return;
    };
    for n in 0..=kind.1 {
        drop[sig[k + n]] = true;
    }
    let marker = format!("{FRAME_EXCLUDE_MARKER}('{}')", kind.0);
    if is_word(tokens, sig, open + 1, "PARTITION") && is_word(tokens, sig, open + 2, "BY") {
        insert_after.insert(sig[open + 2], format!(" {marker},"));
    } else if sig.get(open + 1).is_some_and(|&j| matches!(&tokens[j], Token::Word(_)))
        && !["ORDER", "ROWS", "RANGE", "GROUPS"].iter().any(|w| is_word(tokens, sig, open + 1, w))
    {
        // `OVER (base_window ...)`.
        insert_after.insert(sig[open + 1], format!(" PARTITION BY {marker}"));
    } else {
        insert_after.insert(sig[open], format!("PARTITION BY {marker} "));
    }
}

/// The name of the CTE whose body ends just before `sig[k]` (skipping a SEARCH clause
/// already stripped there): `name [(cols)] AS [[NOT] MATERIALIZED] (body)`.
fn cte_name_before(tokens: &[sqlparser::tokenizer::Token], sig: &[usize], k: usize) -> Option<String> {
    use sqlparser::tokenizer::Token;
    let word = |j: usize, w: &str| matches!(&tokens[j], Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    // Back over a stripped SEARCH clause to the body's closing parenthesis.
    let mut close = k - 1;
    while close > 0 && tokens[sig[close]] != Token::RParen {
        close -= 1;
    }
    let mut depth = 0usize;
    let mut open = close;
    loop {
        match tokens[sig[open]] {
            Token::RParen => depth += 1,
            Token::LParen => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
        open = open.checked_sub(1)?;
    }
    let mut j = open.checked_sub(1)?;
    while word(sig[j], "MATERIALIZED") || word(sig[j], "NOT") {
        j = j.checked_sub(1)?;
    }
    if !word(sig[j], "AS") {
        return None;
    }
    j = j.checked_sub(1)?;
    if tokens[sig[j]] == Token::RParen {
        while tokens[sig[j]] != Token::LParen {
            j = j.checked_sub(1)?;
        }
        j = j.checked_sub(1)?;
    }
    match &tokens[sig[j]] {
        Token::Word(w) if w.quote_style.is_some() => Some(w.value.clone()),
        Token::Word(w) => Some(w.value.to_lowercase()),
        _ => None,
    }
}

fn rewrite_unsupported(sql: &str) -> Option<String> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let assigned = rewrite_array_assignments(sql);
    let sql = assigned.as_deref().unwrap_or(sql);
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize().ok()?;
    let word = |t: &Token, w: &str| matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    // Indexes of significant (non-whitespace) tokens.
    let sig: Vec<usize> = tokens.iter().enumerate().filter(|(_, t)| !matches!(t, Token::Whitespace(_))).map(|(i, _)| i).collect();
    let mut drop = vec![false; tokens.len()];
    // Text inserted after a token.
    let mut insert_after: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut replace: Vec<Option<&str>> = vec![None; tokens.len()];
    for (k, &i) in sig.iter().enumerate() {
        // `(expr).*` → `__pgmem_field_star((expr))`.
        if tokens[i] == Token::RParen
            && sig.get(k + 1).is_some_and(|&j| tokens[j] == Token::Period)
            && sig.get(k + 2).is_some_and(|&j| tokens[j] == Token::Mul)
        {
            let mut depth = 0usize;
            let open = sig[..=k].iter().rev().find(|&&j| {
                match tokens[j] {
                    Token::RParen => depth += 1,
                    Token::LParen => depth -= 1,
                    _ => {}
                }
                depth == 0
            });
            if let Some(&open) = open {
                replace[open] = Some("__pgmem_field_star((");
                replace[i] = Some("))");
                drop[sig[k + 1]] = true;
                drop[sig[k + 2]] = true;
            }
            continue;
        }
        let at = |o: usize, w: &str| sig.get(k + o).is_some_and(|&j| word(&tokens[j], w));
        // `substring(x SIMILAR p ESCAPE e)` → `substring(x, p, e)`.
        if word(&tokens[i], "SUBSTRING")
            && sig.get(k + 1).is_some_and(|&j| tokens[j] == Token::LParen)
            && let Some(close) = matching_paren(&tokens, &sig, k + 1)
        {
            let mut depth = 0usize;
            let inner: Vec<usize> = sig[k + 2..close].to_vec();
            let similar = inner.iter().any(|&j| {
                match tokens[j] {
                    Token::LParen => depth += 1,
                    Token::RParen => depth = depth.saturating_sub(1),
                    _ => {}
                }
                depth == 0 && word(&tokens[j], "SIMILAR")
            });
            if similar {
                let mut depth = 0usize;
                for &j in &inner {
                    match tokens[j] {
                        Token::LParen => depth += 1,
                        Token::RParen => depth = depth.saturating_sub(1),
                        _ if depth == 0 && (word(&tokens[j], "SIMILAR") || word(&tokens[j], "ESCAPE")) => replace[j] = Some(","),
                        _ => {}
                    }
                }
            }
        }
        // `f(a, VARIADIC arr)` → `f(a, __pgmem_variadic(arr))`.
        if word(&tokens[i], "VARIADIC")
            && k > 0
            && matches!(tokens[sig[k - 1]], Token::LParen | Token::Comma)
            && let Some(open) = {
                // The call's opening parenthesis: the nearest unmatched one before.
                let mut depth = 0usize;
                sig[..k].iter().rposition(|&j| match tokens[j] {
                    Token::RParen => {
                        depth += 1;
                        false
                    }
                    Token::LParen if depth > 0 => {
                        depth -= 1;
                        false
                    }
                    Token::LParen => true,
                    _ => false,
                })
            }
            && let Some(close) = matching_paren(&tokens, &sig, open)
            && close > k
        {
            replace[i] = Some("__pgmem_variadic(");
            replace[sig[close]] = Some("))");
        }
        if rewrite_returning_with(&tokens, &sig, k, &mut drop, &mut insert_after) || rewrite_search_cycle(&tokens, &sig, k, &mut drop) {
            continue;
        }
        rewrite_frame_exclude(&tokens, &sig, k, &mut drop, &mut insert_after);
        rewrite_current_of(&tokens, &sig, k, &mut drop, &mut insert_after);
        rewrite_fk_set_columns(&tokens, &sig, k, &mut drop);
        rewrite_conflict_exprs(&tokens, &sig, k, &mut drop, &mut insert_after);
        rewrite_view_options(&tokens, &sig, k, &mut drop);
        rewrite_ctas_columns(&tokens, &sig, k, &mut drop);
        rewrite_without_overlaps(&tokens, &sig, k, &mut drop);
        // MERGE's `THEN INSERT DEFAULT VALUES` → `THEN INSERT VALUES (__pgmem_default_values())`.
        if word(&tokens[i], "THEN") && at(1, "INSERT") && at(2, "DEFAULT") && at(3, "VALUES") {
            drop[sig[k + 2]] = true;
            replace[sig[k + 3]] = Some("VALUES (__pgmem_default_values())");
        }
        // `FOR NO KEY UPDATE` / `FOR KEY SHARE` → `FOR UPDATE` / `FOR SHARE` with a marker
        // table first in OF (sqlparser has neither strength).
        if word(&tokens[i], "FOR") && at(1, "NO") && at(2, "KEY") && at(3, "UPDATE") {
            drop[sig[k + 1]] = true;
            drop[sig[k + 2]] = true;
            lock_marker(&mut insert_after, &sig, k, NO_KEY_UPDATE_MARKER);
        } else if word(&tokens[i], "FOR") && at(1, "KEY") && at(2, "SHARE") {
            drop[sig[k + 1]] = true;
            lock_marker(&mut insert_after, &sig, k, KEY_SHARE_MARKER);
        } else if word(&tokens[i], "GENERATED")
            && at(1, "ALWAYS")
            && at(2, "AS")
            && sig.get(k + 3).is_some_and(|&j| tokens[j] == Token::LParen)
            && let Some(close) = matching_paren(&tokens, &sig, k + 3)
        {
            // `GENERATED ALWAYS AS (expr) [VIRTUAL]` (Postgres 18's default): computed like
            // STORED, which gives the same values.
            if sig.get(close + 1).is_some_and(|&j| word(&tokens[j], "VIRTUAL")) {
                replace[sig[close + 1]] = Some("STORED");
            } else if !sig.get(close + 1).is_some_and(|&j| word(&tokens[j], "STORED")) {
                insert_after.insert(sig[close], " STORED".to_owned());
            }
        } else if word(&tokens[i], "DROP") && at(1, "INDEX") && at(2, "CONCURRENTLY") {
            drop[sig[k + 2]] = true;
        } else if word(&tokens[i], "AT") && at(1, "LOCAL") {
            replace[sig[k + 1]] = Some("TIME ZONE current_setting('TimeZone')");
        } else if word(&tokens[i], "TABLE") && (k == 0 || tokens[sig[k - 1]] == Token::SemiColon) {
            // `TABLE t` as a statement is `SELECT * FROM t`.
            replace[i] = Some("SELECT * FROM");
        } else if word(&tokens[i], "BETWEEN") && (at(1, "SYMMETRIC") || at(1, "ASYMMETRIC")) {
            // `BETWEEN SYMMETRIC lo AND hi` → `BETWEEN __pgmem_symmetric(lo) AND hi`: the low
            // bound is a b_expr, so it ends at the first AND outside parentheses.
            let symmetric = at(1, "SYMMETRIC");
            let mut depth = 0i32;
            let and = sig[k + 2..].iter().copied().find(|&j| {
                match tokens[j] {
                    Token::LParen | Token::LBracket => depth += 1,
                    Token::RParen | Token::RBracket => depth -= 1,
                    _ => {}
                }
                depth == 0 && word(&tokens[j], "AND")
            });
            if let Some(and) = and {
                if symmetric {
                    replace[sig[k + 1]] = Some("__pgmem_symmetric(");
                    replace[and] = Some(") AND");
                } else {
                    drop[sig[k + 1]] = true;
                }
            }
        }
    }
    if !drop.contains(&true) && replace.iter().all(Option::is_none) && insert_after.is_empty() {
        return assigned;
    }
    Some(
        tokens
            .iter()
            .zip(drop)
            .zip(replace)
            .enumerate()
            .filter(|(_, ((_, d), _))| !d)
            .map(|(i, ((t, _), r))| {
                let mut text = r.map_or_else(|| t.to_string(), str::to_owned);
                if let Some(extra) = insert_after.get(&i) {
                    text.push_str(extra);
                }
                text
            })
            .collect(),
    )
}

/// `UPDATE t SET col[i] = v, col[lo:hi][...] = w`: subscripted assignment targets become
/// `col = __pgmem_array_set(col, (v), 'flags', bounds...)`, where each flag is `i` (an index,
/// one bound follows) or `r` (a range, lower and upper follow; NULL for an omitted bound).
fn rewrite_array_assignments(sql: &str) -> Option<String> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize().ok()?;
    let word = |t: &Token, w: &str| matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w));
    let next_sig = |from: usize| (from..tokens.len()).find(|&j| !matches!(tokens[j], Token::Whitespace(_)));
    let mut out = String::new();
    let mut in_set = false;
    let mut changed = false;
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if word(t, "SET") {
            in_set = true;
        } else if word(t, "WHERE") || word(t, "FROM") || word(t, "RETURNING") || *t == Token::SemiColon {
            in_set = false;
        }
        let starts_target =
            in_set && matches!(t, Token::Word(_)) && !word(t, "SET") && next_sig(i + 1).is_some_and(|j| tokens[j] == Token::LBracket);
        if !starts_target {
            out.push_str(&t.to_string());
            i += 1;
            continue;
        }
        // Subscripts: [a] or [a:b] (either side optional), possibly several.
        let col = t.to_string();
        let mut j = next_sig(i + 1)?;
        let mut flags = String::new();
        let mut bounds: Vec<String> = Vec::new();
        while tokens.get(j) == Some(&Token::LBracket) {
            let mut depth = 0usize;
            let mut k = j + 1;
            let mut colon = None;
            while k < tokens.len() {
                match &tokens[k] {
                    Token::LBracket | Token::LParen => depth += 1,
                    Token::RBracket if depth == 0 => break,
                    Token::RBracket | Token::RParen => depth -= 1,
                    Token::Colon if depth == 0 => colon = Some(k),
                    _ => {}
                }
                k += 1;
            }
            let text = |a: usize, b: usize| -> String {
                let s: String = tokens[a..b].iter().map(Token::to_string).collect();
                if s.trim().is_empty() { "NULL".to_owned() } else { format!("({})", s.trim()) }
            };
            match colon {
                Some(c) => {
                    flags.push('r');
                    bounds.push(text(j + 1, c));
                    bounds.push(text(c + 1, k));
                }
                None => {
                    flags.push('i');
                    bounds.push(text(j + 1, k));
                }
            }
            j = next_sig(k + 1)?;
        }
        if tokens.get(j) != Some(&Token::Eq) {
            out.push_str(&t.to_string());
            i += 1;
            continue;
        }
        // The value: up to a top-level comma or the end of the SET list.
        let start = j + 1;
        let mut depth = 0usize;
        let mut k = start;
        while k < tokens.len() {
            match &tokens[k] {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
                Token::Comma if depth == 0 => break,
                Token::SemiColon => break,
                t if depth == 0 && (word(t, "WHERE") || word(t, "FROM") || word(t, "RETURNING")) => break,
                _ => {}
            }
            k += 1;
        }
        let value: String = tokens[start..k].iter().map(Token::to_string).collect();
        let func = if flags.contains('r') { "__pgmem_array_set_slice" } else { "__pgmem_array_set" };
        out.push_str(&format!("{col} = {func}({col}, ({}), '{flags}', {})", value.trim(), bounds.join(", ")));
        out.push(' ');
        changed = true;
        i = k;
    }
    changed.then_some(out)
}

/// Splits on top-level semicolons, respecting quotes and dollar quotes. Only used by the
/// fallback path, so it doesn't need to be exhaustive.
fn split_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'\'' || c == b'"' => quote = Some(c),
            None if c == b';' => {
                out.push(&sql[start..i]);
                start = i + 1;
            }
            None => {}
        }
        i += 1;
    }
    out.push(&sql[start..]);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Prefix recognition for utility statements sqlparser can't parse.
fn fallback_statement(sql: &str) -> Option<Statement> {
    let upper = sql.trim().trim_end_matches(';').trim().to_ascii_uppercase();
    let words: Vec<&str> = upper.split_whitespace().collect();
    let first = *words.first()?;
    let second = words.get(1).copied().unwrap_or("");
    let tag = match (first, second) {
        ("SET", _) => "SET",
        ("RESET", _) => "RESET",
        ("DISCARD", "ALL") => "DISCARD ALL",
        ("DISCARD", _) => "DISCARD",
        ("COMMENT", "ON") => "COMMENT",
        ("CREATE", "EXTENSION") => "CREATE EXTENSION",
        ("DROP", "EXTENSION") => "DROP EXTENSION",
        ("ANALYZE" | "ANALYSE", _) => "ANALYZE",
        ("VACUUM", _) => "VACUUM",
        ("GRANT", _) => "GRANT",
        ("REVOKE", _) => "REVOKE",
        ("ALTER", "DEFAULT") => "ALTER DEFAULT PRIVILEGES",
        ("CHECKPOINT", _) => "CHECKPOINT",
        _ => return None,
    };
    Some(Statement::NoOp { tag: tag.to_owned() })
}

/// Turns a sqlparser error into Postgres' `syntax error at or near "X"` (42601).
fn map_parser_error(err: &ParserError, sql: &str) -> SqlError {
    let text = err.to_string();
    if let ParserError::RecursionLimitExceeded = err {
        return SqlError::new("54001", "stack depth limit exceeded");
    }
    if let Some(e) = lexer_error(&text, sql) {
        return e;
    }
    // sqlparser's own MERGE checks: `UPDATE is not allowed in a NOT MATCHED merge clause`.
    if let Some((what, _)) = text.split_once(" is not allowed in a ")
        && let Some(word) = what.rsplit(' ').next()
    {
        return SqlError::syntax(format!("syntax error at or near \"{word}\"")).with_detail(text);
    }
    // `CHECK (...) DEFERRABLE` as a column constraint.
    if text.contains("found: DEFERRABLE") || text.contains("found: INITIALLY") {
        let what = if text.contains("found: DEFERRABLE") { "DEFERRABLE" } else { "INITIALLY" };
        return SqlError::syntax(format!("misplaced {what} clause")).with_detail(text);
    }
    let found = text.split("found: ").nth(1).map(|rest| rest.split(" at Line:").next().unwrap_or(rest).trim().to_owned());
    let lines = line_starts(sql);
    let at = error_location(&text).and_then(|at| char_position(&lines, at));
    let end = u32::try_from(sql.chars().count() + 1).ok();
    let (message, position) = match found.as_deref() {
        Some("EOF") | None if text.contains("EOF") => ("syntax error at end of input".to_owned(), end),
        // `... ORDER` / `... GROUP` without BY: Postgres fails at the token after the keyword.
        Some(tok) if (tok.eq_ignore_ascii_case("ORDER") || tok.eq_ignore_ascii_case("GROUP")) && text.contains("end of statement") => {
            match error_location(&text).and_then(|at| token_after(sql, at)) {
                Some((next, loc)) if !next.eq_ignore_ascii_case("BY") => {
                    (format!("syntax error at or near \"{next}\""), char_position(&lines, loc))
                }
                Some(_) => (format!("syntax error at or near \"{tok}\""), at),
                None => ("syntax error at end of input".to_owned(), end),
            }
        }
        Some(tok) => (format!("syntax error at or near \"{}\"", tok.trim_matches('\'')), at),
        None => (format!("syntax error: {text}"), None),
    };
    let e = SqlError::syntax(message).with_detail(text);
    match position {
        Some(p) => e.with_position(p),
        None => e,
    }
}

/// The `Line: L, Column: C` location in a sqlparser error message.
fn error_location(text: &str) -> Option<sqlparser::tokenizer::Location> {
    let rest = text.rsplit("Line: ").next()?;
    let (line, col) = rest.split_once(", Column: ")?;
    let col: String = col.chars().take_while(char::is_ascii_digit).collect();
    Some(sqlparser::tokenizer::Location { line: line.trim().parse().ok()?, column: col.parse().ok()? })
}

/// The byte offset of a 1-based line/column (in characters) in `sql`.
fn offset_of(sql: &str, at: sqlparser::tokenizer::Location) -> Option<usize> {
    let mut line = 1u64;
    let mut col = 1u64;
    for (i, c) in sql.char_indices() {
        if line == at.line && col == at.column {
            return Some(i);
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    None
}

/// The first token after the one starting at `at`, and where it starts.
fn token_after(sql: &str, at: sqlparser::tokenizer::Location) -> Option<(String, sqlparser::tokenizer::Location)> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().ok()?;
    tokens
        .into_iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
        .find(|t| (t.span.start.line, t.span.start.column) > (at.line, at.column))
        .map(|t| (t.token.to_string(), t.span.start))
}

/// Tokenizer failures, worded like Postgres' scanner (`unterminated quoted string at or near
/// "'abc"`, quoting the rest of the input from the opening quote).
fn lexer_error(text: &str, sql: &str) -> Option<SqlError> {
    let what = if text.contains("Unterminated string literal") || text.contains("Unterminated encoded string literal") {
        "unterminated quoted string"
    } else if text.contains("Unterminated dollar-quoted") {
        "unterminated dollar-quoted string"
    } else if text.contains("Unterminated unicode encoded string literal") {
        "unterminated quoted string"
    } else {
        return None;
    };
    let at = error_location(text)?;
    let start = offset_of(sql, at)?;
    // sqlparser points at the opening quote; Postgres quotes the rest of the input.
    let rest = &sql[start..];
    let e = SqlError::syntax(format!("{what} at or near \"{}\"", rest.trim_end())).with_detail(text.to_owned());
    Some(match char_position(&line_starts(sql), at) {
        Some(p) => e.with_position(p),
        None => e,
    })
}

/// Postgres identifier folding: unquoted identifiers are lower-cased.
pub(crate) fn ident(i: &sq::Ident) -> String {
    if i.quote_style.is_some() { i.value.clone() } else { i.value.to_lowercase() }
}

pub(crate) fn object_name(name: &sq::ObjectName) -> SqlResult<ObjectName> {
    let mut parts = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        match part {
            sq::ObjectNamePart::Identifier(i) => parts.push(ident(i)),
            sq::ObjectNamePart::Function(_) => return Err(unsupported("function-valued name parts")),
        }
    }
    Ok(ObjectName(parts))
}

pub(crate) fn unsupported(what: &str) -> SqlError {
    SqlError::new(code::FEATURE_NOT_SUPPORTED, format!("{what} is not supported by pgmem"))
}

pub(crate) fn lower_statement(stmt: sq::Statement) -> SqlResult<Statement> {
    use sq::Statement as S;
    Ok(match stmt {
        S::Query(mut q) => {
            // `SELECT ... INTO [TEMP | UNLOGGED] [TABLE] t ...` is CREATE TABLE t AS.
            if let sq::SetExpr::Select(s) = q.body.as_mut()
                && let Some(into) = s.into.take()
            {
                let name = match into.targets.as_slice() {
                    [sq::Expr::Identifier(i)] => pgmem_ir::ObjectName(vec![ident(i)]),
                    [sq::Expr::CompoundIdentifier(parts)] => pgmem_ir::ObjectName(parts.iter().map(ident).collect()),
                    _ => return Err(SqlError::syntax("syntax error at or near \"INTO\"")),
                };
                let query = query::lower_query(*q)?;
                return Ok(Statement::CreateTable(Box::new(pgmem_ir::CreateTable {
                    name,
                    if_not_exists: false,
                    temporary: into.temporary,
                    columns: Vec::new(),
                    constraints: Vec::new(),
                    as_query: Some(Box::new(query)),
                    options: pgmem_ir::CreateTableOptions { unlogged: into.unlogged, ..Default::default() },
                })));
            }
            let mut q = query::lower_query(*q)?;
            // `WITH ... INSERT / UPDATE / DELETE`: the WITH belongs to the statement.
            if let pgmem_ir::SetExpr::Dml(_) = q.body {
                let with = q.with.take();
                let pgmem_ir::SetExpr::Dml(mut inner) = q.body else { unreachable!() };
                match inner.as_mut() {
                    Statement::Insert(i) => i.with = with,
                    Statement::Update(u) => u.with = with,
                    Statement::Delete(d) => d.with = with,
                    Statement::Merge(m) => m.with = with,
                    _ => return Err(unsupported("WITH on this statement")),
                }
                return Ok(*inner);
            }
            Statement::Query(Box::new(q))
        }
        S::Insert(i) => Statement::Insert(Box::new(dml::lower_insert(i)?)),
        S::Update(u) => Statement::Update(Box::new(dml::lower_update(u)?)),
        S::Delete(d) => Statement::Delete(Box::new(dml::lower_delete(d)?)),
        S::Merge(m) => Statement::Merge(Box::new(dml::lower_merge(m)?)),
        S::CreateTable(ct) => Statement::CreateTable(Box::new(ddl::lower_create_table(ct)?)),
        S::CreateView(cv) => Statement::CreateView(Box::new(ddl::lower_create_view(cv)?)),
        S::RefreshMaterializedView(r) => Statement::RefreshMatview {
            name: object_name(&r.name)?,
            concurrently: r.concurrently,
            with_data: r.with_data.is_none_or(|d| d),
        },
        S::AlterView(av) => match av.operation {
            sq::AlterViewOperation::RenameTo { new_name } => Statement::RenameView {
                name: object_name(&av.name)?,
                new_name: object_name(&new_name)?.base().to_owned(),
                materialized: av.materialized,
                if_exists: av.if_exists,
            },
            sq::AlterViewOperation::SetOptions { options } => Statement::AlterRelOptions {
                object: if av.materialized { ObjectType::MaterializedView } else { ObjectType::View },
                name: object_name(&av.name)?,
                if_exists: av.if_exists,
                reset: false,
                options: options.iter().map(ddl::rel_option).collect::<SqlResult<Vec<_>>>()?,
            },
            _ => return Err(unsupported("this ALTER VIEW action")),
        },
        S::CreateIndex(ci) => Statement::CreateIndex(Box::new(ddl::lower_create_index(ci)?)),
        S::AlterTable(at)
            if at.operations.len() == 1
                && matches!(
                    at.operations[0],
                    sq::AlterTableOperation::SetOptionsParens { .. } | sq::AlterTableOperation::ResetOptions { .. }
                ) =>
        {
            let (reset, options) = match &at.operations[0] {
                sq::AlterTableOperation::SetOptionsParens { options } => {
                    (false, options.iter().map(ddl::rel_option).collect::<SqlResult<Vec<_>>>()?)
                }
                sq::AlterTableOperation::ResetOptions { options } => {
                    (true, options.iter().map(|n| ddl::rel_option(&sq::SqlOption::Ident(n.clone()))).collect::<SqlResult<Vec<_>>>()?)
                }
                _ => unreachable!(),
            };
            Statement::AlterRelOptions { object: ObjectType::Table, name: object_name(&at.name)?, if_exists: at.if_exists, reset, options }
        }
        S::AlterTable(at) => Statement::AlterTable(Box::new(ddl::lower_alter_table(at)?)),
        S::CreateSequence { name, if_not_exists, sequence_options, .. } => {
            Statement::CreateSequence(Box::new(ddl::lower_create_sequence(&name, if_not_exists, &sequence_options)?))
        }
        S::Drop { object_type, if_exists, names, cascade, .. } => {
            let object_type = match object_type {
                sq::ObjectType::Table => ObjectType::Table,
                sq::ObjectType::View => ObjectType::View,
                sq::ObjectType::MaterializedView => ObjectType::MaterializedView,
                sq::ObjectType::Index => ObjectType::Index,
                sq::ObjectType::Sequence => ObjectType::Sequence,
                sq::ObjectType::Schema => ObjectType::Schema,
                sq::ObjectType::Type => ObjectType::Type,
                other => return Err(unsupported(&format!("DROP {other}"))),
            };
            let names = names.iter().map(object_name).collect::<SqlResult<Vec<_>>>()?;
            Statement::Drop(Box::new(pgmem_ir::Drop { object_type, names, if_exists, cascade }))
        }
        S::CreateType { name, representation } => {
            use pgmem_ir::CreateTypeKind as K;
            let kind = match representation {
                Some(sq::UserDefinedTypeRepresentation::Enum { labels }) => K::Enum(labels.into_iter().map(|l| l.value).collect()),
                Some(sq::UserDefinedTypeRepresentation::Composite { attributes }) => K::Composite(
                    attributes.iter().map(|a| Ok((ident(&a.name), expr::lower_type(&a.data_type)?))).collect::<SqlResult<Vec<_>>>()?,
                ),
                // `CREATE TYPE name` (a shell type) and range / base types.
                _ => return Err(unsupported("this form of CREATE TYPE")),
            };
            Statement::CreateType(Box::new(pgmem_ir::CreateType { name: object_name(&name)?, kind }))
        }
        S::DropDomain(d) => Statement::Drop(Box::new(pgmem_ir::Drop {
            object_type: ObjectType::Domain,
            names: vec![object_name(&d.name)?],
            if_exists: d.if_exists,
            cascade: matches!(d.drop_behavior, Some(sq::DropBehavior::Cascade)),
        })),
        S::Truncate(t) => Statement::Truncate(Box::new(pgmem_ir::Truncate {
            tables: t.table_names.iter().map(|tt| object_name(&tt.name)).collect::<SqlResult<Vec<_>>>()?,
            restart_identity: matches!(t.identity, Some(sq::TruncateIdentityOption::Restart)),
            cascade: matches!(t.cascade, Some(sq::CascadeOption::Cascade)),
        })),
        S::StartTransaction { modes, .. } => {
            let (isolation, read_only) = transaction_modes(&modes);
            Statement::Transaction(TransactionStmt::Begin { isolation, read_only })
        }
        S::Commit { chain, .. } => Statement::Transaction(TransactionStmt::Commit { chain }),
        S::Rollback { chain, savepoint } => match savepoint {
            Some(name) => Statement::Transaction(TransactionStmt::RollbackTo(ident(&name))),
            None => Statement::Transaction(TransactionStmt::Rollback { chain }),
        },
        S::Savepoint { name } => Statement::Transaction(TransactionStmt::Savepoint(ident(&name))),
        S::ReleaseSavepoint { name } => Statement::Transaction(TransactionStmt::Release(ident(&name))),
        S::Set(set) => lower_set(set)?,
        S::Reset(r) => match r.reset {
            sq::Reset::ALL => Statement::Reset { name: None },
            sq::Reset::ConfigurationParameter(name) => Statement::Reset { name: Some(object_name(&name)?.to_string()) },
            sq::Reset::SessionAuthorization => Statement::NoOp { tag: "RESET".into() },
        },
        S::ShowVariable { variable } => Statement::Show { name: show_name(&variable.iter().map(ident).collect::<Vec<_>>()) },
        S::Discard { object_type } => Statement::Discard(match object_type {
            sq::DiscardObject::ALL => pgmem_ir::DiscardKind::All,
            sq::DiscardObject::PLANS => pgmem_ir::DiscardKind::Plans,
            sq::DiscardObject::SEQUENCES => pgmem_ir::DiscardKind::Sequences,
            sq::DiscardObject::TEMP => pgmem_ir::DiscardKind::Temp,
        }),
        S::Prepare { name, data_types, statement } => Statement::Prepare(Box::new(pgmem_ir::Prepare {
            name: ident(&name),
            param_types: data_types.iter().map(expr::lower_type).collect::<SqlResult<Vec<_>>>()?,
            statement: lower_statement(*statement)?,
            text: String::new(),
        })),
        S::Execute { name: Some(name), parameters, immediate: false, .. } => {
            Statement::Execute { name: object_name(&name)?.base().to_owned(), args: expr::lower_exprs(parameters)? }
        }
        S::Deallocate { name, .. } => {
            let all = name.quote_style.is_none() && name.value.eq_ignore_ascii_case("all");
            Statement::Deallocate { name: (!all).then(|| ident(&name)) }
        }
        S::Declare { mut stmts, .. } if stmts.len() == 1 && stmts[0].for_query.is_some() => {
            let d = stmts.remove(0);
            let [name] = d.names.as_slice() else { return Err(unsupported("DECLARE with several names")) };
            let Some(query) = d.for_query else { return Err(unsupported("DECLARE without a query")) };
            Statement::DeclareCursor(Box::new(pgmem_ir::DeclareCursor {
                name: ident(name),
                query: query::lower_query(*query)?,
                binary: d.binary == Some(true),
                scroll: d.scroll,
                hold: d.hold == Some(true),
                text: String::new(),
            }))
        }
        S::Close { cursor } => Statement::CloseCursor {
            name: match cursor {
                sq::CloseCursor::All => None,
                sq::CloseCursor::Specific { name } => Some(ident(&name)),
            },
        },
        S::LISTEN { channel } => Statement::Listen { channel: ident(&channel) },
        S::UNLISTEN { channel } => {
            Statement::Unlisten { channel: (channel.quote_style.is_some() || channel.value != "*").then(|| ident(&channel)) }
        }
        S::NOTIFY { channel, payload } => Statement::Notify { channel: ident(&channel), payload },
        S::Explain { statement, analyze, options, format, .. } => {
            let mut explain = pgmem_ir::Explain { statement: lower_statement(*statement)?, analyze, format: pgmem_ir::ExplainFormat::Text };
            if let Some(f) = format {
                explain.format = explain_format(&f.to_string());
            }
            for o in options.unwrap_or_default() {
                let arg = o.arg.as_ref().map(set_value).unwrap_or_default();
                match ident(&o.name).as_str() {
                    "analyze" => explain.analyze = !matches!(arg.to_ascii_lowercase().as_str(), "false" | "off" | "0"),
                    "format" => explain.format = explain_format(&arg),
                    _ => {}
                }
            }
            Statement::Explain(Box::new(explain))
        }
        S::Lock(l) => Statement::Lock {
            tables: l.tables.iter().map(|t| Ok((object_name(&t.name)?, t.only))).collect::<SqlResult<Vec<_>>>()?,
            mode: match l.lock_mode {
                Some(sq::LockTableMode::AccessShare) => 1,
                Some(sq::LockTableMode::RowShare) => 2,
                Some(sq::LockTableMode::RowExclusive) => 3,
                Some(sq::LockTableMode::ShareUpdateExclusive) => 4,
                Some(sq::LockTableMode::Share) => 5,
                Some(sq::LockTableMode::ShareRowExclusive) => 6,
                Some(sq::LockTableMode::Exclusive) => 7,
                _ => 8,
            },
            nowait: l.nowait,
        },
        S::Comment { object_type, object_name: target_name, comment, .. } => {
            use pgmem_ir::CommentTarget as T;
            let target = match object_type {
                sq::CommentObject::Table => T::Table,
                sq::CommentObject::View => T::View,
                sq::CommentObject::MaterializedView => T::MaterializedView,
                sq::CommentObject::Sequence => T::Sequence,
                sq::CommentObject::Index => T::Index,
                sq::CommentObject::Column => T::Column,
                sq::CommentObject::Schema => T::Schema,
                sq::CommentObject::Type => T::Type,
                sq::CommentObject::Domain => T::Domain,
                _ => T::Other,
            };
            Statement::Comment { target, name: object_name(&target_name)?, text: comment }
        }
        S::AlterSchema(a) => {
            let name = object_name(&a.name)?.base().to_owned();
            let mut rename_to = None;
            for op in a.operations {
                match op {
                    sq::AlterSchemaOperation::Rename { name: new } => rename_to = Some(object_name(&new)?.base().to_owned()),
                    sq::AlterSchemaOperation::OwnerTo { .. } => {}
                    _ => return Err(unsupported("this ALTER SCHEMA action")),
                }
            }
            Statement::AlterSchema { name, rename_to }
        }
        S::CreateExtension(e) => Statement::CreateExtension {
            name: ident(&e.name),
            if_not_exists: e.if_not_exists,
            schema: e.schema.as_ref().map(ident),
            version: e.version.as_ref().map(ident),
        },
        S::DropExtension(d) => Statement::DropExtension {
            names: d.names.iter().map(ident).collect(),
            if_exists: d.if_exists,
            cascade: matches!(d.cascade_or_restrict, Some(sq::ReferentialAction::Cascade)),
        },
        S::CreateSchema { schema_name, if_not_exists, .. } => lower_create_schema(schema_name, if_not_exists, Vec::new())?,
        S::Grant(_) | S::GrantRole(_) => Statement::NoOp { tag: "GRANT".into() },
        S::Revoke(_) | S::RevokeRole(_) => Statement::NoOp { tag: "REVOKE".into() },
        other => {
            let text = other.to_string();
            let head: String = text.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
            return Err(unsupported(&format!("statement {head}")));
        }
    })
}

fn explain_format(f: &str) -> pgmem_ir::ExplainFormat {
    match f.to_ascii_lowercase().as_str() {
        "json" => pgmem_ir::ExplainFormat::Json,
        "yaml" => pgmem_ir::ExplainFormat::Yaml,
        "xml" => pgmem_ir::ExplainFormat::Xml,
        _ => pgmem_ir::ExplainFormat::Text,
    }
}

fn transaction_modes(modes: &[sq::TransactionMode]) -> (Option<IsolationLevel>, Option<bool>) {
    let mut isolation = None;
    let mut read_only = None;
    for m in modes {
        match m {
            sq::TransactionMode::IsolationLevel(level) => {
                isolation = Some(match level {
                    sq::TransactionIsolationLevel::ReadUncommitted => IsolationLevel::ReadUncommitted,
                    sq::TransactionIsolationLevel::ReadCommitted => IsolationLevel::ReadCommitted,
                    sq::TransactionIsolationLevel::RepeatableRead | sq::TransactionIsolationLevel::Snapshot => {
                        IsolationLevel::RepeatableRead
                    }
                    sq::TransactionIsolationLevel::Serializable => IsolationLevel::Serializable,
                });
            }
            sq::TransactionMode::AccessMode(mode) => read_only = Some(matches!(mode, sq::TransactionAccessMode::ReadOnly)),
        }
    }
    (isolation, read_only)
}

/// The text form of a SET value: identifiers and strings by value, everything else as written.
fn set_value(e: &sq::Expr) -> String {
    match e {
        sq::Expr::Identifier(i) => ident(i),
        sq::Expr::Value(v) => match &v.value {
            sq::Value::SingleQuotedString(s) | sq::Value::EscapedStringLiteral(s) => s.clone(),
            sq::Value::Number(n, _) => n.clone(),
            sq::Value::Boolean(b) => if *b { "on" } else { "off" }.to_owned(),
            other => other.to_string(),
        },
        other => other.to_string(),
    }
}

/// A SHOW target: the multi-word names (`TIME ZONE`, `TRANSACTION ISOLATION LEVEL`) are
/// joined with spaces; anything else (`myapp.user_id`) is a dotted parameter name.
fn show_name(parts: &[String]) -> String {
    let spaced = parts.join(" ").to_ascii_lowercase();
    match spaced.as_str() {
        "time zone" | "transaction isolation level" | "session authorization" | "all" => spaced,
        _ if parts.len() == 1 => spaced,
        _ => parts.join(".").to_ascii_lowercase(),
    }
}

fn lower_set(set: sq::Set) -> SqlResult<Statement> {
    Ok(match set {
        sq::Set::SingleAssignment { scope, variable, values, .. } => Statement::Set {
            name: object_name(&variable)?.to_string().to_ascii_lowercase(),
            value: values.iter().map(set_value).collect(),
            local: matches!(scope, Some(sq::ContextModifier::Local)),
        },
        sq::Set::SetTimeZone { local, value } => Statement::Set { name: "timezone".into(), value: vec![set_value(&value)], local },
        sq::Set::SetNames { charset_name, .. } => {
            Statement::Set { name: "client_encoding".into(), value: vec![ident(&charset_name)], local: false }
        }
        // SET SESSION CHARACTERISTICS AS TRANSACTION sets the session defaults.
        sq::Set::SetTransaction { modes, session: true, .. } => match transaction_modes(&modes) {
            (Some(level), _) => Statement::Set {
                name: "default_transaction_isolation".into(),
                value: vec![
                    match level {
                        IsolationLevel::ReadUncommitted => "read uncommitted",
                        IsolationLevel::ReadCommitted => "read committed",
                        IsolationLevel::RepeatableRead => "repeatable read",
                        IsolationLevel::Serializable => "serializable",
                    }
                    .into(),
                ],
                local: false,
            },
            (None, Some(ro)) => Statement::Set {
                name: "default_transaction_read_only".into(),
                value: vec![if ro { "on" } else { "off" }.into()],
                local: false,
            },
            (None, None) => Statement::NoOp { tag: "SET".into() },
        },
        sq::Set::SetTransaction { modes, .. } => {
            let (isolation, read_only) = transaction_modes(&modes);
            Statement::Transaction(TransactionStmt::SetTransaction { isolation, read_only })
        }
        sq::Set::SetRole { context_modifier, role_name } => Statement::Set {
            name: "role".into(),
            value: vec![role_name.as_ref().map_or_else(|| "none".to_owned(), ident)],
            local: matches!(context_modifier, Some(sq::ContextModifier::Local)),
        },
        sq::Set::SetSessionAuthorization(p) => Statement::Set {
            name: "session_authorization".into(),
            value: vec![match p.kind {
                sq::SetSessionAuthorizationParamKind::Default => "default".to_owned(),
                sq::SetSessionAuthorizationParamKind::User(u) => ident(&u),
            }],
            local: matches!(p.scope, sq::ContextModifier::Local),
        },
        _ => Statement::NoOp { tag: "SET".into() },
    })
}

#[cfg(test)]
mod go_port;
#[cfg(test)]
mod tests;
