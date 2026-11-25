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
            [Type::Bytea, t] if t.is_integer() || *t == Type::Unknown => {
                resolved(strict("substring", substring), vec![Type::Bytea, Type::Int4], Type::Bytea)
            }
            [Type::Bytea, t, u] if (t.is_integer() || *t == Type::Unknown) && (u.is_integer() || *u == Type::Unknown) => {
                resolved(strict("substring", substring), vec![Type::Bytea, Type::Int4, Type::Int4], Type::Bytea)
            }
            [_, t] if t.is_integer() || (name == "substr" && *t == Type::Unknown) => {
                resolved(strict("substring", substring), vec![Type::Text, Type::Int4], Type::Text)
            }
            // Unknown literals prefer the text (SIMILAR) form, as in Postgres.
            [_, Type::Unknown, Type::Unknown] if name != "substr" => resolved(strict("substring", similar_substring), texts(3), Type::Text),
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

const T: Type = Type::Text;
const I: Type = Type::Int4;

/// A regexp function taking a prefix (at least `min` arguments) of `sig`.
fn regex_sig(name: &str, args: &[Type], sig: &[Type], min: usize, f: crate::plan::Func, ret: Type) -> SqlResult<Option<Resolved>> {
    if args.len() < min || args.len() > sig.len() {
        return Err(no_function(name, args));
    }
    for (a, want) in args.iter().zip(sig) {
        let ok = *a == Type::Unknown || if want.is_integer() { a.is_integer() } else { super::implicit_ok(a, &Type::Text) };
        if !ok {
            return Err(no_function(name, args));
        }
    }
    resolved(f, sig[..args.len()].to_vec(), ret)
}

/// `to_bin`, `to_oct`, `to_hex`.
fn resolve_radix(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    let err = || no_function(name, args);
    match name {
        "to_bin" | "to_oct" => {
            let f = if name == "to_bin" {
                strict("to_bin", |a, _| Ok(Value::Text(radix(&a[0], 2))))
            } else {
                strict("to_oct", |a, _| Ok(Value::Text(radix(&a[0], 8))))
            };
            match args {
                [Type::Int8] => resolved(f, vec![Type::Int8], Type::Text),
                [t] if t.is_integer() || *t == Type::Unknown => resolved(f, vec![Type::Int4], Type::Text),
                _ => Err(err()),
            }
        }
        "to_hex" => match args {
            [Type::Int8] => resolved(strict("to_hex", to_hex), vec![Type::Int8], Type::Text),
            [t] if t.is_integer() || *t == Type::Unknown => resolved(strict("to_hex", to_hex), vec![Type::Int4], Type::Text),
            _ => Err(err()),
        },
        _ => Ok(None),
    }
}

