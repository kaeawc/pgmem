//! String functions and operators (LIKE, regular expressions, concatenation helpers).

use super::{Resolved, lenient, no_function, no_operator, resolved, strict};
use crate::eval::EvalCtx;
use pgmem_types::{SqlError, SqlResult, Type, Value, code, text};
use std::fmt::Write as _;

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn texts(n: usize) -> Vec<Type> {
    vec![Type::Text; n]
}

fn is_texty(t: &Type) -> bool {
    matches!(t, Type::Text | Type::Unknown)
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    let text_fn = |f: crate::plan::Func, arity: usize, ret: Type| -> SqlResult<Option<Resolved>> {
        if n != arity || !args.iter().all(|t| super::implicit_ok(t, &Type::Text) || *t == Type::Unknown) {
            return Err(err());
        }
        resolved(f, texts(arity), ret)
    };
    match name {
        "length" | "char_length" | "character_length" => match args {
            [Type::Bytea] => resolved(strict("octet_length", octet_length), vec![Type::Bytea], Type::Int4),
            [_] => text_fn(strict("length", length), 1, Type::Int4),
            _ => Err(err()),
        },
        "octet_length" => match args {
            [Type::Bytea] => resolved(strict("octet_length", octet_length), vec![Type::Bytea], Type::Int4),
            _ => text_fn(strict("octet_length", octet_length), 1, Type::Int4),
        },
        "bit_length" => {
            text_fn(strict("bit_length", |a, _| Ok(Value::Int4(i32::try_from(s(&a[0]).len() * 8).unwrap_or(i32::MAX)))), 1, Type::Int4)
        }
        "lower" => text_fn(strict("lower", |a, _| Ok(Value::Text(s(&a[0]).to_lowercase()))), 1, Type::Text),
        "upper" => text_fn(strict("upper", |a, _| Ok(Value::Text(s(&a[0]).to_uppercase()))), 1, Type::Text),
        "initcap" => text_fn(strict("initcap", initcap), 1, Type::Text),
        "substring" | "substr" => match args {
            [_, t] if t.is_integer() || (name == "substr" && *t == Type::Unknown) => {
                resolved(strict("substring", substring), vec![Type::Text, Type::Int4], Type::Text)
            }
            [_, t, u] if (t.is_integer() || *t == Type::Unknown) && (u.is_integer() || *u == Type::Unknown) => {
                resolved(strict("substring", substring), vec![Type::Text, Type::Int4, Type::Int4], Type::Text)
            }
            [_, t] if is_texty(t) => resolved(strict("substring", regex_substring), texts(2), Type::Text),
            [_, t, u] if is_texty(t) && is_texty(u) => resolved(strict("substring", similar_substring), texts(3), Type::Text),
            _ => Err(err()),
        },
        "position" | "strpos" => text_fn(strict("strpos", strpos), 2, Type::Int4),
        "btrim" | "trim" => match n {
            1 => text_fn(strict("btrim", |a, _| Ok(Value::Text(trim(s(&a[0]), " ", true, true)))), 1, Type::Text),
            _ => text_fn(strict("btrim", |a, _| Ok(Value::Text(trim(s(&a[0]), s(&a[1]), true, true)))), 2, Type::Text),
        },
        "ltrim" => match n {
            1 => text_fn(strict("ltrim", |a, _| Ok(Value::Text(trim(s(&a[0]), " ", true, false)))), 1, Type::Text),
            _ => text_fn(strict("ltrim", |a, _| Ok(Value::Text(trim(s(&a[0]), s(&a[1]), true, false)))), 2, Type::Text),
        },
        "rtrim" => match n {
            1 => text_fn(strict("rtrim", |a, _| Ok(Value::Text(trim(s(&a[0]), " ", false, true)))), 1, Type::Text),
            _ => text_fn(strict("rtrim", |a, _| Ok(Value::Text(trim(s(&a[0]), s(&a[1]), false, true)))), 2, Type::Text),
        },
        // concat / concat_ws take "any" arguments, rendered with their output functions.
        "concat" => resolved(
            lenient("concat", concat),
            args.iter().map(|t| if *t == Type::Unknown { Type::Text } else { t.clone() }).collect(),
            Type::Text,
        ),
        "concat_ws" if n >= 1 => {
            let mut targets: Vec<Type> = args.iter().map(|t| if *t == Type::Unknown { Type::Text } else { t.clone() }).collect();
            targets[0] = Type::Text;
            resolved(lenient("concat_ws", concat_ws), targets, Type::Text)
        }
        "replace" => text_fn(
            strict("replace", |a, _| {
                Ok(Value::Text(if s(&a[1]).is_empty() { s(&a[0]).to_owned() } else { s(&a[0]).replace(s(&a[1]), s(&a[2])) }))
            }),
            3,
            Type::Text,
        ),
        "repeat" if n == 2 => resolved(strict("repeat", repeat), vec![Type::Text, Type::Int4], Type::Text),
        "reverse" => text_fn(strict("reverse", |a, _| Ok(Value::Text(s(&a[0]).chars().rev().collect()))), 1, Type::Text),
        "lpad" | "rpad" => {
            let left = name == "lpad";
            let f = if left { strict("lpad", lpad) } else { strict("rpad", rpad) };
            match n {
                2 => resolved(f, vec![Type::Text, Type::Int4], Type::Text),
                3 => resolved(f, vec![Type::Text, Type::Int4, Type::Text], Type::Text),
                _ => Err(err()),
            }
        }
        "left" if n == 2 => resolved(strict("left", left_fn), vec![Type::Text, Type::Int4], Type::Text),
        "right" if n == 2 => resolved(strict("right", right_fn), vec![Type::Text, Type::Int4], Type::Text),
        "split_part" if n == 3 => resolved(strict("split_part", split_part), vec![Type::Text, Type::Text, Type::Int4], Type::Text),
        "ascii" => text_fn(
            strict("ascii", |a, _| Ok(Value::Int4(s(&a[0]).chars().next().map_or(0, |c| i32::try_from(u32::from(c)).unwrap_or(0))))),
            1,
            Type::Int4,
        ),
        "chr" if n == 1 => resolved(strict("chr", chr), vec![Type::Int4], Type::Text),
        "to_hex" => match args {
            [Type::Int8] => resolved(strict("to_hex", to_hex), vec![Type::Int8], Type::Text),
            [t] if t.is_integer() || *t == Type::Unknown => resolved(strict("to_hex", to_hex), vec![Type::Int4], Type::Text),
            _ => Err(err()),
        },
        "md5" => match args {
            [Type::Bytea] => resolved(strict("md5", md5_fn), vec![Type::Bytea], Type::Text),
            _ => text_fn(strict("md5", md5_fn), 1, Type::Text),
        },
        "sha224" | "sha256" | "sha384" | "sha512" => match args {
            [t] if *t == Type::Bytea || *t == Type::Unknown => {
                let f = match name {
                    "sha224" => strict("sha224", |a, _| Ok(Value::Bytea(digest::<sha2::Sha224>(&a[0])))),
                    "sha256" => strict("sha256", |a, _| Ok(Value::Bytea(digest::<sha2::Sha256>(&a[0])))),
                    "sha384" => strict("sha384", |a, _| Ok(Value::Bytea(digest::<sha2::Sha384>(&a[0])))),
                    _ => strict("sha512", |a, _| Ok(Value::Bytea(digest::<sha2::Sha512>(&a[0])))),
                };
                resolved(f, vec![Type::Bytea], Type::Bytea)
            }
            _ => Err(err()),
        },
        _ => resolve_more(name, args),
    }
}

