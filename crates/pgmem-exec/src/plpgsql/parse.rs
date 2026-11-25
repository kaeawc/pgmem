//! PL/pgSQL source → statement tree (`pl_gram.y`). SQL pieces (expressions, queries,
//! statements) stay text: they are substituted and parsed by the SQL parser when they run.

use pgmem_types::{SqlError, SqlResult};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

#[derive(Clone, Debug)]
pub struct Block {
    pub label: Option<String>,
    pub decls: Vec<Decl>,
    pub body: Vec<Stmt>,
    pub handlers: Vec<Handler>,
}

#[derive(Clone, Debug)]
pub struct Decl {
    pub name: String,
    /// The type as written (`int`, `text[]`, `t%ROWTYPE`, `record`, ...).
    pub ty: String,
    pub constant: bool,
    pub not_null: bool,
    pub default: Option<String>,
    /// `name ALIAS FOR $n` / `ALIAS FOR name`.
    pub alias: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Handler {
    /// Condition names (lowercase) or `sqlstate:XXXXX`.
    pub conditions: Vec<String>,
    pub body: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Assign {
        target: String,
        expr: String,
    },
    If {
        branches: Vec<(String, Vec<Stmt>)>,
        otherwise: Vec<Stmt>,
    },
    Case {
        subject: Option<String>,
        whens: Vec<(String, Vec<Stmt>)>,
        otherwise: Option<Vec<Stmt>>,
    },
    Loop {
        label: Option<String>,
        body: Vec<Stmt>,
    },
    While {
        label: Option<String>,
        cond: String,
        body: Vec<Stmt>,
    },
    ForInt {
        label: Option<String>,
        var: String,
        reverse: bool,
        lo: String,
        hi: String,
        step: Option<String>,
        body: Vec<Stmt>,
    },
    ForQuery {
        label: Option<String>,
        targets: Vec<String>,
        query: Query,
        body: Vec<Stmt>,
    },
    ForEach {
        label: Option<String>,
        var: String,
        array: String,
        body: Vec<Stmt>,
    },
    Exit {
        label: Option<String>,
        when: Option<String>,
        cont: bool,
    },
    Return(Option<String>),
    ReturnNext(Option<String>),
    ReturnQuery(Query),
    Raise(Raise),
    Assert {
        cond: String,
        message: Option<String>,
    },
    Perform(String),
    /// A SQL statement, with `INTO [STRICT] targets` taken out.
    Sql {
        text: String,
        into: Option<(Vec<String>, bool)>,
    },
    Execute {
        sql: String,
        into: Option<(Vec<String>, bool)>,
        using: Vec<String>,
    },
    GetDiagnostics {
        stacked: bool,
        items: Vec<(String, String)>,
    },
    Null,
    Block(Block),
}

/// A query a loop or RETURN QUERY reads: static text or `EXECUTE expr [USING ...]`.
#[derive(Clone, Debug)]
pub enum Query {
    Static(String),
    Dynamic { sql: String, using: Vec<String> },
}

#[derive(Clone, Debug)]
pub struct Raise {
    /// `EXCEPTION`, `WARNING`, `NOTICE`, `INFO`, `LOG`, `DEBUG`.
    pub level: String,
    /// The format string, or a condition name / `sqlstate:XXXXX`.
    pub message: Option<String>,
    pub condition: Option<String>,
    pub args: Vec<String>,
    pub options: Vec<(String, String)>,
    /// A bare `RAISE;` re-raising the current exception.
    pub reraise: bool,
}

struct P {
    /// Significant tokens with the text between them kept (for SQL pieces).
    toks: Vec<Token>,
    /// The source text of each token (whitespace included in the gaps).
    texts: Vec<String>,
    pos: usize,
}

fn syntax(msg: impl Into<String>) -> SqlError {
    SqlError::syntax(msg)
}

impl P {
    fn new(src: &str) -> SqlResult<P> {
        let raw = Tokenizer::new(&PostgreSqlDialect {}, src).tokenize().map_err(|e| syntax(e.to_string()))?;
        let mut toks = Vec::new();
        let mut texts = Vec::new();
        let mut pending = String::new();
        for t in raw {
            match &t {
                Token::Whitespace(Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)) => pending.push(' '),
                Token::Whitespace(w) => pending.push_str(&w.to_string()),
                _ => {
                    texts.push(format!("{pending}{t}"));
                    pending.clear();
                    toks.push(t);
                }
            }
        }
        toks.push(Token::EOF);
        texts.push(String::new());
        Ok(P { toks, texts, pos: 0 })
    }