fn resolve_more(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    if let Some(r) = resolve_radix(name, args)? {
        return Ok(Some(r));
    }
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
        "quote_literal" => match args {
            [t] if !is_texty(t) && *t != Type::Varchar && *t != Type::Bpchar => {
                resolved(strict("quote_literal", |a, _| Ok(Value::Text(quote_literal(&any_text(&a[0])?)))), vec![t.clone()], Type::Text)
            }
            _ => text_fn(strict("quote_literal", |a, _| Ok(Value::Text(quote_literal(s(&a[0]))))), 1, Type::Text),
        },
        "unistr" => text_fn(strict("unistr", unistr), 1, Type::Text),
        "normalize" if n == 1 || n == 2 => text_fn(strict("normalize", normalize), n, Type::Text),
        "is_normalized" if n == 1 || n == 2 => text_fn(strict("is_normalized", is_normalized), n, Type::Bool),
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
        "get_byte" if n == 2 => resolved(strict("get_byte", get_byte), vec![Type::Bytea, Type::Int4], Type::Int4),
        "set_byte" if n == 3 => resolved(strict("set_byte", set_byte), vec![Type::Bytea, Type::Int4, Type::Int4], Type::Bytea),
        "get_bit" if n == 2 && matches!(args[0], Type::Bytea | Type::Unknown) => {
            resolved(strict("get_bit", get_bit), vec![Type::Bytea, Type::Int8], Type::Int4)
        }
        "set_bit" if n == 3 && matches!(args[0], Type::Bytea | Type::Unknown) => {
            resolved(strict("set_bit", set_bit), vec![Type::Bytea, Type::Int8, Type::Int4], Type::Bytea)
        }
        "convert_to" if n == 2 => resolved(strict("convert_to", convert_to), vec![Type::Text, Type::Name], Type::Bytea),
        "convert_from" if n == 2 => resolved(strict("convert_from", convert_from), vec![Type::Bytea, Type::Name], Type::Text),
        "convert" if n == 3 => resolved(strict("convert", convert), vec![Type::Bytea, Type::Name, Type::Name], Type::Bytea),
        "encode" if n == 2 => resolved(strict("encode", encode), vec![Type::Bytea, Type::Text], Type::Text),
        "decode" if n == 2 => resolved(strict("decode", decode), texts(2), Type::Bytea),
        "regexp_replace" => match args {
            [_, _, _] => text_fn(strict("regexp_replace", regexp_replace), 3, Type::Text),
            [_, _, _, t] if !t.is_integer() => text_fn(strict("regexp_replace", regexp_replace), 4, Type::Text),
            [_, _, _, _] | [_, _, _, _, _] | [_, _, _, _, _, _] => {
                regex_sig(name, args, &[T, T, T, I, I, T], 4, strict("regexp_replace", regexp_replace_n), Type::Text)
            }
            _ => Err(err()),
        },
        "regexp_count" if n > 2 => regex_sig(name, args, &[T, T, I, T], 2, strict("regexp_count", regexp_count), Type::Int4),
        "regexp_instr" => regex_sig(name, args, &[T, T, I, I, I, T, I], 2, strict("regexp_instr", regexp_instr), Type::Int4),
        "regexp_substr" => regex_sig(name, args, &[T, T, I, I, T, I], 2, strict("regexp_substr", regexp_substr), Type::Text),
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
        "regexp_count" if n == 2 => text_fn(strict("regexp_count", regexp_count), 2, Type::Int4),
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
        // Only reached for `x ~~ ANY (...)`; a plain `~~` is bound as LIKE.
        "~~" => strict("~~", like_fn),
        "~~*" => strict("~~*", ilike_fn),
        "!~~" => strict("!~~", |a, c| like_fn(a, c).map(|v| Value::Bool(!v.is_true()))),
        "!~~*" => strict("!~~*", |a, c| ilike_fn(a, c).map(|v| Value::Bool(!v.is_true()))),
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
    let mut quotes = 0;
    while let Some(c) = chars.next() {
        if Some(c) == esc {
            match chars.next() {
                // `esc"` separators split the pattern into prefix, returned part and suffix
                // (`similar_escape_internal`).
                Some('"') => {
                    match quotes {
                        0 => out.push_str("){1,1}?("),
                        1 => out.push_str("){1,1}(?:"),
                        _ => {
                            return Err(SqlError::new(
                                code::INVALID_REGULAR_EXPRESSION,
                                "SQL regular expression may not contain more than two escape-double-quote separators",
                            ));
                        }
                    }
                    quotes += 1;
                }
                Some(n) => out.push_str(&regex::escape(&n.to_string())),
                None => {}
            }
            continue;
        }
        match c {
            '(' if !in_bracket => out.push_str("(?:"),
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
    // `parse_re_flags`: later flags override earlier ones.
    let (mut icase, mut quote, mut expanded) = (false, false, false);
    let (mut multi_line, mut dot_nl) = (false, true);
    for f in flags.chars() {
        match f {
            'i' => icase = true,
            'c' => icase = false,
            'n' | 'm' => (multi_line, dot_nl) = (true, false),
            's' => (multi_line, dot_nl) = (false, true),
            'p' => (multi_line, dot_nl) = (false, false),
            'w' => (multi_line, dot_nl) = (true, true),
            'x' => expanded = true,
            't' => expanded = false,
            'q' => quote = true,
            'g' | 'e' | 'b' => {}
            other => {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid regular expression option: \"{other}\"")));
            }
        }
    }
    let body = if quote { regex::escape(pattern) } else { translate_are(pattern) };
    let mut b = regex::RegexBuilder::new(&body);
    b.case_insensitive(icase).multi_line(multi_line).dot_matches_new_line(dot_nl).ignore_whitespace(expanded && !quote);
    b.build().map_err(|e| SqlError::new(code::INVALID_REGULAR_EXPRESSION, format!("invalid regular expression: {}", regex_error(&e))))
}