fn resolve_more(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    let n = args.len();
    let text_fn = |f: crate::plan::Func, arity: usize, ret: Type| -> SqlResult<Option<Resolved>> {
        if n != arity || !args.iter().all(|t| super::implicit_ok(t, &Type::Text) || *t == Type::Unknown) {
            return Err(err());
        }
        resolved(f, texts(arity), ret)
    };
    match name {
        "format" if n >= 1 => {
            let mut targets: Vec<Type> = args.iter().map(|t| if *t == Type::Unknown { Type::Text } else { t.clone() }).collect();
            targets[0] = Type::Text;
            resolved(lenient("format", format_fn), targets, Type::Text)
        }
        "quote_ident" => text_fn(strict("quote_ident", |a, _| Ok(Value::Text(quote_ident(s(&a[0]))))), 1, Type::Text),
        "quote_literal" => text_fn(strict("quote_literal", |a, _| Ok(Value::Text(quote_literal(s(&a[0]))))), 1, Type::Text),
        "quote_nullable" => resolved(
            lenient("quote_nullable", |a, _| Ok(Value::Text(if a[0].is_null() { "NULL".into() } else { quote_literal(s(&a[0])) }))),
            texts(1),
            Type::Text,
        ),
        "starts_with" => text_fn(strict("starts_with", |a, _| Ok(Value::Bool(s(&a[0]).starts_with(s(&a[1]))))), 2, Type::Bool),
        "translate" => text_fn(strict("translate", translate), 3, Type::Text),
        "overlay" => match n {
            3 => resolved(strict("overlay", overlay), vec![Type::Text, Type::Text, Type::Int4], Type::Text),
            4 => resolved(strict("overlay", overlay), vec![Type::Text, Type::Text, Type::Int4, Type::Int4], Type::Text),
            _ => Err(err()),
        },
        "encode" if n == 2 => resolved(strict("encode", encode), vec![Type::Bytea, Type::Text], Type::Text),
        "decode" if n == 2 => resolved(strict("decode", decode), texts(2), Type::Bytea),
        "regexp_replace" => match n {
            3 => text_fn(strict("regexp_replace", regexp_replace), 3, Type::Text),
            4 => text_fn(strict("regexp_replace", regexp_replace), 4, Type::Text),
            _ => Err(err()),
        },
        "regexp_match" => match n {
            2 | 3 => text_fn(strict("regexp_match", regexp_match), n, Type::array_of(Type::Text)),
            _ => Err(err()),
        },
        "regexp_like" => match n {
            2 | 3 => text_fn(
                strict("regexp_like", |a, _| Ok(Value::Bool(compile(s(&a[1]), a.get(2).map_or("", s))?.is_match(s(&a[0]))))),
                n,
                Type::Bool,
            ),
            _ => Err(err()),
        },
        "regexp_count" if n == 2 => text_fn(
            strict("regexp_count", |a, _| {
                Ok(Value::Int4(i32::try_from(compile(s(&a[1]), "")?.find_iter(s(&a[0])).count()).unwrap_or(i32::MAX)))
            }),
            2,
            Type::Int4,
        ),
        "regexp_split_to_array" => match n {
            2 | 3 => text_fn(strict("regexp_split_to_array", regexp_split), n, Type::array_of(Type::Text)),
            _ => Err(err()),
        },
        _ => Ok(None),
    }
}

