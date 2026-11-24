//! SQL/JSON path language (`jsonpath`): a parser following Postgres' `jsonpath_gram.y`, the
//! canonical text form `jsonpath_out` prints, and an evaluator with `jsonpath_exec.c`'s lax
//! and strict semantics (automatic array unwrapping in lax mode, structural errors in strict
//! mode, three-valued predicates whose errors become `unknown`).

use crate::{Numeric, SqlError, SqlResult};
use serde_json::{Map, Number, Value as J};
use std::cmp::Ordering;
use std::fmt::Write as _;

// ---------------------------------------------------------------------------------------------
// Syntax tree
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct JsonPath {
    pub strict: bool,
    pub expr: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A primary followed by accessors.
    Path(Box<Primary>, Vec<Accessor>),
    Unary(char, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// A predicate used as an expression (a boolean-valued path).
    Pred(Box<Pred>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Primary {
    Root,
    Current,
    Last,
    Var(String),
    Literal(J),
    Paren(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Accessor {
    Key(String),
    AnyKey,
    AnyIndex,
    Indexes(Vec<(Expr, Option<Expr>)>),
    /// `.**`, `.**{n}`, `.**{n to m}` (u32::MAX = last).
    AnyLevel(u32, u32),
    Filter(Pred),
    Method(Method),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Method {
    Type,
    Size,
    Double,
    Ceiling,
    Floor,
    Abs,
    KeyValue,
    BigInt,
    Boolean,
    Decimal(Option<i32>, Option<i32>),
    Integer,
    Number,
    String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Pred {
    Cmp(CmpOp, Expr, Expr),
    And(Box<Pred>, Box<Pred>),
    Or(Box<Pred>, Box<Pred>),
    Not(Box<Pred>),
    Exists(Expr),
    IsUnknown(Box<Pred>),
    StartsWith(Expr, Expr),
    LikeRegex(Expr, String, String),
}

fn syntax_error(msg: impl Into<String>) -> SqlError {
    SqlError::new(crate::code::SYNTAX_ERROR, msg)
}

// ---------------------------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(String),
    Var(String),
    Punct(&'static str),
    End,
}

fn lex(src: &str) -> SqlResult<Vec<Tok>> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    let err = |what: &str| syntax_error(format!("syntax error at or near \"{what}\" of jsonpath input"));
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' {
            let mut s = String::new();
            i += 1;
            loop {
                match b.get(i) {
                    None => return Err(syntax_error("unterminated quoted string in jsonpath input")),
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some('\\') => {
                        i += 1;
                        match b.get(i) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('r') => s.push('\r'),
                            Some('b') => s.push('\u{8}'),
                            Some('f') => s.push('\u{c}'),
                            Some('v') => s.push('\u{b}'),
                            Some('u') => {
                                let hex: String = b.iter().skip(i + 1).take(4).collect();
                                let code = u32::from_str_radix(&hex, 16).map_err(|_| err("\\u"))?;
                                s.push(char::from_u32(code).ok_or_else(|| err("\\u"))?);
                                i += 4;
                            }
                            Some(other) => s.push(*other),
                            None => return Err(syntax_error("unterminated quoted string in jsonpath input")),
                        }
                        i += 1;
                    }
                    Some(other) => {
                        s.push(*other);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
            continue;
        }
        if c == '$' {
            i += 1;
            if b.get(i) == Some(&'"') {
                // $"name"
                let mut s = String::new();
                i += 1;
                while let Some(&ch) = b.get(i) {
                    i += 1;
                    if ch == '"' {
                        break;
                    }
                    s.push(ch);
                }
                out.push(Tok::Var(s));
            } else {
                let start = i;
                while b.get(i).is_some_and(|ch| ch.is_alphanumeric() || *ch == '_') {
                    i += 1;
                }
                if start == i {
                    out.push(Tok::Punct("$"));
                } else {
                    out.push(Tok::Var(b[start..i].iter().collect()));
                }
            }
            continue;
        }
        if c.is_ascii_digit()
            || (c == '.'
                && b.get(i + 1).is_some_and(char::is_ascii_digit)
                && !matches!(out.last(), Some(Tok::Punct(")" | "]")) | Some(Tok::Ident(_)) | Some(Tok::Str(_))))
        {
            let start = i;
            while b.get(i).is_some_and(char::is_ascii_digit) {
                i += 1;
            }
            if b.get(i) == Some(&'.') && b.get(i + 1).is_some_and(char::is_ascii_digit) {
                i += 1;
                while b.get(i).is_some_and(char::is_ascii_digit) {
                    i += 1;
                }
            }
            if matches!(b.get(i), Some('e' | 'E')) {
                let save = i;
                i += 1;
                if matches!(b.get(i), Some('+' | '-')) {
                    i += 1;
                }
                if b.get(i).is_some_and(char::is_ascii_digit) {
                    while b.get(i).is_some_and(char::is_ascii_digit) {
                        i += 1;
                    }
                } else {
                    i = save;
                }
            }
            if b.get(i).is_some_and(|ch| ch.is_alphabetic()) {
                return Err(syntax_error(format!(
                    "trailing junk after numeric literal at or near \"{}\" of jsonpath input",
                    b[start..=i].iter().collect::<String>()
                )));
            }
            out.push(Tok::Num(b[start..i].iter().collect()));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while b.get(i).is_some_and(|ch| ch.is_alphanumeric() || *ch == '_') {
                i += 1;
            }
            out.push(Tok::Ident(b[start..i].iter().collect()));
            continue;
        }
        let two: String = b.iter().skip(i).take(2).collect();
        let p = match two.as_str() {
            "==" => Some("=="),
            "!=" => Some("!="),
            "<>" => Some("<>"),
            "<=" => Some("<="),
            ">=" => Some(">="),
            "&&" => Some("&&"),
            "||" => Some("||"),
            "**" => Some("**"),
            _ => None,
        };
        if let Some(p) = p {
            out.push(Tok::Punct(p));
            i += 2;
            continue;
        }
        let p = match c {
            '.' => ".",
            '[' => "[",
            ']' => "]",
            '(' => "(",
            ')' => ")",
            '{' => "{",
            '}' => "}",
            ',' => ",",
            '?' => "?",
            '@' => "@",
            '<' => "<",
            '>' => ">",
            '!' => "!",
            '+' => "+",
            '-' => "-",
            '*' => "*",
            '/' => "/",
            '%' => "%",
            ':' => ":",
            _ => return Err(err(&c.to_string())),
        };
        out.push(Tok::Punct(p));
        i += 1;
    }
    out.push(Tok::End);
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.i]
    }

    fn peek_at(&self, n: usize) -> &Tok {
        self.toks.get(self.i + n).unwrap_or(&Tok::End)
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.i].clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn is_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(q) if *q == p)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == w)
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        if self.is_punct(p) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, p: &str) -> SqlResult<()> {
        if self.eat_punct(p) { Ok(()) } else { Err(self.unexpected()) }
    }

    fn unexpected(&self) -> SqlError {
        match self.peek() {
            Tok::End => syntax_error("syntax error at end of jsonpath input"),
            t => syntax_error(format!("syntax error at or near \"{}\" of jsonpath input", tok_text(t))),
        }
    }

    // expr_or_predicate
    fn top(&mut self) -> SqlResult<Expr> {
        let e = self.or_pred()?;
        if *self.peek() != Tok::End {
            return Err(self.unexpected());
        }
        Ok(e)
    }

    /// predicate-or-expression at the `||` level.
    fn or_pred(&mut self) -> SqlResult<Expr> {
        let mut l = self.and_pred()?;
        while self.is_punct("||") {
            let lp = as_pred(l).map_err(|_| self.unexpected())?;
            self.next();
            let r = self.and_pred()?;
            let rp = as_pred(r).map_err(|_| self.unexpected())?;
            l = Expr::Pred(Box::new(Pred::Or(Box::new(lp), Box::new(rp))));
        }
        Ok(l)
    }

    fn and_pred(&mut self) -> SqlResult<Expr> {
        let mut l = self.not_pred()?;
        while self.is_punct("&&") {
            let lp = as_pred(l).map_err(|_| self.unexpected())?;
            self.next();
            let r = self.not_pred()?;
            let rp = as_pred(r).map_err(|_| self.unexpected())?;
            l = Expr::Pred(Box::new(Pred::And(Box::new(lp), Box::new(rp))));
        }
        Ok(l)
    }

    fn not_pred(&mut self) -> SqlResult<Expr> {
        if self.is_punct("!") {
            self.next();
            self.expect_punct("(")?;
            let inner = self.or_pred()?;
            self.expect_punct(")")?;
            return Ok(Expr::Pred(Box::new(Pred::Not(Box::new(as_pred(inner)?)))));
        }
        self.cmp_pred()
    }

    fn cmp_pred(&mut self) -> SqlResult<Expr> {
        if self.is_word("exists") && matches!(self.peek_at(1), Tok::Punct("(")) {
            self.next();
            self.next();
            let e = self.or_pred()?;
            self.expect_punct(")")?;
            return self.is_unknown(Pred::Exists(e));
        }
        let l = self.additive()?;
        let op = match self.peek() {
            Tok::Punct("==") => Some(CmpOp::Eq),
            Tok::Punct("!=" | "<>") => Some(CmpOp::Ne),
            Tok::Punct("<") => Some(CmpOp::Lt),
            Tok::Punct("<=") => Some(CmpOp::Le),
            Tok::Punct(">") => Some(CmpOp::Gt),
            Tok::Punct(">=") => Some(CmpOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.next();
            let r = self.additive()?;
            return self.is_unknown(Pred::Cmp(op, l, r));
        }
        if self.is_word("starts") && matches!(self.peek_at(1), Tok::Ident(w) if w == "with") {
            self.next();
            self.next();
            let r = self.additive()?;
            return self.is_unknown(Pred::StartsWith(l, r));
        }
        if self.is_word("like_regex") {
            self.next();
            let Tok::Str(pattern) = self.next() else { return Err(self.unexpected()) };
            let mut flags = String::new();
            if self.is_word("flag") {
                self.next();
                let Tok::Str(f) = self.next() else { return Err(self.unexpected()) };
                for ch in f.chars() {
                    if !"ismxq".contains(ch) {
                        return Err(syntax_error("invalid input syntax for type jsonpath")
                            .with_detail(format!("Unrecognized flag character \"{ch}\" in LIKE_REGEX predicate.")));
                    }
                }
                flags = f;
            }
            like_regex(&pattern, &flags).map_err(|e| syntax_error(e.message))?;
            return self.is_unknown(Pred::LikeRegex(l, pattern, flags));
        }
        if let Expr::Pred(p) = l {
            return self.is_unknown(*p);
        }
        // `(predicate) is unknown`
        if self.is_word("is")
            && let Expr::Path(p, acc) = &l
            && acc.is_empty()
            && let Primary::Paren(Expr::Pred(inner)) = p.as_ref()
        {
            return self.is_unknown((**inner).clone());
        }
        Ok(l)
    }

    /// `(predicate) is unknown`.
    fn is_unknown(&mut self, p: Pred) -> SqlResult<Expr> {
        if self.is_word("is") && matches!(self.peek_at(1), Tok::Ident(w) if w == "unknown") {
            self.next();
            self.next();
            return Ok(Expr::Pred(Box::new(Pred::IsUnknown(Box::new(p)))));
        }
        Ok(Expr::Pred(Box::new(p)))
    }

    fn additive(&mut self) -> SqlResult<Expr> {
        let mut l = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Punct("+") => BinOp::Add,
                Tok::Punct("-") => BinOp::Sub,
                _ => return Ok(l),
            };
            self.next();
            let r = self.multiplicative()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
    }

    fn multiplicative(&mut self) -> SqlResult<Expr> {
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Punct("*") => BinOp::Mul,
                Tok::Punct("/") => BinOp::Div,
                Tok::Punct("%") => BinOp::Mod,
                _ => return Ok(l),
            };
            self.next();
            let r = self.unary()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> SqlResult<Expr> {
        match self.peek() {
            Tok::Punct("-") => {
                self.next();
                let e = self.unary()?;
                // A negated numeric literal stays a literal.
                if let Expr::Path(p, acc) = &e
                    && acc.is_empty()
                    && let Primary::Literal(J::Number(n)) = p.as_ref()
                {
                    let neg = format!("-{n}");
                    return Ok(Expr::Path(Box::new(Primary::Literal(J::Number(neg.parse().unwrap_or_else(|_| n.clone())))), vec![]));
                }
                Ok(Expr::Unary('-', Box::new(e)))
            }
            Tok::Punct("+") => {
                self.next();
                let e = self.unary()?;
                Ok(Expr::Unary('+', Box::new(e)))
            }
            _ => self.accessor_expr(),
        }
    }

    fn accessor_expr(&mut self) -> SqlResult<Expr> {
        let primary = match self.next() {
            Tok::Punct("$") => Primary::Root,
            Tok::Punct("@") => Primary::Current,
            Tok::Var(v) => Primary::Var(v),
            Tok::Str(s) => Primary::Literal(J::String(s)),
            Tok::Num(n) => Primary::Literal(J::Number(parse_number(&n)?)),
            Tok::Ident(w) if w == "true" => Primary::Literal(J::Bool(true)),
            Tok::Ident(w) if w == "false" => Primary::Literal(J::Bool(false)),
            Tok::Ident(w) if w == "null" => Primary::Literal(J::Null),
            Tok::Ident(w) if w == "last" => Primary::Last,
            Tok::Punct("(") => {
                let e = self.or_pred()?;
                self.expect_punct(")")?;
                Primary::Paren(e)
            }
            _ => {
                self.i -= 1;
                return Err(self.unexpected());
            }
        };
        let mut accessors = Vec::new();
        loop {
            if self.is_punct(".") {
                self.next();
                match self.next() {
                    Tok::Punct("*") => accessors.push(Accessor::AnyKey),
                    Tok::Punct("**") => accessors.push(self.any_level()?),
                    Tok::Str(s) => accessors.push(Accessor::Key(s)),
                    Tok::Var(v) => accessors.push(Accessor::Key(format!("${v}"))),
                    Tok::Ident(w) => {
                        if self.is_punct("(") {
                            accessors.push(Accessor::Method(self.method(&w)?));
                        } else {
                            accessors.push(Accessor::Key(w));
                        }
                    }
                    _ => {
                        self.i -= 1;
                        return Err(self.unexpected());
                    }
                }
            } else if self.is_punct("[") {
                self.next();
                if self.eat_punct("*") {
                    self.expect_punct("]")?;
                    accessors.push(Accessor::AnyIndex);
                } else {
                    let mut list = Vec::new();
                    loop {
                        let from = self.additive()?;
                        let to = if self.is_word("to") {
                            self.next();
                            Some(self.additive()?)
                        } else {
                            None
                        };
                        list.push((from, to));
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                    self.expect_punct("]")?;
                    accessors.push(Accessor::Indexes(list));
                }
            } else if self.is_punct("?") {
                self.next();
                self.expect_punct("(")?;
                let p = self.or_pred()?;
                self.expect_punct(")")?;
                accessors.push(Accessor::Filter(as_pred(p)?));
            } else {
                break;
            }
        }
        Ok(Expr::Path(Box::new(primary), accessors))
    }

    fn any_level(&mut self) -> SqlResult<Accessor> {
        if !self.eat_punct("{") {
            return Ok(Accessor::AnyLevel(0, u32::MAX));
        }
        let level = |p: &mut Parser| -> SqlResult<u32> {
            match p.next() {
                Tok::Num(n) => n.parse().map_err(|_| syntax_error("invalid jsonpath level")),
                Tok::Ident(w) if w == "last" => Ok(u32::MAX),
                _ => Err(syntax_error("syntax error in jsonpath level")),
            }
        };
        let from = level(self)?;
        let to = if self.is_word("to") {
            self.next();
            level(self)?
        } else {
            from
        };
        self.expect_punct("}")?;
        Ok(Accessor::AnyLevel(from, to))
    }

    fn method(&mut self, name: &str) -> SqlResult<Method> {
        self.expect_punct("(")?;
        let m = match name {
            "type" => Method::Type,
            "size" => Method::Size,
            "double" => Method::Double,
            "ceiling" => Method::Ceiling,
            "floor" => Method::Floor,
            "abs" => Method::Abs,
            "keyvalue" => Method::KeyValue,
            "bigint" => Method::BigInt,
            "boolean" => Method::Boolean,
            "integer" => Method::Integer,
            "number" => Method::Number,
            "string" => Method::String,
            "decimal" => {
                let int = |p: &mut Parser| -> SqlResult<Option<i32>> {
                    let neg = p.eat_punct("-");
                    match p.peek() {
                        Tok::Num(n) => {
                            let v: i32 = n.parse().map_err(|_| syntax_error("invalid decimal precision"))?;
                            p.next();
                            Ok(Some(if neg { -v } else { v }))
                        }
                        _ => Ok(None),
                    }
                };
                let p = int(self)?;
                let s = if self.eat_punct(",") { int(self)? } else { None };
                Method::Decimal(p, s)
            }
            other => return Err(syntax_error(format!("syntax error at or near \"{other}\" of jsonpath input"))),
        };
        self.expect_punct(")")?;
        Ok(m)
    }
}

fn tok_text(t: &Tok) -> String {
    match t {
        Tok::Ident(s) | Tok::Num(s) => s.clone(),
        Tok::Str(s) => format!("\"{s}\""),
        Tok::Var(v) => format!("${v}"),
        Tok::Punct(p) => (*p).to_owned(),
        Tok::End => String::new(),
    }
}

fn as_pred(e: Expr) -> SqlResult<Pred> {
    match e {
        Expr::Pred(p) => Ok(*p),
        Expr::Path(p, acc) if acc.is_empty() => match *p {
            Primary::Paren(inner) => as_pred(inner),
            other => Err(syntax_error(format!("syntax error at or near \"{}\" of jsonpath input", primary_text(&other)))),
        },
        _ => Err(syntax_error("syntax error in jsonpath predicate")),
    }
}

fn primary_text(p: &Primary) -> String {
    let mut s = String::new();
    write_primary(&mut s, p);
    s
}

fn parse_number(n: &str) -> SqlResult<Number> {
    let num = Numeric::parse(n).map_err(|_| syntax_error(format!("invalid numeric literal \"{n}\"")))?;
    num.to_string().parse().map_err(|_| syntax_error(format!("invalid numeric literal \"{n}\"")))
}

/// Parses jsonpath text.
pub fn parse(src: &str) -> SqlResult<JsonPath> {
    let mut toks = lex(src)?;
    let mut strict = false;
    if let Some(Tok::Ident(w)) = toks.first() {
        if w == "strict" {
            strict = true;
            toks.remove(0);
        } else if w == "lax" {
            toks.remove(0);
        }
    }
    let mut p = Parser { toks, i: 0 };
    let expr = p.top()?;
    check_context(&expr, false, false)?;
    Ok(JsonPath { strict, expr })
}

/// `flattenJsonPathParseItem`'s checks: `@` only inside filters, `last` only in subscripts.
fn check_context(e: &Expr, in_filter: bool, in_subscript: bool) -> SqlResult<()> {
    let pred = |p: &Pred, f: bool, s: bool| -> SqlResult<()> {
        match p {
            Pred::Cmp(_, l, r) | Pred::StartsWith(l, r) => {
                check_context(l, f, s)?;
                check_context(r, f, s)
            }
            Pred::And(l, r) | Pred::Or(l, r) => {
                check_context(&Expr::Pred(l.clone()), f, s)?;
                check_context(&Expr::Pred(r.clone()), f, s)
            }
            Pred::Not(i) | Pred::IsUnknown(i) => check_context(&Expr::Pred(i.clone()), f, s),
            Pred::Exists(x) | Pred::LikeRegex(x, ..) => check_context(x, f, s),
        }
    };
    match e {
        Expr::Path(p, acc) => {
            match p.as_ref() {
                Primary::Current if !in_filter => return Err(syntax_error("@ is not allowed in root expressions")),
                Primary::Last if !in_subscript => return Err(syntax_error("LAST is allowed only in array subscripts")),
                Primary::Paren(inner) => check_context(inner, in_filter, in_subscript)?,
                _ => {}
            }
            for a in acc {
                match a {
                    Accessor::Filter(p) => pred(p, true, in_subscript)?,
                    Accessor::Indexes(list) => {
                        for (from, to) in list {
                            check_context(from, in_filter, true)?;
                            if let Some(t) = to {
                                check_context(t, in_filter, true)?;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(())
        }
        Expr::Unary(_, i) => check_context(i, in_filter, in_subscript),
        Expr::Binary(_, l, r) => {
            check_context(l, in_filter, in_subscript)?;
            check_context(r, in_filter, in_subscript)
        }
        Expr::Pred(p) => pred(p, in_filter, in_subscript),
    }
}

// ---------------------------------------------------------------------------------------------
// Canonical text (jsonpath_out)
// ---------------------------------------------------------------------------------------------

fn write_string(out: &mut String, s: &str) {
    crate::json::write_json_string(out, s);
}

fn write_primary(out: &mut String, p: &Primary) {
    match p {
        Primary::Root => out.push('$'),
        Primary::Current => out.push('@'),
        Primary::Last => out.push_str("last"),
        Primary::Var(v) => {
            out.push('$');
            write_string(out, v);
        }
        Primary::Literal(J::String(s)) => write_string(out, s),
        Primary::Literal(v) => out.push_str(&v.to_string()),
        Primary::Paren(e) => write_expr(out, e, false),
    }
}

fn method_name(m: &Method) -> String {
    match m {
        Method::Type => "type()".into(),
        Method::Size => "size()".into(),
        Method::Double => "double()".into(),
        Method::Ceiling => "ceiling()".into(),
        Method::Floor => "floor()".into(),
        Method::Abs => "abs()".into(),
        Method::KeyValue => "keyvalue()".into(),
        Method::BigInt => "bigint()".into(),
        Method::Boolean => "boolean()".into(),
        Method::Integer => "integer()".into(),
        Method::Number => "number()".into(),
        Method::String => "string()".into(),
        Method::Decimal(p, s) => match (p, s) {
            (Some(p), Some(s)) => format!("decimal({p},{s})"),
            (Some(p), None) => format!("decimal({p})"),
            _ => "decimal()".into(),
        },
    }
}

fn write_accessor(out: &mut String, a: &Accessor) {
    match a {
        Accessor::Key(k) => {
            out.push('.');
            write_string(out, k);
        }
        Accessor::AnyKey => out.push_str(".*"),
        Accessor::AnyIndex => out.push_str("[*]"),
        Accessor::Indexes(list) => {
            out.push('[');
            for (i, (from, to)) in list.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_expr(out, from, false);
                if let Some(t) = to {
                    out.push_str(" to ");
                    write_expr(out, t, false);
                }
            }
            out.push(']');
        }
        Accessor::AnyLevel(from, to) => {
            out.push_str(".**");
            let lvl = |n: u32| if n == u32::MAX { "last".to_owned() } else { n.to_string() };
            if !(*from == 0 && *to == u32::MAX) {
                if from == to {
                    let _ = write!(out, "{{{}}}", lvl(*from));
                } else {
                    let _ = write!(out, "{{{} to {}}}", lvl(*from), lvl(*to));
                }
            }
        }
        Accessor::Filter(p) => {
            out.push_str("?(");
            write_pred(out, p, false);
            out.push(')');
        }
        Accessor::Method(m) => {
            out.push('.');
            out.push_str(&method_name(m));
        }
    }
}

/// `operationPriority`: how tightly an item binds, for parenthesizing operands.
fn expr_priority(e: &Expr) -> u8 {
    match e {
        Expr::Path(p, acc) if acc.is_empty() => match p.as_ref() {
            Primary::Paren(inner) => expr_priority(inner),
            _ => 6,
        },
        Expr::Path(..) => 6,
        Expr::Binary(BinOp::Add | BinOp::Sub, ..) => 3,
        Expr::Binary(..) => 4,
        Expr::Unary(..) => 5,
        Expr::Pred(p) => pred_priority(p),
    }
}

fn pred_priority(p: &Pred) -> u8 {
    match p {
        Pred::Or(..) => 0,
        Pred::And(..) => 1,
        Pred::Cmp(..) | Pred::StartsWith(..) => 2,
        _ => 6,
    }
}

/// `printJsonPathItem`: `brackets` asks for parentheses around a lower-priority operand.
fn write_expr(out: &mut String, e: &Expr, brackets: bool) {
    match e {
        Expr::Path(p, acc) => {
            if let (Primary::Paren(inner), false) = (p.as_ref(), acc.is_empty()) {
                out.push('(');
                write_expr(out, inner, false);
                out.push(')');
            } else if let (Primary::Paren(inner), true) = (p.as_ref(), acc.is_empty()) {
                write_expr(out, inner, brackets);
            } else {
                write_primary(out, p);
            }
            for a in acc {
                write_accessor(out, a);
            }
        }
        Expr::Unary(op, inner) => {
            if brackets {
                out.push('(');
            }
            out.push(*op);
            write_expr(out, inner, expr_priority(inner) <= 5);
            if brackets {
                out.push(')');
            }
        }
        Expr::Binary(op, l, r) => {
            let prio = expr_priority(e);
            if brackets {
                out.push('(');
            }
            write_expr(out, l, expr_priority(l) <= prio);
            out.push_str(match op {
                BinOp::Add => " + ",
                BinOp::Sub => " - ",
                BinOp::Mul => " * ",
                BinOp::Div => " / ",
                BinOp::Mod => " % ",
            });
            write_expr(out, r, expr_priority(r) <= prio);
            if brackets {
                out.push(')');
            }
        }
        Expr::Pred(p) => write_pred(out, p, brackets),
    }
}

fn write_pred(out: &mut String, p: &Pred, brackets: bool) {
    let prio = pred_priority(p);
    match p {
        Pred::Cmp(_, l, r) | Pred::StartsWith(l, r) => {
            if brackets {
                out.push('(');
            }
            write_expr(out, l, expr_priority(l) <= prio);
            out.push_str(match p {
                Pred::Cmp(CmpOp::Eq, ..) => " == ",
                Pred::Cmp(CmpOp::Ne, ..) => " != ",
                Pred::Cmp(CmpOp::Lt, ..) => " < ",
                Pred::Cmp(CmpOp::Le, ..) => " <= ",
                Pred::Cmp(CmpOp::Gt, ..) => " > ",
                Pred::Cmp(CmpOp::Ge, ..) => " >= ",
                _ => " starts with ",
            });
            write_expr(out, r, expr_priority(r) <= prio);
            if brackets {
                out.push(')');
            }
        }
        Pred::And(l, r) | Pred::Or(l, r) => {
            if brackets {
                out.push('(');
            }
            write_pred(out, l, pred_priority(l) <= prio);
            out.push_str(if matches!(p, Pred::And(..)) { " && " } else { " || " });
            write_pred(out, r, pred_priority(r) <= prio);
            if brackets {
                out.push(')');
            }
        }
        Pred::Not(inner) => {
            out.push_str("!(");
            write_pred(out, inner, false);
            out.push(')');
        }
        Pred::Exists(e) => {
            out.push_str("exists (");
            write_expr(out, e, false);
            out.push(')');
        }
        Pred::IsUnknown(inner) => {
            out.push('(');
            write_pred(out, inner, false);
            out.push_str(") is unknown");
        }
        Pred::LikeRegex(l, pattern, flags) => {
            write_expr(out, l, expr_priority(l) <= prio);
            out.push_str(" like_regex ");
            write_string(out, pattern);
            if !flags.is_empty() {
                out.push_str(" flag ");
                write_string(out, flags);
            }
        }
    }
}

impl std::fmt::Display for JsonPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        if self.strict {
            out.push_str("strict ");
        }
        // jsonpath_out prints the top item with brackets: `($."a" + 1)`, `($ == 1)`.
        write_expr(&mut out, &self.expr, true);
        f.write_str(&out)
    }
}

/// `jsonpath_in` followed by `jsonpath_out`: the canonical text of a path.
pub fn normalize(src: &str) -> SqlResult<String> {
    Ok(parse(src)?.to_string())
}

// ---------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------

/// An evaluation error: SQLSTATE and message. In predicates these become `unknown`; with
/// `silent` they become an empty result.
fn jerr(code: &'static str, msg: impl Into<String>) -> SqlError {
    SqlError::new(code, msg)
}

const NO_KEY: &str = "2203A";
const NOT_ARRAY: &str = "22039";
const NOT_OBJECT: &str = "2203C";
const ARRAY_INDEX: &str = "22033";
const NOT_NUMERIC: &str = "2203B";
const NON_NUMERIC_ITEM: &str = "22036";
const SINGLETON: &str = "22038";

pub struct Evaluator<'a> {
    pub strict: bool,
    pub root: &'a J,
    pub vars: &'a Map<String, J>,
}

type Seq = Vec<J>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tri {
    True,
    False,
    Unknown,
}

fn num_of(v: &J) -> Option<Numeric> {
    match v {
        J::Number(n) => Numeric::parse(&n.to_string()).ok(),
        _ => None,
    }
}

fn to_json_number(n: &Numeric) -> J {
    n.to_string().parse::<Number>().map_or(J::Null, J::Number)
}

fn type_name(v: &J) -> &'static str {
    match v {
        J::Null => "null",
        J::Bool(_) => "boolean",
        J::Number(_) => "number",
        J::String(_) => "string",
        J::Array(_) => "array",
        J::Object(_) => "object",
    }
}

impl<'a> Evaluator<'a> {
    /// Evaluates a path to its sequence of items.
    pub fn eval(&self, path: &JsonPath) -> SqlResult<Seq> {
        self.expr(&path.expr, self.root, None)
    }

    fn lax(&self) -> bool {
        !self.strict
    }

    fn expr(&self, e: &Expr, current: &J, last: Option<i64>) -> SqlResult<Seq> {
        match e {
            Expr::Path(p, accessors) => {
                let mut items = self.primary(p, current, last)?;
                // After `.**`, structural errors are ignored (`ignoreStructuralErrors`).
                let mut lenient = false;
                for a in accessors {
                    items = if lenient {
                        let relaxed = Evaluator { strict: false, root: self.root, vars: self.vars };
                        relaxed.accessor_no_unwrap(a, items, last)?
                    } else {
                        self.accessor(a, items, last)?
                    };
                    lenient |= matches!(a, Accessor::AnyLevel(..));
                }
                Ok(items)
            }
            Expr::Unary(op, inner) => {
                let items = self.expr(inner, current, last)?;
                let mut out = Vec::new();
                for v in self.unwrap_all(items) {
                    let Some(n) = num_of(&v) else {
                        return Err(jerr(NOT_NUMERIC, format!("operand of unary jsonpath operator {op} is not a numeric value")));
                    };
                    out.push(to_json_number(&if *op == '-' { n.neg() } else { n }));
                }
                Ok(out)
            }
            Expr::Binary(op, l, r) => {
                let l = self.singleton_number(self.expr(l, current, last)?, op, true)?;
                let r = self.singleton_number(self.expr(r, current, last)?, op, false)?;
                let v = match op {
                    BinOp::Add => l.add(&r),
                    BinOp::Sub => l.sub(&r),
                    BinOp::Mul => l.mul(&r),
                    BinOp::Div => l.div(&r)?,
                    BinOp::Mod => l.rem(&r)?,
                };
                Ok(vec![to_json_number(&v)])
            }
            Expr::Pred(p) => Ok(vec![match self.pred(p, current, last) {
                Tri::True => J::Bool(true),
                Tri::False => J::Bool(false),
                Tri::Unknown => J::Null,
            }]),
        }
    }

    fn singleton_number(&self, items: Seq, op: &BinOp, left: bool) -> SqlResult<Numeric> {
        let items = if self.lax() { self.unwrap_all(items) } else { items };
        let sym = match op {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
        };
        let side = if left { "left" } else { "right" };
        match items.as_slice() {
            [v] => {
                num_of(v).ok_or_else(|| jerr(SINGLETON, format!("{side} operand of jsonpath operator {sym} is not a single numeric value")))
            }
            _ => Err(jerr(SINGLETON, format!("{side} operand of jsonpath operator {sym} is not a single numeric value"))),
        }
    }

    fn primary(&self, p: &Primary, current: &J, last: Option<i64>) -> SqlResult<Seq> {
        Ok(match p {
            Primary::Root => vec![self.root.clone()],
            Primary::Current => vec![current.clone()],
            Primary::Last => match last {
                Some(l) => vec![J::Number(Number::from(l))],
                None => return Err(jerr("22000", "evaluating jsonpath LAST outside of array subscript")),
            },
            Primary::Var(name) => match self.vars.get(name) {
                Some(v) => vec![v.clone()],
                None => return Err(jerr("42704", format!("could not find jsonpath variable \"{name}\""))),
            },
            Primary::Literal(v) => vec![v.clone()],
            Primary::Paren(e) => self.expr(e, current, last)?,
        })
    }

    /// Lax mode's automatic unwrapping of arrays one level deep.
    fn unwrap_all(&self, items: Seq) -> Seq {
        let mut out = Vec::new();
        for v in items {
            match v {
                J::Array(a) => out.extend(a),
                other => out.push(other),
            }
        }
        out
    }

    fn accessor(&self, a: &Accessor, items: Seq, last: Option<i64>) -> SqlResult<Seq> {
        let mut out = Vec::new();
        match a {
            Accessor::Key(k) => {
                let items = if self.lax() { self.unwrap_all(items) } else { items };
                for v in items {
                    match v {
                        J::Object(m) => match m.get(k) {
                            Some(x) => out.push(x.clone()),
                            None if self.strict => return Err(jerr(NO_KEY, format!("JSON object does not contain key \"{k}\""))),
                            None => {}
                        },
                        _ if self.strict => return Err(jerr(NOT_OBJECT, "jsonpath member accessor can only be applied to an object")),
                        _ => {}
                    }
                }
            }
            Accessor::AnyKey => {
                let items = if self.lax() { self.unwrap_all(items) } else { items };
                for v in items {
                    match v {
                        J::Object(m) => out.extend(m.into_iter().map(|(_, v)| v)),
                        _ if self.strict => {
                            return Err(jerr(NOT_OBJECT, "jsonpath wildcard member accessor can only be applied to an object"));
                        }
                        _ => {}
                    }
                }
            }
            Accessor::AnyIndex => {
                for v in items {
                    match v {
                        J::Array(a) => out.extend(a),
                        other if self.lax() => out.push(other),
                        _ => return Err(jerr(NOT_ARRAY, "jsonpath wildcard array accessor can only be applied to an array")),
                    }
                }
            }
            Accessor::Indexes(list) => {
                for v in items {
                    let arr = match v {
                        J::Array(a) => a,
                        other if self.lax() => vec![other],
                        _ => return Err(jerr(NOT_ARRAY, "jsonpath array accessor can only be applied to an array")),
                    };
                    let size = i64::try_from(arr.len()).unwrap_or(i64::MAX);
                    let last_idx = size - 1;
                    for (from, to) in list {
                        let lo = self.index(from, &arr, last_idx)?;
                        let hi = match to {
                            Some(t) => self.index(t, &arr, last_idx)?,
                            None => lo,
                        };
                        if self.strict && (lo < 0 || lo > last_idx || hi < 0 || hi > last_idx || lo > hi) {
                            return Err(jerr(ARRAY_INDEX, "jsonpath array subscript is out of bounds"));
                        }
                        let (lo, hi) = (lo.max(0), hi.min(last_idx));
                        let mut i = lo;
                        while i <= hi {
                            if let Some(x) = usize::try_from(i).ok().and_then(|i| arr.get(i)) {
                                out.push(x.clone());
                            }
                            i += 1;
                        }
                    }
                }
                let _ = last;
            }
            Accessor::AnyLevel(from, to) => {
                for v in items {
                    self.any_level(&v, 0, *from, *to, &mut out);
                }
            }
            Accessor::Filter(p) => {
                let items = if self.lax() { self.unwrap_all(items) } else { items };
                for v in items {
                    if self.pred(p, &v, last) == Tri::True {
                        out.push(v);
                    }
                }
            }
            Accessor::Method(m) => {
                for v in items {
                    self.method(m, v, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    /// An accessor applied without lax unwrapping, skipping items it doesn't apply to.
    fn accessor_no_unwrap(&self, a: &Accessor, items: Seq, last: Option<i64>) -> SqlResult<Seq> {
        match a {
            Accessor::Key(k) => Ok(items.into_iter().filter_map(|v| if let J::Object(m) = v { m.get(k).cloned() } else { None }).collect()),
            Accessor::AnyKey => Ok(items
                .into_iter()
                .flat_map(|v| if let J::Object(m) = v { m.into_iter().map(|(_, v)| v).collect() } else { Vec::new() })
                .collect()),
            _ => self.accessor(a, items, last),
        }
    }

    fn index(&self, e: &Expr, arr: &[J], last: i64) -> SqlResult<i64> {
        let cur = J::Array(arr.to_vec());
        let items = self.expr(e, &cur, Some(last))?;
        match items.as_slice() {
            [J::Number(n)] => {
                let f = Numeric::parse(&n.to_string())
                    .map_err(|_| jerr(ARRAY_INDEX, "jsonpath array subscript is not a single numeric value"))?;
                f.trunc(0).to_i64().ok_or_else(|| jerr(ARRAY_INDEX, "jsonpath array subscript is out of integer range"))
            }
            _ => Err(jerr(ARRAY_INDEX, "jsonpath array subscript is not a single numeric value")),
        }
    }

    fn any_level(&self, v: &J, level: u32, from: u32, to: u32, out: &mut Seq) {
        if level >= from && level <= to {
            out.push(v.clone());
        }
        if level >= to {
            return;
        }
        match v {
            J::Object(m) => m.values().for_each(|c| self.any_level(c, level + 1, from, to, out)),
            J::Array(a) => a.iter().for_each(|c| self.any_level(c, level + 1, from, to, out)),
            _ => {}
        }
    }

    fn method(&self, m: &Method, v: J, out: &mut Seq) -> SqlResult<()> {
        // Lax mode unwraps arrays for every method but type() and size().
        if self.lax()
            && !matches!(m, Method::Type | Method::Size)
            && let J::Array(items) = v
        {
            for i in items {
                self.method(m, i, out)?;
            }
            return Ok(());
        }
        let name = method_name(m);
        let name = name.trim_end_matches("()");
        let numeric_required = || jerr(NON_NUMERIC_ITEM, format!("jsonpath item method .{name}() can only be applied to a numeric value"));
        match m {
            Method::Type => out.push(J::String(type_name(&v).into())),
            Method::Size => out.push(J::Number(Number::from(match &v {
                J::Array(a) => a.len() as u64,
                _ if self.lax() => 1,
                _ => return Err(jerr(NOT_ARRAY, "jsonpath item method .size() can only be applied to an array")),
            }))),
            Method::Abs | Method::Floor | Method::Ceiling => {
                let n = num_of(&v).ok_or_else(numeric_required)?;
                out.push(to_json_number(&match m {
                    Method::Abs => n.abs(),
                    Method::Floor => n.floor(),
                    _ => n.ceil(),
                }));
            }
            Method::Double | Method::Number | Method::Decimal(..) | Method::Integer | Method::BigInt => {
                let n = match &v {
                    J::Number(_) => num_of(&v).ok_or_else(numeric_required)?,
                    J::String(s) => Numeric::parse(s.trim()).map_err(|_| {
                        jerr(
                            NON_NUMERIC_ITEM,
                            format!("argument \"{s}\" of jsonpath item method .{name}() is invalid for type {}", target_type(m)),
                        )
                    })?,
                    _ => {
                        return Err(jerr(
                            NON_NUMERIC_ITEM,
                            format!("jsonpath item method .{name}() can only be applied to a string or numeric value"),
                        ));
                    }
                };
                let n = match m {
                    Method::Integer | Method::BigInt => n.round(0),
                    Method::Decimal(Some(p), s) => n.apply_typmod(*p, s.unwrap_or(0)).map_err(|_| {
                        jerr(NON_NUMERIC_ITEM, format!("argument \"{n}\" of jsonpath item method .{name}() is invalid for type numeric"))
                    })?,
                    Method::Double => Numeric::from_f64(n.to_f64())?,
                    _ => n,
                };
                out.push(to_json_number(&n));
            }
            Method::String => out.push(J::String(match v {
                J::String(s) => s,
                J::Number(n) => n.to_string(),
                J::Bool(b) => b.to_string(),
                _ => {
                    return Err(jerr(
                        NON_NUMERIC_ITEM,
                        "jsonpath item method .string() can only be applied to a boolean, string, numeric, or datetime value",
                    ));
                }
            })),
            Method::Boolean => out.push(J::Bool(match &v {
                J::Bool(b) => *b,
                J::Number(_) => num_of(&v).is_some_and(|n| !n.is_zero()),
                J::String(s) => match s.to_ascii_lowercase().as_str() {
                    "t" | "true" | "y" | "yes" | "on" | "1" => true,
                    "f" | "false" | "n" | "no" | "off" | "0" => false,
                    _ => {
                        return Err(jerr(
                            NON_NUMERIC_ITEM,
                            format!("argument \"{s}\" of jsonpath item method .boolean() is invalid for type boolean"),
                        ));
                    }
                },
                _ => {
                    return Err(jerr(
                        NON_NUMERIC_ITEM,
                        "jsonpath item method .boolean() can only be applied to a boolean, string, or numeric value",
                    ));
                }
            })),
            Method::KeyValue => match v {
                J::Object(m) => {
                    for (k, val) in m {
                        let mut o = Map::new();
                        o.insert("key".into(), J::String(k));
                        o.insert("value".into(), val);
                        o.insert("id".into(), J::Number(Number::from(0)));
                        out.push(J::Object(o));
                    }
                }
                _ => return Err(jerr(NOT_OBJECT, "jsonpath item method .keyvalue() can only be applied to an object")),
            },
        }
        Ok(())
    }

    fn pred(&self, p: &Pred, current: &J, last: Option<i64>) -> Tri {
        match p {
            Pred::And(l, r) => match (self.pred(l, current, last), self.pred(r, current, last)) {
                (Tri::False, _) | (_, Tri::False) => Tri::False,
                (Tri::True, Tri::True) => Tri::True,
                _ => Tri::Unknown,
            },
            Pred::Or(l, r) => match (self.pred(l, current, last), self.pred(r, current, last)) {
                (Tri::True, _) | (_, Tri::True) => Tri::True,
                (Tri::False, Tri::False) => Tri::False,
                _ => Tri::Unknown,
            },
            Pred::Not(inner) => match self.pred(inner, current, last) {
                Tri::True => Tri::False,
                Tri::False => Tri::True,
                Tri::Unknown => Tri::Unknown,
            },
            Pred::IsUnknown(inner) => {
                if self.pred(inner, current, last) == Tri::Unknown {
                    Tri::True
                } else {
                    Tri::False
                }
            }
            Pred::Exists(e) => match self.expr(e, current, last) {
                Ok(items) if items.is_empty() => Tri::False,
                Ok(_) => Tri::True,
                Err(_) => Tri::Unknown,
            },
            Pred::Cmp(op, l, r) => self.compare(*op, l, r, current, last),
            Pred::StartsWith(l, r) => {
                let (Ok(ls), Ok(rs)) = (self.expr(l, current, last), self.expr(r, current, last)) else { return Tri::Unknown };
                let ls = if self.lax() { self.unwrap_all(ls) } else { ls };
                let [J::String(prefix)] = rs.as_slice() else { return Tri::Unknown };
                self.any_of(&ls, |v| match v {
                    J::String(s) => Tri::from(s.starts_with(prefix.as_str())),
                    _ => Tri::Unknown,
                })
            }
            Pred::LikeRegex(l, pattern, flags) => {
                let Ok(ls) = self.expr(l, current, last) else { return Tri::Unknown };
                let ls = if self.lax() { self.unwrap_all(ls) } else { ls };
                let Ok(re) = like_regex(pattern, flags) else { return Tri::Unknown };
                self.any_of(&ls, |v| match v {
                    J::String(s) => Tri::from(re.is_match(s)),
                    _ => Tri::Unknown,
                })
            }
        }
    }

    /// Existential semantics: true if any item matches; unknown if any errs (strict) and none
    /// matches.
    fn any_of(&self, items: &[J], f: impl Fn(&J) -> Tri) -> Tri {
        let mut unknown = false;
        for v in items {
            match f(v) {
                Tri::True => {
                    if self.lax() {
                        return Tri::True;
                    }
                    if !unknown {
                        return Tri::True;
                    }
                }
                Tri::Unknown => {
                    unknown = true;
                    if self.strict {
                        return Tri::Unknown;
                    }
                }
                Tri::False => {}
            }
        }
        if unknown { Tri::Unknown } else { Tri::False }
    }

    fn compare(&self, op: CmpOp, l: &Expr, r: &Expr, current: &J, last: Option<i64>) -> Tri {
        let (Ok(ls), Ok(rs)) = (self.expr(l, current, last), self.expr(r, current, last)) else { return Tri::Unknown };
        let (ls, rs) = if self.lax() { (self.unwrap_all(ls), self.unwrap_all(rs)) } else { (ls, rs) };
        let mut unknown = false;
        let mut found = false;
        for a in &ls {
            for b in &rs {
                match compare_items(op, a, b) {
                    Tri::True => {
                        found = true;
                        if self.lax() {
                            return Tri::True;
                        }
                    }
                    Tri::Unknown => {
                        unknown = true;
                        if self.strict {
                            return Tri::Unknown;
                        }
                    }
                    Tri::False => {}
                }
            }
        }
        if found {
            Tri::True
        } else if unknown {
            Tri::Unknown
        } else {
            Tri::False
        }
    }
}

fn target_type(m: &Method) -> &'static str {
    match m {
        Method::Double => "double precision",
        Method::Integer => "integer",
        Method::BigInt => "bigint",
        _ => "numeric",
    }
}

impl From<bool> for Tri {
    fn from(b: bool) -> Tri {
        if b { Tri::True } else { Tri::False }
    }
}

/// `compareItems`: same-type comparisons; null equals only null; mixed types are unknown,
/// except comparisons with null, which are false.
fn compare_items(op: CmpOp, a: &J, b: &J) -> Tri {
    let ord = match (a, b) {
        (J::Null, J::Null) => Some(Ordering::Equal),
        (J::Null, _) | (_, J::Null) => {
            return Tri::from(op == CmpOp::Ne);
        }
        (J::Bool(x), J::Bool(y)) => Some(x.cmp(y)),
        (J::Number(_), J::Number(_)) => match (num_of(a), num_of(b)) {
            (Some(x), Some(y)) => Some(x.cmp_num(&y)),
            _ => None,
        },
        (J::String(x), J::String(y)) => Some(x.as_bytes().cmp(y.as_bytes())),
        (J::Array(_) | J::Object(_), _) | (_, J::Array(_) | J::Object(_)) => None,
        _ => None,
    };
    let Some(o) = ord else { return Tri::Unknown };
    Tri::from(match op {
        CmpOp::Eq => o == Ordering::Equal,
        CmpOp::Ne => o != Ordering::Equal,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Le => o != Ordering::Greater,
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::Ge => o != Ordering::Less,
    })
}

fn like_regex(pattern: &str, flags: &str) -> SqlResult<regex::Regex> {
    let mut b = regex::RegexBuilder::new(&if flags.contains('q') { regex::escape(pattern) } else { pattern.to_owned() });
    b.case_insensitive(flags.contains('i'))
        .dot_matches_new_line(flags.contains('s'))
        .multi_line(flags.contains('m'))
        .ignore_whitespace(flags.contains('x'));
    b.build().map_err(|e| SqlError::new(crate::code::INVALID_REGULAR_EXPRESSION, format!("invalid regular expression: {e}")))
}

/// Errors that `silent` mode (and the `@?` / `@@` operators) turn into empty results.
pub fn is_suppressible(e: &SqlError) -> bool {
    matches!(e.code, NO_KEY | NOT_ARRAY | NOT_OBJECT | ARRAY_INDEX | NOT_NUMERIC | NON_NUMERIC_ITEM | SINGLETON | "2203F" | "22035")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(doc: &str, path: &str) -> SqlResult<Vec<String>> {
        let doc: J = serde_json::from_str(doc).expect("json");
        let p = parse(path)?;
        let vars = Map::new();
        let ev = Evaluator { strict: p.strict, root: &doc, vars: &vars };
        Ok(ev.eval(&p)?.iter().map(J::to_string).collect())
    }

    #[test]
    fn canonical_text() {
        assert_eq!(normalize("$.a ? (@ > 0)").expect("ok"), r#"$."a"?(@ > 0)"#);
        assert_eq!(normalize("strict $.**{1 to last}.c").expect("ok"), r#"strict $.**{1 to last}."c""#);
        assert_eq!(normalize("$.a[1, 2 to last]").expect("ok"), r#"$."a"[1,2 to last]"#);
        assert_eq!(
            normalize("$ ? (@.s like_regex \"^h\" flag \"i\" && exists(@.x))").expect("ok"),
            r#"$?(@."s" like_regex "^h" flag "i" && exists (@."x"))"#
        );
        assert_eq!(normalize("$.a + 2 * (3 - $.b)").expect("ok"), r#"($."a" + 2 * (3 - $."b"))"#);
        assert_eq!(normalize("$.a ? (!(@ == 1) && (@ > 2) is unknown)").expect("ok"), r#"$."a"?(!(@ == 1) && (@ > 2) is unknown)"#);
        assert_eq!(normalize("@.a").expect_err("root").message, "@ is not allowed in root expressions");
    }

    #[test]
    fn lax_and_strict() {
        assert_eq!(run(r#"{"a": [1, 2, 3, 4]}"#, "$.a[*] ? (@ > 2)").expect("ok"), vec!["3", "4"]);
        assert_eq!(run(r#"{"a": [1, 2]}"#, "$.a[last]").expect("ok"), vec!["2"]);
        assert_eq!(run(r#"{"a": 1}"#, "lax $.b").expect("ok"), Vec::<String>::new());
        assert_eq!(run(r#"{"a": 1}"#, "strict $.b").expect_err("strict").code, "2203A");
        assert_eq!(run(r#"{"a": {"b": {"c": 1}}}"#, "strict $.**.c").expect("ok"), vec!["1"]);
        assert_eq!(run(r#"{"a": -2.5}"#, "$.a.abs().floor()").expect("ok"), vec!["2"]);
        assert_eq!(run("[1, 2, 3]", "$.size()").expect("ok"), vec!["3"]);
        assert_eq!(run(r#"{"s": "Hello"}"#, r#"$.s ? (@ like_regex "^h" flag "i")"#).expect("ok"), vec![r#""Hello""#]);
        assert_eq!(run(r#"{"a": 1}"#, "$.a == 1").expect("ok"), vec!["true"]);
        assert_eq!(run(r#"{"a": [1, 2]}"#, "strict $.a + 1").expect_err("not single").code, "22038");
    }
}