/// Postgres' wording (`regerror`) for the common syntax errors.
fn regex_error(e: &regex::Error) -> String {
    let msg = e.to_string();
    let detail = msg.lines().rev().find(|l| l.trim_start().starts_with("error:")).map_or(msg.as_str(), |l| l.trim_start()[6..].trim());
    let known = [
        ("unclosed group", "parentheses () not balanced"),
        ("unopened group", "parentheses () not balanced"),
        ("unclosed character class", "brackets [] not balanced"),
        ("repetition operator missing expression", "quantifier operand invalid"),
        ("unclosed counted repetition", "braces {} not balanced"),
        ("invalid character class range", "invalid character range"),
        ("invalid escape sequence", "invalid escape \\ sequence"),
        ("unrecognized escape sequence", "invalid escape \\ sequence"),
        ("decimal literal", "invalid repetition count(s)"),
        ("counted repetition", "invalid repetition count(s)"),
        ("repetition count", "invalid repetition count(s)"),
        ("backreferences are not supported", "invalid backreference number"),
    ];
    known.iter().find(|(k, _)| detail.contains(k)).map_or_else(|| detail.to_owned(), |(_, v)| (*v).to_owned())
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
/// `substring(text|bytea, start [, count])`: characters of text, bytes of bytea.
fn substring(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let start = int(&a[1]);
    let (lo, hi) = substring_range(
        start,
        a.get(2).map(int),
        match &a[0] {
            Value::Bytea(b) => b.len(),
            v => s(v).chars().count(),
        },
    )?;
    Ok(match &a[0] {
        Value::Bytea(b) => Value::Bytea(b[lo..hi].to_vec()),
        v => Value::Text(s(v).chars().skip(lo).take(hi - lo).collect()),
    })
}

/// The 0-based `[lo, hi)` range `substring(x, start, count)` selects from `len` items.
fn substring_range(start: i64, count: Option<i64>, len: usize) -> SqlResult<(usize, usize)> {
    let end = match count {
        Some(count) if count < 0 => return Err(SqlError::new("22011", "negative substring length not allowed")),
        Some(count) => start.saturating_add(count),
        None => i64::MAX,
    };
    let lo = start.max(1);
    let hi = end.min(i64::try_from(len).unwrap_or(i64::MAX) + 1);
    if hi <= lo {
        return Ok((0, 0));
    }
    Ok((usize::try_from(lo - 1).unwrap_or(0), usize::try_from(hi - 1).unwrap_or(0)))
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

/// `to_bin` / `to_oct`: the two's-complement digits of the integer's own width.
fn radix(v: &Value, base: u32) -> String {
    #[allow(clippy::cast_sign_loss)]
    let n: u64 = match v {
        Value::Int8(n) => *n as u64,
        v => u64::from(int(v) as u32),
    };
    match base {
        2 => format!("{n:b}"),
        _ => format!("{n:o}"),
    }
}

/// `unistr`: `\XXXX`, `\+XXXXXX`, `\uXXXX`, `\UXXXXXXXX` escapes and `\\`.
fn unistr(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let bad = || {
        SqlError::new("22025", "invalid Unicode escape").with_hint("Unicode escapes must be \\XXXX, \\+XXXXXX, \\uXXXX, or \\UXXXXXXXX.")
    };
    let src: Vec<char> = s(&a[0]).chars().collect();
    let mut out = String::new();
    let mut pending_high: Option<u32> = None;
    let mut i = 0;
    while i < src.len() {
        if src[i] != '\\' {
            if pending_high.is_some() {
                return Err(SqlError::new("22025", "invalid Unicode surrogate pair"));
            }
            out.push(src[i]);
            i += 1;
            continue;
        }
        let (skip, digits) = match src.get(i + 1) {
            Some('\\') => {
                out.push('\\');
                i += 2;
                continue;
            }
            Some('+') => (2, 6),
            Some('u') => (2, 4),
            Some('U') => (2, 8),
            Some(c) if c.is_ascii_hexdigit() => (1, 4),
            _ => return Err(bad()),
        };
        let hex: String = src.get(i + skip..i + skip + digits).ok_or_else(bad)?.iter().collect();
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let cp = u32::from_str_radix(&hex, 16).map_err(|_| bad())?;
        i += skip + digits;
        if (0xD800..0xDC00).contains(&cp) {
            if pending_high.is_some() {
                return Err(SqlError::new("22025", "invalid Unicode surrogate pair"));
            }
            pending_high = Some(cp);
            continue;
        }
        let cp = if (0xDC00..0xE000).contains(&cp) {
            let hi = pending_high.take().ok_or_else(|| SqlError::new("22025", "invalid Unicode surrogate pair"))?;
            0x10000 + ((hi - 0xD800) << 10) + (cp - 0xDC00)
        } else {
            if pending_high.is_some() {
                return Err(SqlError::new("22025", "invalid Unicode surrogate pair"));
            }
            cp
        };
        out.push(char::from_u32(cp).ok_or_else(|| SqlError::new("22025", format!("invalid Unicode code point: {cp:04X}")))?);
    }
    if pending_high.is_some() {
        return Err(SqlError::new("22025", "invalid Unicode surrogate pair"));
    }
    Ok(Value::Text(out))
}

fn normalization_form(a: &[Value]) -> SqlResult<()> {
    let form = a.get(1).map_or("NFC", s).trim().to_ascii_uppercase();
    if !matches!(form.as_str(), "NFC" | "NFD" | "NFKC" | "NFKD") {
        return Err(SqlError::new(code::SYNTAX_ERROR, format!("invalid normalization form: {form}")));
    }
    Ok(())
}

/// `normalize` / `IS NORMALIZED`: ASCII text is normalized in every form; pgmem has no
/// Unicode normalization tables for anything else.
fn normalize(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    normalization_form(a)?;
    if !s(&a[0]).is_ascii() {
        return Err(SqlError::feature_not_supported("Unicode normalization of non-ASCII text is not supported by pgmem"));
    }
    Ok(a[0].clone())
}

fn is_normalized(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    normalization_form(a)?;
    if !s(&a[0]).is_ascii() {
        return Err(SqlError::feature_not_supported("Unicode normalization of non-ASCII text is not supported by pgmem"));
    }
    Ok(Value::Bool(true))
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
        // Optional flags/width; `*` (or `*n$`) takes the width from an argument.
        let mut left_align = fmt.get(i) == Some(&'-');
        if left_align {
            i += 1;
        }
        let mut width = String::new();
        if fmt.get(i) == Some(&'*') {
            i += 1;
            let mut j = i;
            while j < fmt.len() && fmt[j].is_ascii_digit() {
                j += 1;
            }
            let widx = if j > i && fmt.get(j) == Some(&'$') {
                let p = fmt[i..j].iter().collect::<String>().parse::<usize>().unwrap_or(0);
                i = j + 1;
                if p == 0 {
                    return Err(SqlError::new(
                        code::INVALID_PARAMETER_VALUE,
                        "format specifies argument 0, but arguments are numbered from 1",
                    ));
                }
                p - 1
            } else {
                next
            };
            next = widx + 1;
            let warg = args.get(widx).ok_or_else(|| SqlError::new(code::INVALID_PARAMETER_VALUE, "too few arguments for format()"))?;
            let w = if warg.is_null() {
                0
            } else {
                any_text(warg)?.trim().parse::<i64>().map_err(|_| SqlError::invalid_text("integer", &any_text(warg).unwrap_or_default()))?
            };
            if w < 0 {
                left_align = true;
            }
            width = w.unsigned_abs().to_string();
        }
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
        "hex" => text::format_bytea_hex(b)[2..].to_owned(),
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

fn regexp_replace(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let flags = a.get(3).cloned().unwrap_or(Value::Text(String::new()));
    regexp_replace_n(
        &[a[0].clone(), a[1].clone(), a[2].clone(), Value::Int4(1), Value::Int4(if s(&flags).contains('g') { 0 } else { 1 }), flags],
        ctx,
    )
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

/// The byte offset of 1-based character position `start` (validated), or None past the end.
fn start_offset(src: &str, start: i64) -> SqlResult<Option<usize>> {
    if start <= 0 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"start\": {start}")));
    }
    let skip = usize::try_from(start - 1).unwrap_or(usize::MAX);
    Ok(if skip == 0 { Some(0) } else { src.char_indices().map(|(i, _)| i).chain(std::iter::once(src.len())).nth(skip) })
}