/// LIKE / ILIKE / regex / SIMILAR TO operators (`~~` forms are bound as LIKE by the binder).
pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    let both_text = (is_texty(l) || super::implicit_ok(l, &Type::Text)) && is_texty(r);
    let f = match op {
        "~" => strict("~", |a, _| Ok(Value::Bool(compile(s(&a[1]), "")?.is_match(s(&a[0]))))),
        "~*" => strict("~*", |a, _| Ok(Value::Bool(compile(s(&a[1]), "i")?.is_match(s(&a[0]))))),
        "!~" => strict("!~", |a, _| Ok(Value::Bool(!compile(s(&a[1]), "")?.is_match(s(&a[0]))))),
        "!~*" => strict("!~*", |a, _| Ok(Value::Bool(!compile(s(&a[1]), "i")?.is_match(s(&a[0]))))),
        "^@" => strict("^@", |a, _| Ok(Value::Bool(s(&a[0]).starts_with(s(&a[1]))))),
        _ => return Ok(None),
    };
    if !both_text {
        return Err(no_operator(op, Some(l), r));
    }
    resolved(f, texts(2), Type::Bool)
}

/// `LIKE` implementation: `(text, pattern[, escape])`; `ILIKE` folds case first.
pub fn like_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let esc = escape_char(a.get(2))?;
    Ok(Value::Bool(like_match(s(&a[0]), s(&a[1]), esc)?))
}

