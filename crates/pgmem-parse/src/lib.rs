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

mod ddl;
mod dml;
mod expr;
mod query;
mod search_cycle;
mod sqljson;
mod typeddl;
mod utility;

use pgmem_ir::{IsolationLevel, ObjectName, ObjectType, Statement, TransactionStmt};
use pgmem_types::{SqlError, SqlResult, code};
use sqlparser::ast as sq;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};

/// Parses a query string that may contain several `;`-separated statements (the simple-query
/// protocol). An empty string yields `[Statement::Empty]`.
pub fn parse(sql: &str) -> SqlResult<Vec<Statement>> {
    let original = sql;
    search_cycle::clear();
    if sql.trim().trim_matches(';').trim().is_empty() {
        return Ok(vec![Statement::Empty]);
    }
    // SQL/JSON query functions are rewritten up front (sqlparser can't parse their clauses).
    let json_rewritten = sqljson::rewrite(sql);
    let sql = json_rewritten.as_deref().unwrap_or(sql);
    let parsed = parse_statements(sql).map(|s| (s, sql.to_owned())).or_else(|err| match rewrite_unsupported(sql) {
        // Retry Postgres syntax sqlparser lacks after rewriting it to an equivalent form.
        Some(rewritten) => parse_statements(&rewritten).map(|s| (s, rewritten)).map_err(|_| err),
        None => Err(err),
    });
    match parsed {
        Ok((stmts, text)) => {
            PARSED_TEXT.with(|t| *t.borrow_mut() = text);
            let mut lowered = stmts.into_iter().map(lower_parsed).collect::<SqlResult<Vec<_>>>()?;
            // PREPARE and DECLARE remember the query string they came from, as Postgres does.
            for s in &mut lowered {
                match s {
                    Statement::Prepare(p) => original.clone_into(&mut p.text),
                    Statement::DeclareCursor(d) => original.clone_into(&mut d.text),
                    _ => {}
                }
            }
            Ok(lowered)
        }
        Err(err) => {
            // Fall back to per-statement prefix recognition for the utility statements
            // sqlparser doesn't model; only when every piece is recognized do we accept it.
            let pieces: Vec<&str> = split_statements(sql);
            let fallback: Option<Vec<Statement>> = pieces.iter().map(|p| fallback_statement(p)).collect();
            fallback.ok_or_else(|| map_parser_error(&err, sql))
        }
    }
}