/// Successive matches from byte offset `from`, as Postgres finds them: the search resumes at
/// the end of each match, one character further after an empty one.
fn matches_from<'h>(re: &regex::Regex, src: &'h str, from: usize) -> Vec<regex::Captures<'h>> {
    let mut out = Vec::new();
    let mut pos = from;
    while pos <= src.len() {
        let Some(c) = re.captures_at(src, pos) else { break };
        let m = c.get(0).map_or((pos, pos), |m| (m.start(), m.end()));
        pos = if m.1 == m.0 { m.1 + src[m.1..].chars().next().map_or(1, char::len_utf8) } else { m.1 };
        out.push(c);
    }
    out
}

fn no_global(flags: &str, func: &str) -> SqlResult<()> {
    if flags.contains('g') {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("{func}() does not support the \"global\" option")));
    }
    Ok(())
}

fn positive(v: Option<&Value>, param: &str, default: i64) -> SqlResult<i64> {
    let n = v.map_or(default, int);
    if n <= 0 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"{param}\": {n}")));
    }
    Ok(n)
}

/// `regexp_replace(source, pattern, replacement, start [, N [, flags]])`.
fn regexp_replace_n(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let src = s(&a[0]);
    let flags = a.get(5).map_or("", s);
    let re = compile(s(&a[1]), flags)?;
    let n = a.get(4).map_or(if flags.contains('g') { 0 } else { 1 }, int);
    if n < 0 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"n\": {n}")));
    }
    let all = n == 0 || flags.contains('g');
    let Some(from) = start_offset(src, int(&a[3]))? else { return Ok(Value::Text(src.to_owned())) };
    let rep = replacement(s(&a[2]));
    let mut out = String::with_capacity(src.len());
    let mut last = 0;
    for (i, c) in matches_from(&re, src, from).iter().enumerate() {
        if !all && i64::try_from(i + 1).unwrap_or(i64::MAX) != n {
            continue;
        }
        let m = c.get(0).map_or(0..0, |m| m.range());
        out.push_str(&src[last..m.start]);
        c.expand(&rep, &mut out);
        last = m.end;
        if !all {
            break;
        }
    }
    out.push_str(&src[last..]);
    Ok(Value::Text(out))
}