pub fn ilike_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let esc = escape_char(a.get(2))?;
    Ok(Value::Bool(like_match(&s(&a[0]).to_lowercase(), &s(&a[1]).to_lowercase(), esc)?))
}

pub fn similar_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let esc = escape_char(a.get(2))?;
    let re = similar_to_regex(s(&a[1]), esc)?;
    Ok(Value::Bool(compile(&format!("^(?:{re})$"), "")?.is_match(s(&a[0]))))
}

fn escape_char(v: Option<&Value>) -> SqlResult<Option<char>> {
    match v {
        None => Ok(Some('\\')),
        Some(e) => {
            let e = s(e);
            let mut chars = e.chars();
            match (chars.next(), chars.next()) {
                (None, _) => Ok(None),
                (Some(c), None) => Ok(Some(c)),
                _ => Err(SqlError::new("22025", "invalid escape string").with_hint("Escape string must be empty or one character.")),
            }
        }
    }
}

/// SQL LIKE matching on characters with an optional escape character.
pub fn like_match(text: &str, pattern: &str, esc: Option<char>) -> SqlResult<bool> {
    // Compile to tokens first.
    enum Tok {
        Any,
        One,
        Lit(char),
    }
    let mut toks = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == esc {
            match chars.next() {
                Some(n) => toks.push(Tok::Lit(n)),
                None => return Err(SqlError::new("22025", "LIKE pattern must not end with escape character")),
            }
        } else if c == '%' {
            toks.push(Tok::Any);
        } else if c == '_' {
            toks.push(Tok::One);
        } else {
            toks.push(Tok::Lit(c));
        }
    }
    let t: Vec<char> = text.chars().collect();
    // Iterative matcher with backtracking on the last `%`.
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match toks.get(pi) {
            Some(Tok::Any) => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(Tok::One) => {
                ti += 1;
                pi += 1;
            }
            Some(Tok::Lit(c)) if *c == t[ti] => {
                ti += 1;
                pi += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return Ok(false),
            },
        }
    }
    while matches!(toks.get(pi), Some(Tok::Any)) {
        pi += 1;
    }
    Ok(pi == toks.len())
}

/// Translates a SQL SIMILAR TO pattern to a regex body.
fn similar_to_regex(p: &str, esc: Option<char>) -> SqlResult<String> {
    let mut out = String::new();
    let mut chars = p.chars();
    let mut in_bracket = false;
    while let Some(c) = chars.next() {
        if Some(c) == esc {
            if let Some(n) = chars.next() {
                out.push_str(&regex::escape(&n.to_string()));
            }
            continue;
        }
        match c {
            '%' if !in_bracket => out.push_str(".*"),
            '_' if !in_bracket => out.push('.'),
            '[' => {
                in_bracket = true;
                out.push('[');
            }
            ']' => {
                in_bracket = false;
                out.push(']');
            }
            '.' | '^' | '$' | '\\' if !in_bracket => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Compiles a Postgres (ARE) regular expression. Flags: `i` case-insensitive, `g` global
/// (handled by callers), `n`/`m` newline-sensitive, `s` single-line, `x` extended.
pub fn compile(pattern: &str, flags: &str) -> SqlResult<regex::Regex> {
    let mut b = regex::RegexBuilder::new(&translate_are(pattern));
    for f in flags.chars() {
        match f {
            'i' => {
                b.case_insensitive(true);
            }
            'c' => {
                b.case_insensitive(false);
            }
            'n' | 'm' => {
                b.multi_line(true);
            }
            's' => {
                b.dot_matches_new_line(true);
            }
            'x' => {
                b.ignore_whitespace(true);
            }
            'g' => {}
            other => {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid regular expression option: \"{other}\"")));
            }
        }
    }
    // Postgres' `.` matches newlines unless newline-sensitive mode is on.
    if !flags.contains('n') && !flags.contains('m') {
        b.dot_matches_new_line(true);
    }
    b.build().map_err(|e| SqlError::new(code::INVALID_REGULAR_EXPRESSION, format!("invalid regular expression: {e}")))
}