    fn peek(&self, n: usize) -> &Token {
        self.toks.get(self.pos + n).unwrap_or(&Token::EOF)
    }

    fn is(&self, n: usize, w: &str) -> bool {
        matches!(self.peek(n), Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w))
    }

    fn eat(&mut self, w: &str) -> bool {
        if self.is(0, w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, w: &str) -> SqlResult<()> {
        if self.eat(w) { Ok(()) } else { Err(self.error()) }
    }

    fn error(&self) -> SqlError {
        match self.peek(0) {
            Token::EOF => syntax("syntax error at end of input"),
            t => syntax(format!("syntax error at or near \"{t}\"")),
        }
    }

    fn semi(&mut self) -> SqlResult<()> {
        if *self.peek(0) == Token::SemiColon {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error())
        }
    }

    fn ident(&mut self) -> SqlResult<String> {
        match self.peek(0).clone() {
            Token::Word(w) => {
                self.pos += 1;
                Ok(if w.quote_style.is_some() { w.value } else { w.value.to_lowercase() })
            }
            _ => Err(self.error()),
        }
    }

    /// The source text of tokens `from..to`.
    fn text(&self, from: usize, to: usize) -> String {
        self.texts[from..to].concat().trim().to_owned()
    }

    /// Text up to (not including) the first top-level token satisfying `stop`.
    fn until(&mut self, stop: impl Fn(&P) -> bool) -> SqlResult<String> {
        let start = self.pos;
        let mut depth = 0i32;
        loop {
            match self.peek(0) {
                Token::EOF => break,
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth -= 1,
                _ => {}
            }
            if depth == 0 && stop(self) {
                break;
            }
            // A closing parenthesis can end the piece only once depth is back to 0.
            if depth < 0 {
                break;
            }
            self.pos += 1;
        }
        Ok(self.text(start, self.pos))
    }

    fn until_semi(&mut self) -> SqlResult<String> {
        let s = self.until(|p| *p.peek(0) == Token::SemiColon)?;
        self.semi()?;
        Ok(s)
    }

    fn until_word(&mut self, words: &[&str]) -> SqlResult<String> {
        self.until(|p| words.iter().any(|w| p.is(0, w)) || *p.peek(0) == Token::SemiColon)
    }
}

/// Parses a function body / DO block: one block, optionally followed by `;`.
pub fn parse_body(src: &str) -> SqlResult<Block> {
    let mut p = P::new(src)?;
    let b = block(&mut p, None)?;
    if *p.peek(0) == Token::SemiColon {
        p.pos += 1;
    }
    if *p.peek(0) != Token::EOF {
        return Err(p.error());
    }
    Ok(b)
}

fn label(p: &mut P) -> SqlResult<Option<String>> {
    if *p.peek(0) == Token::ShiftLeft {
        p.pos += 1;
        let l = p.ident()?;
        if *p.peek(0) != Token::ShiftRight {
            return Err(p.error());
        }
        p.pos += 1;
        return Ok(Some(l));
    }
    Ok(None)
}