/// `regexp_count(string, pattern [, start [, flags]])`.
fn regexp_count(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let src = s(&a[0]);
    let flags = a.get(3).map_or("", s);
    no_global(flags, "regexp_count")?;
    let re = compile(s(&a[1]), flags)?;
    let Some(from) = start_offset(src, a.get(2).map_or(1, int))? else { return Ok(Value::Int4(0)) };
    Ok(Value::Int4(i32::try_from(matches_from(&re, src, from).len()).unwrap_or(i32::MAX)))
}

/// The Nth match's capture `subexpr` (0 = the whole match), for regexp_instr / regexp_substr.
fn nth_match<'h>(src: &'h str, a: &[Value], n_at: usize, flags: &str, sub_at: usize, func: &str) -> SqlResult<Option<regex::Match<'h>>> {
    no_global(flags, func)?;
    let re = compile(s(&a[1]), flags)?;
    let Some(from) = start_offset(src, a.get(2).map_or(1, int))? else { return Ok(None) };
    let n = positive(a.get(n_at), "n", 1)?;
    let sub = a.get(sub_at).map_or(0, int);
    if sub < 0 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"subexpr\": {sub}")));
    }
    let all = matches_from(&re, src, from);
    let Some(c) = all.into_iter().nth(usize::try_from(n - 1).unwrap_or(usize::MAX)) else { return Ok(None) };
    Ok(c.get(usize::try_from(sub).unwrap_or(usize::MAX)))
}