/// Rewrites the few ARE constructs Rust's regex syntax spells differently.
fn translate_are(p: &str) -> String {
    p.replace("[[:<:]]", r"\b").replace("[[:>:]]", r"\b").replace(r"\m", r"\b").replace(r"\M", r"\b").replace(r"\y", r"\b")
}

fn length(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Int4(i32::try_from(s(&a[0]).chars().count()).unwrap_or(i32::MAX)))
}

fn octet_length(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let n = match &a[0] {
        Value::Bytea(b) => b.len(),
        v => s(v).len(),
    };
    Ok(Value::Int4(i32::try_from(n).unwrap_or(i32::MAX)))
}

fn initcap(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut out = String::new();
    let mut prev_alnum = false;
    for c in s(&a[0]).chars() {
        if c.is_alphanumeric() {
            if prev_alnum {
                out.extend(c.to_lowercase());
            } else {
                out.extend(c.to_uppercase());
            }
            prev_alnum = true;
        } else {
            out.push(c);
            prev_alnum = false;
        }
    }
    Ok(Value::Text(out))
}

/// `substring(text, start[, count])` with Postgres' handling of out-of-range starts.
fn substring(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let chars: Vec<char> = s(&a[0]).chars().collect();
    let start = int(&a[1]);
    let end = match a.get(2) {
        Some(c) => {
            let count = int(c);
            if count < 0 {
                return Err(SqlError::new("22011", "negative substring length not allowed"));
            }
            start.saturating_add(count)
        }
        None => i64::MAX,
    };
    let lo = start.max(1);
    let len = i64::try_from(chars.len()).unwrap_or(i64::MAX);
    let hi = end.min(len + 1);
    if hi <= lo {
        return Ok(Value::Text(String::new()));
    }
    let (lo, hi) = (usize::try_from(lo - 1).unwrap_or(0), usize::try_from(hi - 1).unwrap_or(0));
    Ok(Value::Text(chars[lo..hi].iter().collect()))
}

/// `substring(text FROM pattern)`: the first parenthesized group, or the whole match.
fn regex_substring(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let re = compile(s(&a[1]), "")?;
    Ok(match re.captures(s(&a[0])) {
        None => Value::Null,
        Some(c) => match c.get(1).or_else(|| c.get(0)) {
            Some(m) => Value::Text(m.as_str().to_owned()),
            None => Value::Null,
        },
    })
}

/// `substring(text SIMILAR pattern ESCAPE esc)`.
fn similar_substring(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let esc = escape_char(Some(&a[2]))?;
    let re = similar_to_regex(s(&a[1]), esc)?;
    let re = compile(&format!("^(?:{re})$"), "")?;
    Ok(match re.captures(s(&a[0])) {
        None => Value::Null,
        Some(c) => Value::Text(c.get(1).or_else(|| c.get(0)).map_or("", |m| m.as_str()).to_owned()),
    })
}

fn strpos(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (hay, needle) = (s(&a[0]), s(&a[1]));
    Ok(Value::Int4(match hay.find(needle) {
        Some(byte) => i32::try_from(hay[..byte].chars().count() + 1).unwrap_or(i32::MAX),
        None => 0,
    }))
}

fn trim(src: &str, chars: &str, left: bool, right: bool) -> String {
    let set: Vec<char> = chars.chars().collect();
    let mut out = src;
    if left {
        out = out.trim_start_matches(|c| set.contains(&c));
    }
    if right {
        out = out.trim_end_matches(|c| set.contains(&c));
    }
    out.to_owned()
}

/// Text form of an "any" argument (for concat / format).
pub fn any_text(v: &Value) -> SqlResult<String> {
    Ok(match v {
        Value::Text(t) => t.clone(),
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_owned(),
        other => text::encode(other, &other.natural_type())?,
    })
}

fn concat(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut out = String::new();
    for v in a.iter().filter(|v| !v.is_null()) {
        out.push_str(&any_text(v)?);
    }
    Ok(Value::Text(out))
}

fn concat_ws(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let sep = s(&a[0]);
    let parts = a[1..].iter().filter(|v| !v.is_null()).map(any_text).collect::<SqlResult<Vec<_>>>()?;
    Ok(Value::Text(parts.join(sep)))
}

