//! SQL/JSON query functions sqlparser cannot parse: `JSON_VALUE`, `JSON_QUERY`, `JSON_EXISTS`
//! (with their PASSING / RETURNING / WRAPPER / QUOTES / ON EMPTY / ON ERROR clauses) and
//! `JSON_TABLE`. Before parsing, each call is rewritten into an internal function call whose
//! arguments encode the clauses; the executor implements them (`pgmem_exec::funcs::sqljson`).
//!
//! The rewrite works on tokens, so literals and identifiers are never altered.

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

/// Behaviour codes shared with the executor.
pub const ON_NULL: i32 = 0;
pub const ON_ERROR_RAISE: i32 = 1;
pub const ON_DEFAULT: i32 = 2;
pub const ON_EMPTY_ARRAY: i32 = 3;
pub const ON_EMPTY_OBJECT: i32 = 4;
pub const ON_TRUE: i32 = 5;
pub const ON_FALSE: i32 = 6;
pub const ON_UNKNOWN: i32 = 7;

/// Rewrites every SQL/JSON query function in `sql`; None when there is none.
pub fn rewrite(sql: &str) -> Option<String> {
    let lower = sql.to_ascii_lowercase();
    if !["json_value", "json_query", "json_exists", "json_table"].iter().any(|k| lower.contains(k)) {
        return None;
    }
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize().ok()?;
    let out = rewrite_tokens(&tokens)?;
    (out != sql).then_some(out)
}

fn is_word(t: &Token, w: &str) -> bool {
    matches!(t, Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w))
}

fn text(tokens: &[Token]) -> String {
    tokens.iter().map(Token::to_string).collect()
}