fn block(p: &mut P, label: Option<String>) -> SqlResult<Block> {
    let mut decls = Vec::new();
    while p.eat("DECLARE") {
        while !p.is(0, "BEGIN") && !p.is(0, "DECLARE") {
            decls.push(decl(p)?);
        }
    }
    p.expect("BEGIN")?;
    let body = stmts(p, &["EXCEPTION", "END"])?;
    let mut handlers = Vec::new();
    if p.eat("EXCEPTION") {
        while p.eat("WHEN") {
            let mut conditions = Vec::new();
            loop {
                if p.eat("SQLSTATE") {
                    match p.peek(0).clone() {
                        Token::SingleQuotedString(s) => {
                            p.pos += 1;
                            conditions.push(format!("sqlstate:{s}"));
                        }
                        _ => return Err(p.error()),
                    }
                } else {
                    conditions.push(p.ident()?);
                }
                if !p.eat("OR") {
                    break;
                }
            }
            p.expect("THEN")?;
            let body = stmts(p, &["WHEN", "END"])?;
            handlers.push(Handler { conditions, body });
        }
    }
    p.expect("END")?;
    if let Some(l) = &label
        && matches!(p.peek(0), Token::Word(w) if w.value.eq_ignore_ascii_case(l))
    {
        p.pos += 1;
    }
    Ok(Block { label, decls, body, handlers })
}

fn decl(p: &mut P) -> SqlResult<Decl> {
    let name = p.ident()?;
    if p.eat("ALIAS") {
        p.expect("FOR")?;
        let target = p.until_semi()?;
        return Ok(Decl { name, ty: String::new(), constant: false, not_null: false, default: None, alias: Some(target) });
    }
    let constant = p.eat("CONSTANT");
    let ty = p.until(|p| {
        *p.peek(0) == Token::SemiColon
            || p.is(0, "DEFAULT")
            || p.is(0, "NOT")
            || p.is(0, "COLLATE")
            || *p.peek(0) == Token::Assignment
            || *p.peek(0) == Token::Eq
    })?;
    if p.eat("COLLATE") {
        p.ident()?;
    }
    let not_null = p.eat("NOT") && {
        p.expect("NULL")?;
        true
    };
    let default = if p.eat("DEFAULT") || matches!(p.peek(0), Token::Assignment | Token::Eq) {
        if matches!(p.peek(0), Token::Assignment | Token::Eq) {
            p.pos += 1;
        }
        Some(p.until_semi()?)
    } else {
        p.semi()?;
        None
    };
    Ok(Decl { name, ty, constant, not_null, default, alias: None })
}

/// Statements up to one of `ends` (not consumed).
fn stmts(p: &mut P, ends: &[&str]) -> SqlResult<Vec<Stmt>> {
    let mut out = Vec::new();
    loop {
        if *p.peek(0) == Token::EOF {
            return Err(syntax("syntax error at end of input"));
        }
        if ends.iter().any(|e| p.is(0, e)) {
            return Ok(out);
        }
        out.push(stmt(p)?);
    }
}

fn end_loop(p: &mut P, label: Option<&str>) -> SqlResult<()> {
    p.expect("END")?;
    p.expect("LOOP")?;
    if let Some(l) = label
        && matches!(p.peek(0), Token::Word(w) if w.value.eq_ignore_ascii_case(l))
    {
        p.pos += 1;
    }
    p.semi()
}

fn loop_body(p: &mut P, label: Option<&str>) -> SqlResult<Vec<Stmt>> {
    p.expect("LOOP")?;
    let body = stmts(p, &["END"])?;
    end_loop(p, label)?;
    Ok(body)
}

/// `INTO [STRICT] a, b` targets.
fn into_targets(p: &mut P) -> SqlResult<(Vec<String>, bool)> {
    let strict = p.eat("STRICT");
    let mut targets = Vec::new();
    loop {
        let start = p.pos;
        p.ident()?;
        while *p.peek(0) == Token::Period {
            p.pos += 1;
            p.ident()?;
        }
        targets.push(p.text(start, p.pos).to_lowercase());
        if *p.peek(0) != Token::Comma {
            break;
        }
        p.pos += 1;
    }
    Ok((targets, strict))
}