fn repeat(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let n = usize::try_from(int(&a[1]).max(0)).unwrap_or(0);
    Ok(Value::Text(s(&a[0]).repeat(n)))
}

fn pad(a: &[Value], left: bool) -> SqlResult<Value> {
    let src: Vec<char> = s(&a[0]).chars().collect();
    let len = usize::try_from(int(&a[1]).max(0)).unwrap_or(0);
    let fill: Vec<char> = a.get(2).map_or(" ", s).chars().collect();
    if src.len() >= len {
        return Ok(Value::Text(src[..len].iter().collect()));
    }
    if fill.is_empty() {
        return Ok(Value::Text(src.iter().collect()));
    }
    let padding: String = fill.iter().cycle().take(len - src.len()).collect();
    let body: String = src.iter().collect();
    Ok(Value::Text(if left { format!("{padding}{body}") } else { format!("{body}{padding}") }))
}

fn lpad(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    pad(a, true)
}

fn rpad(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    pad(a, false)
}

fn left_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let chars: Vec<char> = s(&a[0]).chars().collect();
    let n = int(&a[1]);
    let len = i64::try_from(chars.len()).unwrap_or(0);
    let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
    Ok(Value::Text(chars[..usize::try_from(take).unwrap_or(0)].iter().collect()))
}

fn right_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let chars: Vec<char> = s(&a[0]).chars().collect();
    let n = int(&a[1]);
    let len = i64::try_from(chars.len()).unwrap_or(0);
    let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
    let start = usize::try_from(len - take).unwrap_or(0);
    Ok(Value::Text(chars[start..].iter().collect()))
}

fn split_part(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let (src, delim, n) = (s(&a[0]), s(&a[1]), int(&a[2]));
    if n == 0 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "field position must not be zero"));
    }
    let parts: Vec<&str> = if delim.is_empty() { vec![src] } else { src.split(delim).collect() };
    let idx = if n > 0 { usize::try_from(n - 1).ok() } else { parts.len().checked_sub(usize::try_from(-n).unwrap_or(usize::MAX)) };
    Ok(Value::Text(idx.and_then(|i| parts.get(i)).map_or("", |p| *p).to_owned()))
}

fn chr(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let n = int(&a[0]);
    if n == 0 {
        return Err(SqlError::new("54000", "null character not permitted"));
    }
    let c = u32::try_from(n)
        .ok()
        .and_then(char::from_u32)
        .ok_or_else(|| SqlError::new("54000", "requested character too large for encoding"))?;
    Ok(Value::Text(c.to_string()))
}

fn to_hex(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Text(match &a[0] {
        Value::Int8(n) => format!("{:x}", *n as u64),
        #[allow(clippy::cast_sign_loss)]
        v => format!("{:x}", int(v) as u32),
    }))
}

fn md5_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    use md5::Digest;
    let bytes: &[u8] = match &a[0] {
        Value::Bytea(b) => b,
        v => s(v).as_bytes(),
    };
    let mut hex = String::with_capacity(32);
    for b in md5::Md5::digest(bytes) {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(Value::Text(hex))
}

fn digest<D: sha2::Digest>(v: &Value) -> Vec<u8> {
    let bytes: &[u8] = match v {
        Value::Bytea(b) => b,
        other => s(other).as_bytes(),
    };
    D::digest(bytes).to_vec()
}

pub fn quote_ident(id: &str) -> String {
    let simple = id.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !crate::bind::is_reserved_keyword(id);
    if simple { id.to_owned() } else { format!("\"{}\"", id.replace('"', "\"\"")) }
}

pub fn quote_literal(v: &str) -> String {
    if v.contains('\\') { format!("E'{}'", v.replace('\\', "\\\\").replace('\'', "''")) } else { format!("'{}'", v.replace('\'', "''")) }
}