/// `regexp_instr(string, pattern [, start [, N [, endoption [, flags [, subexpr]]]]])`.
fn regexp_instr(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let src = s(&a[0]);
    let end = a.get(4).map_or(0, int);
    if end != 0 && end != 1 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid value for parameter \"endoption\": {end}")));
    }
    let m = nth_match(src, a, 3, a.get(5).map_or("", s), 6, "regexp_instr")?;
    Ok(Value::Int4(m.map_or(0, |m| {
        let at = if end == 1 { m.end() } else { m.start() };
        i32::try_from(src[..at].chars().count() + 1).unwrap_or(i32::MAX)
    })))
}

/// `regexp_substr(string, pattern [, start [, N [, flags [, subexpr]]]])`.
fn regexp_substr(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let src = s(&a[0]);
    let m = nth_match(src, a, 3, a.get(4).map_or("", s), 5, "regexp_substr")?;
    Ok(m.map_or(Value::Null, |m| Value::Text(m.as_str().to_owned())))
}

/// `regexp_matches(string, pattern [, flags])`: one text[] row per match.
pub fn regexp_matches(a: &[Value], _: &EvalCtx) -> SqlResult<Vec<crate::plan::Row>> {
    if a.iter().any(Value::is_null) {
        return Ok(Vec::new());
    }
    let src = s(&a[0]);
    let flags = a.get(2).map_or("", s);
    let re = compile(s(&a[1]), flags)?;
    let mut found = matches_from(&re, src, 0);
    if !flags.contains('g') {
        found.truncate(1);
    }
    Ok(found
        .iter()
        .map(|c| {
            let arr = if c.len() == 1 {
                vec![Value::Text(c.get(0).map_or("", |m| m.as_str()).to_owned())]
            } else {
                (1..c.len()).map(|i| c.get(i).map_or(Value::Null, |m| Value::Text(m.as_str().to_owned()))).collect()
            };
            vec![Value::Array(arr)]
        })
        .collect())
}

/// `regexp_split_to_table(string, pattern [, flags])`.
pub fn regexp_split_to_table(a: &[Value], ctx: &EvalCtx) -> SqlResult<Vec<crate::plan::Row>> {
    if a.iter().any(Value::is_null) {
        return Ok(Vec::new());
    }
    match regexp_split(a, ctx)? {
        Value::Array(items) => Ok(items.into_iter().map(|v| vec![v]).collect()),
        _ => Ok(Vec::new()),
    }
}

/// `regexp_split_to_array`: the pieces between matches, ignoring zero-length matches at
/// the start, at the end, or right after another match.
fn regexp_split(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let re = compile(s(&a[1]), a.get(2).map_or("", s))?;
    let src = s(&a[0]);
    let mut out = Vec::new();
    let (mut last, mut prev_end) = (0, 0);
    for c in matches_from(&re, src, 0) {
        let Some(m) = c.get(0) else { continue };
        if m.start() < src.len() && m.end() > prev_end {
            out.push(Value::Text(src[last..m.start()].to_owned()));
            last = m.end();
        }
        prev_end = m.end();
    }
    out.push(Value::Text(src[last..].to_owned()));
    Ok(Value::Array(out))
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

// ---------------------------------------------------------------------------------------------
// bytea byte/bit access and encoding conversion
// ---------------------------------------------------------------------------------------------

fn bytes_of(v: &Value) -> &[u8] {
    match v {
        Value::Bytea(b) => b,
        Value::Text(t) => t.as_bytes(),
        _ => &[],
    }
}

fn byte_index(b: &[u8], n: i64, unit: i64) -> SqlResult<usize> {
    let len = i64::try_from(b.len()).unwrap_or(i64::MAX) * unit;
    if n < 0 || n >= len {
        return Err(SqlError::new("2202E", format!("index {n} out of valid range, 0..{}", len - 1)));
    }
    Ok(usize::try_from(n).unwrap_or(0))
}

fn get_byte(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let b = bytes_of(&a[0]);
    let i = byte_index(b, a[1].as_i64().unwrap_or(0), 1)?;
    Ok(Value::Int4(i32::from(b[i])))
}

fn set_byte(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut b = bytes_of(&a[0]).to_vec();
    let i = byte_index(&b, a[1].as_i64().unwrap_or(0), 1)?;
    // set_byte keeps the low 8 bits of the new value.
    b[i] = u8::try_from(a[2].as_i64().unwrap_or(0) & 0xff).unwrap_or(0);
    Ok(Value::Bytea(b))
}

/// Bits are numbered from the least significant bit of the first byte.
fn get_bit(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let b = bytes_of(&a[0]);
    let n = byte_index(b, a[1].as_i64().unwrap_or(0), 8)?;
    Ok(Value::Int4(i32::from((b[n / 8] >> (n % 8)) & 1)))
}

fn set_bit(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let mut b = bytes_of(&a[0]).to_vec();
    let n = byte_index(&b, a[1].as_i64().unwrap_or(0), 8)?;
    match a[2].as_i64() {
        Some(0) => b[n / 8] &= !(1 << (n % 8)),
        Some(1) => b[n / 8] |= 1 << (n % 8),
        _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "new bit must be 0 or 1")),
    }
    Ok(Value::Bytea(b))
}