fn using_list(p: &mut P) -> SqlResult<Vec<String>> {
    let mut out = Vec::new();
    if p.eat("USING") {
        loop {
            out.push(p.until(|p| *p.peek(0) == Token::Comma || *p.peek(0) == Token::SemiColon || p.is(0, "LOOP") || p.is(0, "INTO"))?);
            if *p.peek(0) != Token::Comma {
                break;
            }
            p.pos += 1;
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_lines)]
fn stmt(p: &mut P) -> SqlResult<Stmt> {
    let label = label(p)?;
    if p.is(0, "DECLARE") || p.is(0, "BEGIN") {
        let b = block(p, label)?;
        p.semi()?;
        return Ok(Stmt::Block(b));
    }
    if p.eat("LOOP") {
        let body = stmts(p, &["END"])?;
        end_loop(p, label.as_deref())?;
        return Ok(Stmt::Loop { label, body });
    }
    if p.eat("WHILE") {
        let cond = p.until_word(&["LOOP"])?;
        let body = loop_body(p, label.as_deref())?;
        return Ok(Stmt::While { label, cond, body });
    }
    if p.eat("FOR") {
        let mut targets = vec![p.ident()?];
        while *p.peek(0) == Token::Comma {
            p.pos += 1;
            targets.push(p.ident()?);
        }
        p.expect("IN")?;
        let reverse = p.eat("REVERSE");
        // `lo .. hi` (an integer loop) or a query.
        let save = p.pos;
        let lo = p.until(|p| *p.peek(0) == Token::Period && *p.peek(1) == Token::Period || p.is(0, "LOOP"))?;
        if *p.peek(0) == Token::Period && *p.peek(1) == Token::Period {
            p.pos += 2;
            let hi = p.until_word(&["BY", "LOOP"])?;
            let step = if p.eat("BY") { Some(p.until_word(&["LOOP"])?) } else { None };
            let body = loop_body(p, label.as_deref())?;
            return Ok(Stmt::ForInt { label, var: targets.remove(0), reverse, lo, hi, step, body });
        }
        p.pos = save;
        let query = if p.eat("EXECUTE") {
            let sql = p.until_word(&["USING", "LOOP"])?;
            Query::Dynamic { sql, using: using_list(p)? }
        } else {
            Query::Static(p.until_word(&["LOOP"])?)
        };
        let body = loop_body(p, label.as_deref())?;
        return Ok(Stmt::ForQuery { label, targets, query, body });
    }
    if p.eat("FOREACH") {
        let var = p.ident()?;
        if p.eat("SLICE") {
            p.pos += 1;
        }
        p.expect("IN")?;
        p.expect("ARRAY")?;
        let array = p.until_word(&["LOOP"])?;
        let body = loop_body(p, label.as_deref())?;
        return Ok(Stmt::ForEach { label, var, array, body });
    }
    if p.eat("IF") {
        let mut branches = Vec::new();
        let cond = p.until_word(&["THEN"])?;
        p.expect("THEN")?;
        let body = stmts(p, &["ELSIF", "ELSEIF", "ELSE", "END"])?;
        branches.push((cond, body));
        let mut otherwise = Vec::new();
        loop {
            if p.eat("ELSIF") || p.eat("ELSEIF") {
                let cond = p.until_word(&["THEN"])?;
                p.expect("THEN")?;
                branches.push((cond, stmts(p, &["ELSIF", "ELSEIF", "ELSE", "END"])?));
            } else if p.eat("ELSE") {
                otherwise = stmts(p, &["END"])?;
            } else {
                break;
            }
        }
        p.expect("END")?;
        p.expect("IF")?;
        p.semi()?;
        return Ok(Stmt::If { branches, otherwise });
    }
    if p.eat("CASE") {
        let subject = if p.is(0, "WHEN") { None } else { Some(p.until_word(&["WHEN"])?) };
        let mut whens = Vec::new();
        let mut otherwise = None;
        while p.eat("WHEN") {
            let e = p.until_word(&["THEN"])?;
            p.expect("THEN")?;
            whens.push((e, stmts(p, &["WHEN", "ELSE", "END"])?));
        }
        if p.eat("ELSE") {
            otherwise = Some(stmts(p, &["END"])?);
        }
        p.expect("END")?;
        p.expect("CASE")?;
        p.semi()?;
        return Ok(Stmt::Case { subject, whens, otherwise });
    }
    if p.is(0, "EXIT") || p.is(0, "CONTINUE") {
        let cont = p.eat("CONTINUE");
        if !cont {
            p.pos += 1;
        }
        let label = if matches!(p.peek(0), Token::Word(w) if !w.value.eq_ignore_ascii_case("when")) { Some(p.ident()?) } else { None };
        let when = if p.eat("WHEN") { Some(p.until_semi()?) } else { p.semi().map(|()| None)? };
        return Ok(Stmt::Exit { label, when, cont });
    }
    if p.eat("RETURN") {
        if p.eat("NEXT") {
            let e = p.until_semi()?;
            return Ok(Stmt::ReturnNext((!e.is_empty()).then_some(e)));
        }
        if p.eat("QUERY") {
            if p.eat("EXECUTE") {
                let sql = p.until_word(&["USING"])?;
                let using = using_list(p)?;
                p.semi()?;
                return Ok(Stmt::ReturnQuery(Query::Dynamic { sql, using }));
            }
            return Ok(Stmt::ReturnQuery(Query::Static(p.until_semi()?)));
        }
        let e = p.until_semi()?;
        return Ok(Stmt::Return((!e.is_empty()).then_some(e)));
    }
    if p.eat("RAISE") {
        return raise(p);
    }
    if p.eat("ASSERT") {
        let cond = p.until(|p| *p.peek(0) == Token::Comma || *p.peek(0) == Token::SemiColon)?;
        let message = if *p.peek(0) == Token::Comma {
            p.pos += 1;
            Some(p.until_semi()?)
        } else {
            p.semi()?;
            None
        };
        return Ok(Stmt::Assert { cond, message });
    }
    if p.eat("PERFORM") {
        return Ok(Stmt::Perform(p.until_semi()?));
    }
    if p.eat("NULL") {
        p.semi()?;
        return Ok(Stmt::Null);
    }
    if p.is(0, "GET") && (p.is(1, "DIAGNOSTICS") || p.is(1, "CURRENT") || p.is(1, "STACKED")) {
        p.pos += 1;
        let stacked = p.eat("STACKED");
        let _ = p.eat("CURRENT");
        p.expect("DIAGNOSTICS")?;
        let mut items = Vec::new();
        loop {
            let var = p.ident()?;
            if !(*p.peek(0) == Token::Eq || *p.peek(0) == Token::Assignment) {
                return Err(p.error());
            }
            p.pos += 1;
            items.push((var, p.ident()?.to_lowercase()));
            if *p.peek(0) != Token::Comma {
                break;
            }
            p.pos += 1;
        }
        p.semi()?;
        return Ok(Stmt::GetDiagnostics { stacked, items });
    }
    if p.eat("EXECUTE") {
        let sql = p.until_word(&["INTO", "USING"])?;
        let mut into = None;
        let mut using = Vec::new();
        loop {
            if p.eat("INTO") {
                into = Some(into_targets(p)?);
            } else if p.is(0, "USING") {
                using = using_list(p)?;
            } else {
                break;
            }
        }
        p.semi()?;
        return Ok(Stmt::Execute { sql, into, using });
    }
    // An assignment: target [. field | [subscript]] {:= | =} expr.
    if matches!(p.peek(0), Token::Word(_)) {
        let start = p.pos;
        let mut i = 1;
        while *p.peek(i) == Token::Period && matches!(p.peek(i + 1), Token::Word(_)) {
            i += 2;
        }
        if *p.peek(i) == Token::LBracket {
            while *p.peek(i) != Token::RBracket && *p.peek(i) != Token::EOF {
                i += 1;
            }
            i += 1;
        }
        if matches!(p.peek(i), Token::Assignment | Token::Eq) && !is_sql_start(p) {
            let target = p.text(start, start + i).to_lowercase().replace(' ', "");
            p.pos = start + i + 1;
            return Ok(Stmt::Assign { target, expr: p.until_semi()? });
        }
    }
    sql_statement(p)
}

/// Keywords that start a SQL statement (not an assignment target).
fn is_sql_start(p: &P) -> bool {
    ["SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "VALUES", "MERGE"].iter().any(|w| p.is(0, w))
}

/// A SQL statement up to `;`, with its `INTO targets` (for SELECT, after the select list;
/// for DML, after RETURNING) taken out.
fn sql_statement(p: &mut P) -> SqlResult<Stmt> {
    let start = p.pos;
    let mut depth = 0i32;
    let mut into: Option<(Vec<String>, bool)> = None;
    let mut cut: Option<(usize, usize)> = None;
    let into_table = p.is(0, "INSERT") || p.is(0, "MERGE");
    loop {
        match p.peek(0) {
            Token::EOF => return Err(syntax("syntax error at end of input")),
            Token::SemiColon if depth == 0 => break,
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            _ => {}
        }
        // `INSERT INTO t` / `MERGE INTO t` is not an INTO clause.
        if depth == 0 && into.is_none() && p.is(0, "INTO") && !(into_table && p.pos == start + 1) {
            let from = p.pos;
            p.pos += 1;
            into = Some(into_targets(p)?);
            cut = Some((from, p.pos));
            continue;
        }
        p.pos += 1;
    }
    let end = p.pos;
    p.semi()?;
    let text = match cut {
        Some((a, b)) => format!("{} {}", p.text(start, a), p.text(b, end)),
        None => p.text(start, end),
    };
    Ok(Stmt::Sql { text, into })
}

fn raise(p: &mut P) -> SqlResult<Stmt> {
    let mut r = Raise { level: "EXCEPTION".into(), message: None, condition: None, args: Vec::new(), options: Vec::new(), reraise: false };
    for l in ["EXCEPTION", "WARNING", "NOTICE", "INFO", "LOG", "DEBUG"] {
        if p.eat(l) {
            r.level = l.to_owned();
            break;
        }
    }
    if *p.peek(0) == Token::SemiColon {
        p.pos += 1;
        r.reraise = true;
        return Ok(Stmt::Raise(r));
    }
    match p.peek(0).clone() {
        Token::SingleQuotedString(s) | Token::EscapedStringLiteral(s) => {
            p.pos += 1;
            r.message = Some(s);
            while *p.peek(0) == Token::Comma {
                p.pos += 1;
                r.args.push(p.until(|p| *p.peek(0) == Token::Comma || *p.peek(0) == Token::SemiColon || p.is(0, "USING"))?);
            }
        }
        Token::Word(w) if w.value.eq_ignore_ascii_case("sqlstate") => {
            p.pos += 1;
            match p.peek(0).clone() {
                Token::SingleQuotedString(s) => {
                    p.pos += 1;
                    r.condition = Some(format!("sqlstate:{s}"));
                }
                _ => return Err(p.error()),
            }
        }
        Token::Word(w) if !w.value.eq_ignore_ascii_case("using") => {
            p.pos += 1;
            r.condition = Some(w.value.to_lowercase());
        }
        _ => {}
    }
    if p.eat("USING") {
        loop {
            let opt = p.ident()?.to_lowercase();
            if !(*p.peek(0) == Token::Eq || *p.peek(0) == Token::Assignment) {
                return Err(p.error());
            }
            p.pos += 1;
            r.options.push((opt, p.until(|p| *p.peek(0) == Token::Comma || *p.peek(0) == Token::SemiColon)?));
            if *p.peek(0) != Token::Comma {
                break;
            }
            p.pos += 1;
        }
    }
    p.semi()?;
    // `%` placeholders must match the arguments (checked when the function is created).
    if let Some(m) = &r.message {
        let mut n = 0;
        let mut chars = m.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '%' {
                if chars.peek() == Some(&'%') {
                    chars.next();
                } else {
                    n += 1;
                }
            }
        }
        if n > r.args.len() {
            return Err(syntax("too few parameters specified for RAISE"));
        }
        if n < r.args.len() {
            return Err(syntax("too many parameters specified for RAISE"));
        }
    }
    Ok(Stmt::Raise(r))
}