/// `format(fmt, args...)`: `%s`, `%I`, `%L`, `%%`, with optional `n$` positions and widths.
fn format_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    if a[0].is_null() {
        return Ok(Value::Null);
    }
    let fmt: Vec<char> = s(&a[0]).chars().collect();
    let args = &a[1..];
    let mut out = String::new();
    let mut next = 0usize;
    let mut i = 0;
    let bad =
        || SqlError::new(code::INVALID_PARAMETER_VALUE, "unterminated format() type specifier").with_hint("For a single \"%\" use \"%%\".");
    while i < fmt.len() {
        let c = fmt[i];
        i += 1;
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(&spec) = fmt.get(i) else { return Err(bad()) };
        if spec == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        // Optional argument position `n$`.
        let mut j = i;
        while j < fmt.len() && fmt[j].is_ascii_digit() {
            j += 1;
        }
        let mut pos = None;
        if j < fmt.len() && fmt[j] == '$' && j > i {
            pos = fmt[i..j].iter().collect::<String>().parse::<usize>().ok();
            i = j + 1;
        }
        // Optional flags/width.
        let left_align = fmt.get(i) == Some(&'-');
        if left_align {
            i += 1;
        }
        let mut width = String::new();
        while i < fmt.len() && fmt[i].is_ascii_digit() {
            width.push(fmt[i]);
            i += 1;
        }
        let Some(&kind) = fmt.get(i) else { return Err(bad()) };
        i += 1;
        let idx = match pos {
            Some(p) if p >= 1 => p - 1,
            Some(_) => {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "format specifies argument 0, but arguments are numbered from 1"));
            }
            None => next,
        };
        next = idx + 1;
        let arg = args.get(idx).ok_or_else(|| SqlError::new(code::INVALID_PARAMETER_VALUE, "too few arguments for format()"))?;
        let piece = match kind {
            's' => {
                if arg.is_null() {
                    String::new()
                } else {
                    any_text(arg)?
                }
            }
            'I' => {
                if arg.is_null() {
                    return Err(SqlError::new("22004", "null values cannot be formatted as an SQL identifier"));
                }
                quote_ident(&any_text(arg)?)
            }
            'L' => {
                if arg.is_null() {
                    "NULL".to_owned()
                } else {
                    quote_literal(&any_text(arg)?)
                }
            }
            other => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized format() type specifier \"{other}\""))),
        };
        let w: usize = width.parse().unwrap_or(0);
        let len = piece.chars().count();
        if len < w {
            let padding = " ".repeat(w - len);
            if left_align {
                out.push_str(&piece);
                out.push_str(&padding);
            } else {
                out.push_str(&padding);
                out.push_str(&piece);
            }
        } else {
            out.push_str(&piece);
        }
    }
    Ok(Value::Text(out))
}

fn translate(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let from: Vec<char> = s(&a[1]).chars().collect();
    let to: Vec<char> = s(&a[2]).chars().collect();
    let out: String = s(&a[0])
        .chars()
        .filter_map(|c| match from.iter().position(|f| *f == c) {
            Some(i) => to.get(i).copied(),
            None => Some(c),
        })
        .collect();
    Ok(Value::Text(out))
}

fn overlay(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let src: Vec<char> = s(&a[0]).chars().collect();
    let repl = s(&a[1]);
    let start = usize::try_from(int(&a[2]).max(1) - 1).unwrap_or(0).min(src.len());
    let count = match a.get(3) {
        Some(c) => usize::try_from(int(c).max(0)).unwrap_or(0),
        None => repl.chars().count(),
    };
    let end = (start + count).min(src.len());
    let mut out: String = src[..start].iter().collect();
    out.push_str(repl);
    out.extend(src[end..].iter());
    Ok(Value::Text(out))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let Value::Bytea(b) = &a[0] else { return Err(SqlError::internal("encode expects bytea")) };
    Ok(Value::Text(match s(&a[1]).to_ascii_lowercase().as_str() {
        "hex" => text::format_bytea(b)[2..].to_owned(),
        "base64" => {
            let mut out = String::new();
            for (i, chunk) in b.chunks(3).enumerate() {
                if i > 0 && i % 19 == 0 {
                    out.push('\n');
                }
                let n =
                    (u32::from(chunk[0]) << 16) | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8) | u32::from(*chunk.get(2).unwrap_or(&0));
                for k in 0..4 {
                    if k <= chunk.len() {
                        out.push(char::from(B64[((n >> (18 - 6 * k)) & 63) as usize]));
                    } else {
                        out.push('=');
                    }
                }
            }
            out
        }
        "escape" => {
            let mut out = String::new();
            for byte in b {
                match byte {
                    b'\\' => out.push_str("\\\\"),
                    0x20..=0x7e => out.push(char::from(*byte)),
                    other => {
                        let _ = write!(out, "\\{other:03o}");
                    }
                }
            }
            out
        }
        other => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized encoding: \"{other}\""))),
    }))
}