/// Index of the `)` matching the `(` at `open`.
fn matching(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate().skip(open) {
        match t {
            Token::LParen => depth += 1,
            Token::RParen => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn rewrite_tokens(tokens: &[Token]) -> Option<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < tokens.len() {
        let name = ["json_value", "json_query", "json_exists", "json_table"].into_iter().find(|n| is_word(&tokens[i], n));
        let open = (i + 1..tokens.len()).find(|&j| !matches!(tokens[j], Token::Whitespace(_)));
        if let (Some(name), Some(open)) = (name, open)
            && tokens[open] == Token::LParen
        {
            let close = matching(tokens, open)?;
            // Inner calls first.
            let inner = rewrite_tokens(&tokens[open + 1..close])?;
            let inner_tokens = Tokenizer::new(&PostgreSqlDialect {}, &inner).tokenize().ok()?;
            let call = Call::parse(&inner_tokens)?;
            if name == "json_table" {
                // JSON_TABLE(...) [AS] alias → __pgmem_json_table(...) AS alias(columns)
                let mut j = close + 1;
                while j < tokens.len() && matches!(tokens[j], Token::Whitespace(_)) {
                    j += 1;
                }
                if j < tokens.len() && is_word(&tokens[j], "AS") {
                    j += 1;
                    while j < tokens.len() && matches!(tokens[j], Token::Whitespace(_)) {
                        j += 1;
                    }
                }
                let alias = match tokens.get(j) {
                    Some(Token::Word(w)) if w.keyword == sqlparser::keywords::Keyword::NoKeyword || w.quote_style.is_some() => {
                        let a = tokens[j].to_string();
                        j += 1;
                        a
                    }
                    _ => {
                        j = close + 1;
                        "\"json_table\"".to_owned()
                    }
                };
                out.push_str(&call.json_table(&alias)?);
                i = j;
            } else {
                out.push_str(&call.render(name)?);
                i = close + 1;
            }
            continue;
        }
        out.push_str(&tokens[i].to_string());
        i += 1;
    }
    Some(out)
}

/// Keywords that end an argument at the top level of an SQL/JSON call.
const CLAUSES: &[&str] = &[
    "PASSING",
    "RETURNING",
    "WITH",
    "WITHOUT",
    "KEEP",
    "OMIT",
    "ERROR",
    "NULL",
    "DEFAULT",
    "EMPTY",
    "TRUE",
    "FALSE",
    "UNKNOWN",
    "COLUMNS",
    "FORMAT",
    "AS",
    "ON",
];

/// A cursor over one call's argument tokens (whitespace skipped at the top level).
struct Cursor<'a> {
    t: &'a [Token],
    i: usize,
}

impl<'a> Cursor<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.t.len() && matches!(self.t[self.i], Token::Whitespace(_)) {
            self.i += 1;
        }
    }

    fn peek_word(&mut self, w: &str) -> bool {
        self.skip_ws();
        self.t.get(self.i).is_some_and(|t| is_word(t, w))
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.peek_word(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn eat(&mut self, tok: &Token) -> bool {
        self.skip_ws();
        if self.t.get(self.i) == Some(tok) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn done(&mut self) -> bool {
        self.skip_ws();
        self.i >= self.t.len()
    }

    /// An expression: tokens up to a top-level comma, `stop` keyword or the end.
    fn expr(&mut self, stop: &[&str]) -> Option<String> {
        self.skip_ws();
        let start = self.i;
        let mut depth = 0usize;
        while self.i < self.t.len() {
            match &self.t[self.i] {
                Token::LParen | Token::LBracket => depth += 1,
                // An unmatched `)` closes the enclosing list (a column definition).
                Token::RParen | Token::RBracket if depth == 0 => break,
                Token::RParen | Token::RBracket => depth -= 1,
                Token::Comma if depth == 0 => break,
                t if depth == 0 && stop.iter().any(|k| is_word(t, k)) && self.i > start => break,
                _ => {}
            }
            self.i += 1;
        }
        let s = text(&self.t[start..self.i]);
        (!s.trim().is_empty()).then(|| s.trim().to_owned())
    }

    /// A type name: tokens up to a top-level clause keyword or comma.
    fn type_name(&mut self) -> Option<String> {
        self.expr(&[
            "FORMAT", "WITH", "WITHOUT", "KEEP", "OMIT", "ERROR", "NULL", "DEFAULT", "EMPTY", "TRUE", "FALSE", "UNKNOWN", "PATH", "EXISTS",
            "ON",
        ])
    }
}

/// `{ERROR | NULL | DEFAULT expr | EMPTY [ARRAY|OBJECT] | TRUE | FALSE | UNKNOWN} ON EMPTY|ERROR`.
#[derive(Default, Clone)]
struct Behavior {
    code: Option<i32>,
    default: Option<String>,
}

#[derive(Default, Clone)]
struct Call {
    context: String,
    path: String,
    path_name: Option<String>,
    passing: Vec<(String, String)>,
    returning: Option<String>,
    wrapper: i32,
    omit_quotes: bool,
    on_empty: Behavior,
    on_error: Behavior,
    columns: Vec<Column>,
}

#[derive(Clone)]
enum Column {
    Ordinality(String),
    Regular {
        name: String,
        ty: String,
        path: Option<String>,
        exists: bool,
        format_json: bool,
        wrapper: i32,
        omit_quotes: bool,
        on_empty: Behavior,
        on_error: Behavior,
    },
    Nested {
        path: String,
        columns: Vec<Column>,
    },
}

/// One `<behaviour> ON EMPTY|ERROR` clause: (applies to EMPTY, behaviour), or None if the
/// tokens ahead are not one.
fn behavior(c: &mut Cursor) -> Option<Option<(bool, Behavior)>> {
    let save = c.i;
    let (code, default) = if c.eat_word("ERROR") {
        (ON_ERROR_RAISE, None)
    } else if c.eat_word("NULL") {
        (ON_NULL, None)
    } else if c.eat_word("TRUE") {
        (ON_TRUE, None)
    } else if c.eat_word("FALSE") {
        (ON_FALSE, None)
    } else if c.eat_word("UNKNOWN") {
        (ON_UNKNOWN, None)
    } else if c.eat_word("EMPTY") {
        if c.eat_word("OBJECT") {
            (ON_EMPTY_OBJECT, None)
        } else {
            c.eat_word("ARRAY");
            (ON_EMPTY_ARRAY, None)
        }
    } else if c.eat_word("DEFAULT") {
        (ON_DEFAULT, Some(c.expr(&["ON"])?))
    } else {
        return Some(None);
    };
    if !c.eat_word("ON") {
        c.i = save;
        return Some(None);
    }
    let on_empty = if c.eat_word("EMPTY") {
        true
    } else if c.eat_word("ERROR") {
        false
    } else {
        return None;
    };
    Some(Some((on_empty, Behavior { code: Some(code), default })))
}

/// Parses the shared clauses after the path, into `call` (or a JSON_TABLE column).
fn clauses(
    c: &mut Cursor,
    returning: &mut Option<String>,
    wrapper: &mut i32,
    omit: &mut bool,
    empty: &mut Behavior,
    error: &mut Behavior,
    format_json: &mut bool,
) -> Option<()> {
    loop {
        if c.eat_word("RETURNING") {
            *returning = Some(c.type_name()?);
        } else if c.eat_word("FORMAT") {
            c.eat_word("JSON");
            *format_json = true;
            if c.eat_word("ENCODING") {
                c.expr(CLAUSES)?;
            }
        } else if c.peek_word("WITH") || c.peek_word("WITHOUT") {
            let with = c.eat_word("WITH");
            if !with {
                c.eat_word("WITHOUT");
            }
            let conditional = c.eat_word("CONDITIONAL");
            c.eat_word("UNCONDITIONAL");
            c.eat_word("ARRAY");
            if !c.eat_word("WRAPPER") {
                return None;
            }
            *wrapper = match (with, conditional) {
                (false, _) => 0,
                (true, false) => 1,
                (true, true) => 2,
            };
        } else if c.peek_word("KEEP") || c.peek_word("OMIT") {
            *omit = c.eat_word("OMIT");
            if !*omit {
                c.eat_word("KEEP");
            }
            if !c.eat_word("QUOTES") {
                return None;
            }
            if c.eat_word("ON") {
                c.eat_word("SCALAR");
                c.eat_word("STRING");
            }
        } else {
            match behavior(c)? {
                None => return Some(()),
                Some((true, b)) => *empty = b,
                Some((false, b)) => *error = b,
            }
        }
    }
}

fn passing(c: &mut Cursor, call: &mut Call) -> Option<()> {
    if !c.eat_word("PASSING") {
        return Some(());
    }
    loop {
        let e = c.expr(&["AS"])?;
        if !c.eat_word("AS") {
            return None;
        }
        c.skip_ws();
        let name = match c.t.get(c.i)? {
            Token::Word(w) => {
                if w.quote_style.is_some() {
                    w.value.clone()
                } else {
                    w.value.to_lowercase()
                }
            }
            _ => return None,
        };
        c.i += 1;
        call.passing.push((name, e));
        if !c.eat(&Token::Comma) {
            return Some(());
        }
    }
}

fn columns(c: &mut Cursor) -> Option<Vec<Column>> {
    if !c.eat(&Token::LParen) {
        return None;
    }
    let mut out = Vec::new();
    loop {
        if c.eat_word("NESTED") {
            c.eat_word("PATH");
            let path = c.expr(&["AS", "COLUMNS"])?;
            if c.eat_word("AS") {
                c.expr(&["COLUMNS"])?;
            }
            if !c.eat_word("COLUMNS") {
                return None;
            }
            let cols = columns(c)?;
            out.push(Column::Nested { path, columns: cols });
        } else {
            c.skip_ws();
            let name = match c.t.get(c.i)? {
                Token::Word(w) => {
                    if w.quote_style.is_some() {
                        w.value.clone()
                    } else {
                        w.value.to_lowercase()
                    }
                }
                _ => return None,
            };
            c.i += 1;
            if c.eat_word("FOR") {
                if !c.eat_word("ORDINALITY") {
                    return None;
                }
                out.push(Column::Ordinality(name));
            } else {
                let ty = c.type_name()?;
                let exists = c.eat_word("EXISTS");
                let mut path = None;
                if c.eat_word("PATH") {
                    path = Some(c.expr(CLAUSES)?);
                }
                let (mut returning, mut wrapper, mut omit, mut empty, mut error, mut fj) =
                    (None, 0, false, Behavior::default(), Behavior::default(), false);
                clauses(c, &mut returning, &mut wrapper, &mut omit, &mut empty, &mut error, &mut fj)?;
                out.push(Column::Regular {
                    name,
                    ty,
                    path,
                    exists,
                    format_json: fj,
                    wrapper,
                    omit_quotes: omit,
                    on_empty: empty,
                    on_error: error,
                });
            }
        }
        if c.eat(&Token::Comma) {
            continue;
        }
        if c.eat(&Token::RParen) {
            return Some(out);
        }
        return None;
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl Call {
    fn parse(tokens: &[Token]) -> Option<Call> {
        let mut c = Cursor { t: tokens, i: 0 };
        let mut call = Call { context: c.expr(&["FORMAT"])?, ..Default::default() };
        if c.eat_word("FORMAT") {
            c.eat_word("JSON");
        }
        if !c.eat(&Token::Comma) {
            return None;
        }
        call.path = c.expr(CLAUSES)?;
        if c.eat_word("AS") {
            call.path_name = c.expr(CLAUSES);
        }
        passing(&mut c, &mut call)?;
        if c.eat_word("COLUMNS") {
            call.columns = columns(&mut c)?;
        }
        let (mut returning, mut wrapper, mut omit, mut empty, mut error, mut fj) =
            (None, 0, false, Behavior::default(), Behavior::default(), false);
        clauses(&mut c, &mut returning, &mut wrapper, &mut omit, &mut empty, &mut error, &mut fj)?;
        call.returning = returning;
        call.wrapper = wrapper;
        call.omit_quotes = omit;
        call.on_empty = empty;
        call.on_error = error;
        c.done().then_some(call)
    }

    fn vars(&self) -> String {
        if self.passing.is_empty() {
            return "NULL::jsonb".into();
        }
        let args: Vec<String> = self.passing.iter().map(|(n, e)| format!("{}, ({e})", quote(n))).collect();
        format!("jsonb_build_object({})", args.join(", "))
    }

    fn behavior_args(b: &Behavior, default_code: i32, ty: &str) -> String {
        let code = b.code.unwrap_or(default_code);
        let default = match &b.default {
            Some(e) => format!("CAST(({e}) AS {ty})"),
            None => format!("CAST(NULL AS {ty})"),
        };
        format!("{code}, {default}")
    }

    fn render(&self, name: &str) -> Option<String> {
        let ctx = &self.context;
        let path = &self.path;
        let vars = self.vars();
        Some(match name {
            "json_value" => {
                let ty = self.returning.clone().unwrap_or_else(|| "text".into());
                format!(
                    "__pgmem_json_value(({ctx}), ({path}), {vars}, CAST(NULL AS {ty}), {}, {})",
                    Self::behavior_args(&self.on_empty, ON_NULL, &ty),
                    Self::behavior_args(&self.on_error, ON_NULL, &ty)
                )
            }
            "json_query" => {
                let ty = self.returning.clone().unwrap_or_else(|| "jsonb".into());
                format!(
                    "__pgmem_json_query(({ctx}), ({path}), {vars}, CAST(NULL AS {ty}), {}, {}, {}, {})",
                    self.wrapper,
                    self.omit_quotes,
                    Self::behavior_args(&self.on_empty, ON_NULL, &ty),
                    Self::behavior_args(&self.on_error, ON_NULL, &ty)
                )
            }
            _ => {
                let ty = self.returning.clone().unwrap_or_else(|| "boolean".into());
                format!("__pgmem_json_exists(({ctx}), ({path}), {vars}, CAST(NULL AS {ty}), {})", self.on_error.code.unwrap_or(ON_FALSE))
            }
        })
    }

    /// `JSON_TABLE(...) alias`: an internal table function with the columns as a column
    /// definition list and their paths and behaviours as a JSON spec.
    fn json_table(&self, alias: &str) -> Option<String> {
        fn flat(cols: &[Column], defs: &mut Vec<String>, spec: &mut String) {
            spec.push('[');
            for (i, col) in cols.iter().enumerate() {
                if i > 0 {
                    spec.push(',');
                }
                match col {
                    Column::Ordinality(n) => {
                        defs.push(format!("\"{n}\" int4"));
                        spec.push_str("{\"k\":\"ord\"}");
                    }
                    Column::Regular { name, ty, path, exists, format_json, wrapper, omit_quotes, on_empty, on_error } => {
                        defs.push(format!("\"{name}\" {ty}"));
                        let p = path.clone().map_or_else(|| format!("$.\"{name}\""), |p| unquote(&p));
                        let kind = if *exists {
                            "exists"
                        } else if *format_json || is_json_type(ty) {
                            "query"
                        } else {
                            "value"
                        };
                        let opt = |v: &Option<i32>| v.map_or_else(|| "null".to_owned(), |c| c.to_string());
                        let _ = (&on_empty.default, &on_error.default);
                        spec.push_str(&format!(
                            "{{\"k\":\"{kind}\",\"p\":{},\"w\":{wrapper},\"q\":{omit_quotes},\"e\":{},\"r\":{}}}",
                            json_string(&p),
                            opt(&on_empty.code),
                            opt(&on_error.code)
                        ));
                    }
                    Column::Nested { path, columns } => {
                        spec.push_str(&format!("{{\"k\":\"nested\",\"p\":{},\"c\":", json_string(&unquote(path))));
                        flat(columns, defs, spec);
                        spec.push('}');
                    }
                }
            }
            spec.push(']');
        }
        let mut defs = Vec::new();
        let mut spec = String::new();
        flat(&self.columns, &mut defs, &mut spec);
        Some(format!(
            "__pgmem_json_table(({}), ({}), {}, {}, {}) AS {alias}({})",
            self.context,
            self.path,
            self.vars(),
            quote(&spec),
            self.on_error.code.unwrap_or(-1),
            defs.join(", ")
        ))
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn is_json_type(ty: &str) -> bool {
    let t = ty.trim().to_ascii_lowercase();
    t == "json" || t == "jsonb"
}

/// The text of a string literal argument (`'$.a'` → `$.a`).
fn unquote(s: &str) -> String {
    let t = s.trim();
    t.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')).map_or_else(|| t.to_owned(), |r| r.replace("''", "'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_json_table() {
        let out = rewrite("SELECT * FROM JSON_TABLE(jsonb '[{\"id\": 1}]', '$[*]' COLUMNS (id int PATH '$.id', ord FOR ORDINALITY))");
        let out = out.expect("rewritten");
        assert!(out.contains("__pgmem_json_table"), "{out}");
        assert!(out.contains("AS \"json_table\"(\"id\" int, \"ord\" int4)"), "{out}");
        let sql =
            r#"SELECT * FROM JSON_TABLE(jsonb '[{"id": 1, "name": "a"}]', '$[*]' COLUMNS (id int PATH '$.id', name text PATH '$.name'))"#;
        let out = rewrite(sql).expect("rewritten");
        assert!(out.contains("__pgmem_json_table"), "{out}");
        let stmts = crate::parse(sql).expect("parse");
        assert_eq!(stmts.len(), 1, "{out}");
    }
}