/// The encodings pgmem converts between, by Postgres' (case- and punctuation-insensitive) name.
#[derive(Clone, Copy, PartialEq)]
enum Encoding {
    Utf8,
    Latin1,
    SqlAscii,
}

fn encoding(name: &str, what: &str) -> SqlResult<Encoding> {
    let norm: String = name.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    Ok(match norm.as_str() {
        "utf8" | "unicode" => Encoding::Utf8,
        "latin1" | "iso88591" => Encoding::Latin1,
        "sqlascii" => Encoding::SqlAscii,
        _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("invalid {what} encoding name \"{name}\""))),
    })
}

fn invalid_utf8(b: &[u8], at: usize) -> SqlError {
    let seq: Vec<String> = b[at..b.len().min(at + 2)].iter().map(|x| format!("0x{x:02x}")).collect();
    SqlError::new("22021", format!("invalid byte sequence for encoding \"UTF8\": {}", seq.join(" ")))
}

/// Decodes `b` from `enc` into a string.
fn decode_from(b: &[u8], enc: Encoding) -> SqlResult<String> {
    match enc {
        Encoding::Utf8 | Encoding::SqlAscii => match std::str::from_utf8(b) {
            Ok(s) if !s.contains('\0') => Ok(s.to_owned()),
            Ok(s) => Err(invalid_utf8(b, s.find('\0').unwrap_or(0))),
            Err(e) => Err(invalid_utf8(b, e.valid_up_to())),
        },
        Encoding::Latin1 => Ok(b.iter().map(|&x| char::from(x)).collect()),
    }
}

/// Encodes `s` into `enc`.
fn encode_to(s: &str, enc: Encoding) -> SqlResult<Vec<u8>> {
    match enc {
        Encoding::Utf8 | Encoding::SqlAscii => Ok(s.as_bytes().to_vec()),
        Encoding::Latin1 => s
            .chars()
            .map(|c| {
                u8::try_from(u32::from(c)).map_err(|_| {
                    let bytes: Vec<String> = c.to_string().bytes().map(|x| format!("0x{x:02x}")).collect();
                    SqlError::new(
                        "22P05",
                        format!(
                            "character with byte sequence {} in encoding \"UTF8\" has no equivalent in encoding \"LATIN1\"",
                            bytes.join(" ")
                        ),
                    )
                })
            })
            .collect(),
    }
}

fn convert_to(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let enc = encoding(s(&a[1]), "destination")?;
    Ok(Value::Bytea(encode_to(s(&a[0]), enc)?))
}

fn convert_from(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let enc = encoding(s(&a[1]), "source")?;
    Ok(Value::Text(decode_from(bytes_of(&a[0]), enc)?))
}

fn convert(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let from = encoding(s(&a[1]), "source")?;
    let to = encoding(s(&a[2]), "destination")?;
    let text = decode_from(bytes_of(&a[0]), from)?;
    Ok(Value::Bytea(encode_to(&text, to)?))
}