fn decode(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let src = s(&a[0]);
    Ok(Value::Bytea(match s(&a[1]).to_ascii_lowercase().as_str() {
        "hex" => text::parse_bytea(&format!("\\x{src}"))?,
        "base64" => {
            let mut out = Vec::new();
            let mut buf = 0u32;
            let mut bits = 0;
            for c in src.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
                let v = B64
                    .iter()
                    .position(|b| *b == c)
                    .ok_or_else(|| SqlError::new(code::INVALID_PARAMETER_VALUE, "invalid symbol in base64 input"))?;
                buf = (buf << 6) | u32::try_from(v).unwrap_or(0);
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    out.push(u8::try_from((buf >> bits) & 0xff).unwrap_or(0));
                }
            }
            out
        }
        "escape" => text::parse_bytea(src)?,
        other => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized encoding: \"{other}\""))),
    }))
}

/// Converts Postgres replacement syntax (`\1`, `\&`) to the regex crate's (`${1}`, `${0}`).
fn replacement(r: &str) -> String {
    let mut out = String::new();
    let mut chars = r.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(d @ '0'..='9') => {
                    let _ = write!(out, "${{{d}}}");
                }
                Some('&') => out.push_str("${0}"),
                Some(other) => out.push(other),
                None => out.push('\\'),
            },
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out
}

fn regexp_replace(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let flags = a.get(3).map_or("", s);
    let re = compile(s(&a[1]), flags)?;
    let rep = replacement(s(&a[2]));
    let out = if flags.contains('g') { re.replace_all(s(&a[0]), rep.as_str()) } else { re.replace(s(&a[0]), rep.as_str()) };
    Ok(Value::Text(out.into_owned()))
}

fn regexp_match(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let flags = a.get(2).map_or("", s);
    if flags.contains('g') {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "regexp_match() does not support the \"global\" option")
            .with_hint("Use the regexp_matches function instead."));
    }
    let re = compile(s(&a[1]), flags)?;
    Ok(match re.captures(s(&a[0])) {
        None => Value::Null,
        Some(c) if c.len() == 1 => Value::Array(vec![Value::Text(c.get(0).map_or("", |m| m.as_str()).to_owned())]),
        Some(c) => Value::Array((1..c.len()).map(|i| c.get(i).map_or(Value::Null, |m| Value::Text(m.as_str().to_owned()))).collect()),
    })
}

fn regexp_split(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let re = compile(s(&a[1]), a.get(2).map_or("", s))?;
    let src = s(&a[0]);
    if s(&a[1]).is_empty() {
        return Ok(Value::Array(src.chars().map(|c| Value::Text(c.to_string())).collect()));
    }
    Ok(Value::Array(re.split(src).map(|p| Value::Text(p.to_owned())).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_semantics() {
        assert!(like_match("hello", "h%o", Some('\\')).expect("like"));
        assert!(like_match("hello", "h_llo", Some('\\')).expect("like"));
        assert!(!like_match("hello", "h_lo", Some('\\')).expect("like"));
        assert!(like_match("50%", "50\\%", Some('\\')).expect("like"));
        assert!(!like_match("500", "50\\%", Some('\\')).expect("like"));
        assert!(like_match("", "%", Some('\\')).expect("like"));
        assert!(like_match("abcabc", "%bc", Some('\\')).expect("like"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("my table"), "\"my table\"");
        assert_eq!(quote_ident("users"), "users");
        assert_eq!(quote_ident("select"), "\"select\"");
        assert_eq!(quote_literal("a'b"), "'a''b'");
    }

    #[test]
    fn replacement_syntax() {
        assert_eq!(replacement(r"[\1]"), "[${1}]");
        assert_eq!(replacement(r"\&!"), "${0}!");
    }
}