thread_local! {
    /// The text the statements being lowered were parsed from (spans point into it).
    static PARSED_TEXT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Postgres' `syntax error at or near "X"` for the first token at or after `at` in the text
/// being lowered (`at end of input` when there is none): for syntax sqlparser accepts but
/// Postgres' grammar rejects.
pub(crate) fn syntax_error_at(at: sqlparser::tokenizer::Location) -> SqlError {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let next = PARSED_TEXT.with(|t| {
        let text = t.borrow();
        let tokens = Tokenizer::new(&PostgreSqlDialect {}, &text).tokenize_with_location().ok()?;
        tokens
            .into_iter()
            .filter(|t| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
            .find(|t| (t.span.start.line, t.span.start.column) >= (at.line, at.column))
            .map(|t| t.token.to_string())
    });
    match next {
        Some(tok) => SqlError::syntax(format!("syntax error at or near \"{tok}\"")),
        None => SqlError::syntax("syntax error at end of input"),
    }
}

fn lower_parsed(p: Parsed) -> SqlResult<Statement> {
    match p {
        Parsed::Sql(s) => lower_statement(*s),
        Parsed::TypeDdl(t) => typeddl::lower(*t),
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

/// `SEARCH BREADTH FIRST`'s depth: field 0 of the search record.
pub(crate) const SEARCH_DEPTH_FN: &str = "__pgmem_search_depth";

/// The function `BETWEEN SYMMETRIC`'s low bound is wrapped in before parsing.
pub(crate) const SYMMETRIC_MARKER: &str = "__pgmem_symmetric";

enum Parsed {
    Sql(Box<sq::Statement>),
    TypeDdl(Box<typeddl::TypeDdl>),
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
    let mut stmts = parse(sql)?;
    if stmts.len() > 1 {
        return Err(SqlError::syntax("cannot insert multiple commands into a prepared statement"));
    }
    Ok(stmts.pop().unwrap_or(Statement::Empty))
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
        // MERGE's `THEN INSERT DEFAULT VALUES` → `THEN INSERT VALUES (__pgmem_default_values())`.
        if word(&tokens[i], "THEN") && at(1, "INSERT") && at(2, "DEFAULT") && at(3, "VALUES") {
            drop[sig[k + 2]] = true;
            replace[sig[k + 3]] = Some("VALUES (__pgmem_default_values())");
        }
        if word(&tokens[i], "FOR") && at(1, "NO") && at(2, "KEY") && at(3, "UPDATE") {
            drop[sig[k + 1]] = true;
            drop[sig[k + 2]] = true;
        } else if word(&tokens[i], "FOR") && at(1, "KEY") && at(2, "SHARE") {
            drop[sig[k + 1]] = true;
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
    let found = text.split("found: ").nth(1).map(|rest| rest.split(" at Line:").next().unwrap_or(rest).trim().to_owned());
    let message = match found.as_deref() {
        Some("EOF") | None if text.contains("EOF") => "syntax error at end of input".to_owned(),
        // `... ORDER` / `... GROUP` without BY: Postgres fails at the token after the keyword.
        Some(tok) if (tok.eq_ignore_ascii_case("ORDER") || tok.eq_ignore_ascii_case("GROUP")) && text.contains("end of statement") => {
            match error_location(&text).and_then(|at| token_after(sql, at)) {
                Some(next) if !next.eq_ignore_ascii_case("BY") => format!("syntax error at or near \"{next}\""),
                Some(_) => format!("syntax error at or near \"{tok}\""),
                None => "syntax error at end of input".to_owned(),
            }
        }
        Some(tok) => format!("syntax error at or near \"{}\"", tok.trim_matches('\'')),
        None => format!("syntax error: {text}"),
    };
    SqlError::syntax(message).with_detail(text)
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

/// The first token after the one starting at `at`.
fn token_after(sql: &str, at: sqlparser::tokenizer::Location) -> Option<String> {
    use sqlparser::tokenizer::{Token, Tokenizer};
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().ok()?;
    tokens
        .into_iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
        .find(|t| (t.span.start.line, t.span.start.column) > (at.line, at.column))
        .map(|t| t.token.to_string())
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
    let start = offset_of(sql, error_location(text)?)?;
    // sqlparser points at the opening quote; Postgres quotes the rest of the input.
    let rest = &sql[start..];
    Some(SqlError::syntax(format!("{what} at or near \"{}\"", rest.trim_end())).with_detail(text.to_owned()))
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
        S::Query(q) => {
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
        S::CreateIndex(ci) => Statement::CreateIndex(Box::new(ddl::lower_create_index(ci)?)),
        S::AlterTable(at) => Statement::AlterTable(Box::new(ddl::lower_alter_table(at)?)),
        S::CreateSequence { name, if_not_exists, sequence_options, .. } => {
            Statement::CreateSequence(Box::new(ddl::lower_create_sequence(&name, if_not_exists, &sequence_options)?))
        }
        S::Drop { object_type, if_exists, names, cascade, .. } => {
            let object_type = match object_type {
                sq::ObjectType::Table => ObjectType::Table,
                sq::ObjectType::View | sq::ObjectType::MaterializedView => ObjectType::View,
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
        S::Lock(l) => Statement::Lock { tables: l.tables.iter().map(|t| object_name(&t.name)).collect::<SqlResult<Vec<_>>>()? },
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
        S::CreateExtension(_) => Statement::NoOp { tag: "CREATE EXTENSION".into() },
        S::DropExtension(_) => Statement::NoOp { tag: "DROP EXTENSION".into() },
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
        _ => Statement::NoOp { tag: "SET".into() },
    })
}

#[cfg(test)]
mod go_port;
#[cfg(test)]
mod tests;
