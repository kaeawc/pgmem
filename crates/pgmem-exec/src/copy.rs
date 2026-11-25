//! COPY's data formats, following `copy.c` (option validation), `copyto.c` (output) and
//! `copyfromparse.c` (input): text, CSV and binary.

use pgmem_ir::{CopyOption, CopyOptionValue};
use pgmem_types::{SqlError, SqlResult, code};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Csv,
    Binary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Header {
    No,
    Yes,
    Match,
}

/// FORCE_QUOTE / FORCE_NOT_NULL / FORCE_NULL: unset, `*`, or a column list.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Force {
    #[default]
    Unset,
    All,
    Columns(Vec<String>),
}

impl Force {
    pub fn applies(&self, column: &str) -> bool {
        match self {
            Force::Unset => false,
            Force::All => true,
            Force::Columns(c) => c.iter().any(|x| x == column),
        }
    }

    fn is_set(&self) -> bool {
        *self != Force::Unset
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Silent,
    Default,
    Verbose,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub kind: Kind,
    pub delimiter: u8,
    pub null: String,
    pub default: Option<String>,
    pub header: Header,
    pub quote: u8,
    pub escape: u8,
    pub force_quote: Force,
    pub force_not_null: Force,
    pub force_null: Force,
    pub freeze: bool,
    /// ON_ERROR IGNORE: rows with values their type does not accept are skipped.
    pub ignore_errors: bool,
    pub reject_limit: Option<i64>,
    pub verbosity: Verbosity,
}

fn invalid(msg: impl Into<String>) -> SqlError {
    SqlError::new(code::INVALID_PARAMETER_VALUE, msg)
}

fn unsupported(msg: impl Into<String>) -> SqlError {
    SqlError::new(code::FEATURE_NOT_SUPPORTED, msg)
}

fn string_arg(o: &CopyOption) -> SqlResult<String> {
    match &o.value {
        CopyOptionValue::Str(s) => Ok(s.clone()),
        _ => Err(SqlError::syntax(format!("{} requires a parameter", o.name))),
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    let l = s.to_ascii_lowercase();
    match l.as_str() {
        "t" | "tr" | "tru" | "true" | "y" | "ye" | "yes" | "on" | "1" => Some(true),
        "f" | "fa" | "fal" | "fals" | "false" | "n" | "no" | "of" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn bool_arg(o: &CopyOption) -> SqlResult<bool> {
    match &o.value {
        CopyOptionValue::None => Ok(true),
        CopyOptionValue::Str(s) if s != "o" => {
            parse_bool(s).ok_or_else(|| SqlError::syntax(format!("{} requires a Boolean value", o.name)))
        }
        _ => Err(SqlError::syntax(format!("{} requires a Boolean value", o.name))),
    }
}

fn header_arg(o: &CopyOption, from: bool) -> SqlResult<Header> {
    let s = match &o.value {
        CopyOptionValue::None => return Ok(Header::Yes),
        CopyOptionValue::Str(s) => s,
        _ => return Err(SqlError::syntax("header requires a Boolean value or \"match\"")),
    };
    if s.eq_ignore_ascii_case("match") {
        if !from {
            return Err(unsupported("cannot use \"match\" with HEADER in COPY TO"));
        }
        return Ok(Header::Match);
    }
    match parse_bool(s) {
        Some(true) => Ok(Header::Yes),
        Some(false) => Ok(Header::No),
        None => Err(SqlError::syntax("header requires a Boolean value or \"match\"")),
    }
}

fn force_arg(o: &CopyOption) -> SqlResult<Force> {
    match &o.value {
        CopyOptionValue::Star => Ok(Force::All),
        CopyOptionValue::List(c) => Ok(Force::Columns(c.clone())),
        _ => Err(invalid(format!("argument to option \"{}\" must be a list of column names", o.name))),
    }
}

const ENCODINGS: &[&str] = &[
    "utf8",
    "unicode",
    "sql_ascii",
    "latin1",
    "latin2",
    "latin9",
    "win1252",
    "win1251",
    "euc_jp",
    "sjis",
    "gbk",
    "big5",
    "koi8r",
    "iso_8859_5",
];

/// The options as written, before defaults: (name, value) with duplicates rejected.
#[derive(Default)]
struct Given {
    format: Option<Kind>,
    delimiter: Option<String>,
    null: Option<String>,
    default: Option<String>,
    header: Option<Header>,
    quote: Option<String>,
    escape: Option<String>,
    force_quote: Force,
    force_not_null: Force,
    force_null: Force,
    freeze: Option<bool>,
    encoding: bool,
    on_error: Option<bool>,
    reject_limit: Option<i64>,
    verbosity: Option<Verbosity>,
}

fn conflicting() -> SqlError {
    SqlError::syntax("conflicting or redundant options")
}

fn set<T>(slot: &mut Option<T>, v: T) -> SqlResult<()> {
    if slot.is_some() {
        return Err(conflicting());
    }
    *slot = Some(v);
    Ok(())
}

fn set_force(slot: &mut Force, o: &CopyOption) -> SqlResult<()> {
    if slot.is_set() {
        return Err(conflicting());
    }
    *slot = force_arg(o)?;
    Ok(())
}

fn on_error_arg(o: &CopyOption, from: bool) -> SqlResult<bool> {
    if !from {
        return Err(invalid("COPY ON_ERROR cannot be used with COPY TO"));
    }
    let s = string_arg(o)?;
    match s.to_ascii_lowercase().as_str() {
        "stop" => Ok(false),
        "ignore" => Ok(true),
        _ => Err(invalid(format!("COPY ON_ERROR \"{s}\" not recognized"))),
    }
}

fn verbosity_arg(o: &CopyOption) -> SqlResult<Verbosity> {
    let s = string_arg(o)?;
    match s.to_ascii_lowercase().as_str() {
        "silent" => Ok(Verbosity::Silent),
        "default" => Ok(Verbosity::Default),
        "verbose" => Ok(Verbosity::Verbose),
        _ => Err(invalid(format!("COPY LOG_VERBOSITY \"{s}\" not recognized"))),
    }
}

fn collect(list: &[CopyOption], from: bool) -> SqlResult<Given> {
    let mut g = Given::default();
    for o in list {
        match o.name.as_str() {
            "format" => {
                let f = string_arg(o)?;
                let kind = match f.as_str() {
                    "text" => Kind::Text,
                    "csv" => Kind::Csv,
                    "binary" => Kind::Binary,
                    _ => return Err(invalid(format!("COPY format \"{f}\" not recognized"))),
                };
                set(&mut g.format, kind)?;
            }
            "freeze" => set(&mut g.freeze, bool_arg(o)?)?,
            "delimiter" => set(&mut g.delimiter, string_arg(o)?)?,
            "null" => set(&mut g.null, string_arg(o)?)?,
            "default" => set(&mut g.default, string_arg(o)?)?,
            "header" => set(&mut g.header, header_arg(o, from)?)?,
            "quote" => set(&mut g.quote, string_arg(o)?)?,
            "escape" => set(&mut g.escape, string_arg(o)?)?,
            "force_quote" => set_force(&mut g.force_quote, o)?,
            "force_not_null" => set_force(&mut g.force_not_null, o)?,
            "force_null" => set_force(&mut g.force_null, o)?,
            "encoding" => {
                if g.encoding {
                    return Err(conflicting());
                }
                let e = string_arg(o)?.to_ascii_lowercase().replace(['-', '_'], "");
                if !ENCODINGS.iter().any(|x| x.replace('_', "") == e) {
                    return Err(invalid(format!("argument to option \"{}\" must be a valid encoding name", o.name)));
                }
                g.encoding = true;
            }
            "on_error" => set(&mut g.on_error, on_error_arg(o, from)?)?,
            "reject_limit" => {
                let s = string_arg(o)?;
                let n: i64 = s.parse().map_err(|_| {
                    SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type bigint: \"{s}\""))
                })?;
                if n <= 0 {
                    return Err(invalid(format!("REJECT_LIMIT ({n}) must be greater than zero")));
                }
                set(&mut g.reject_limit, n)?;
            }
            "log_verbosity" => set(&mut g.verbosity, verbosity_arg(o)?)?,
            other => return Err(SqlError::syntax(format!("option \"{other}\" not recognized"))),
        }
    }
    Ok(g)
}

fn one_byte(s: &str, what: &str) -> SqlResult<u8> {
    match s.as_bytes() {
        [b] => Ok(*b),
        _ => Err(unsupported(format!("COPY {what} must be a single one-byte character"))),
    }
}

fn binary_conflicts(g: &Given) -> SqlResult<()> {
    for (set, what) in [(g.delimiter.is_some(), "DELIMITER"), (g.null.is_some(), "NULL"), (g.default.is_some(), "DEFAULT")] {
        if set {
            return Err(SqlError::syntax(format!("cannot specify {what} in BINARY mode")));
        }
    }
    Ok(())
}

fn requires_csv(g: &Given, csv: bool, from: bool) -> SqlResult<()> {
    let csv_only = [
        (g.quote.is_some(), "QUOTE"),
        (g.escape.is_some(), "ESCAPE"),
        (g.force_quote.is_set(), "FORCE_QUOTE"),
        (g.force_not_null.is_set(), "FORCE_NOT_NULL"),
        (g.force_null.is_set(), "FORCE_NULL"),
    ];
    for (set, what) in csv_only {
        if set && !csv {
            return Err(unsupported(format!("COPY {what} requires CSV mode")));
        }
        let wrong_direction = match what {
            "FORCE_QUOTE" => from,
            "FORCE_NOT_NULL" | "FORCE_NULL" => !from,
            _ => false,
        };
        if set && wrong_direction {
            let msg = format!("COPY {what} cannot be used with {}", if from { "COPY FROM" } else { "COPY TO" });
            return Err(if from { unsupported(msg) } else { invalid(msg) });
        }
    }
    Ok(())
}

fn check_strings(o: &Options, from: bool) -> SqlResult<()> {
    let delim = o.delimiter as char;
    if o.null.contains(delim) {
        return Err(invalid("COPY delimiter character must not appear in the NULL specification"));
    }
    if o.kind == Kind::Csv && o.null.contains(o.quote as char) {
        return Err(invalid("CSV quote character must not appear in the NULL specification"));
    }
    if o.freeze && !from {
        return Err(invalid("COPY FREEZE cannot be used with COPY TO"));
    }
    if let Some(d) = &o.default {
        if !from {
            return Err(unsupported("COPY DEFAULT cannot be used with COPY TO"));
        }
        if d.contains(delim) {
            return Err(unsupported("COPY delimiter character must not appear in the DEFAULT specification"));
        }
        if o.kind == Kind::Csv && d.contains(o.quote as char) {
            return Err(unsupported("CSV quote character must not appear in the DEFAULT specification"));
        }
        if *d == o.null {
            return Err(unsupported("NULL specification and DEFAULT specification cannot be the same"));
        }
    }
    Ok(())
}

/// `ProcessCopyOptions`.
pub fn options(list: &[CopyOption], from: bool) -> SqlResult<Options> {
    let g = collect(list, from)?;
    let kind = g.format.unwrap_or(Kind::Text);
    let csv = kind == Kind::Csv;
    if kind == Kind::Binary {
        binary_conflicts(&g)?;
    }
    let delimiter = g.delimiter.clone().unwrap_or_else(|| if csv { ",".into() } else { "\t".into() });
    let null = g.null.clone().unwrap_or_else(|| if csv { String::new() } else { "\\N".into() });
    let delimiter = one_byte(&delimiter, "delimiter")?;
    if delimiter == b'\r' || delimiter == b'\n' {
        return Err(invalid("COPY delimiter cannot be newline or carriage return"));
    }
    if null.contains(['\r', '\n']) {
        return Err(invalid("COPY null representation cannot use newline or carriage return"));
    }
    if g.default.as_ref().is_some_and(|d| d.contains(['\r', '\n'])) {
        return Err(invalid("COPY default representation cannot use newline or carriage return"));
    }
    if !csv && b"\\.abcdefghijklmnopqrstuvwxyz0123456789".contains(&delimiter) {
        return Err(invalid(format!("COPY delimiter cannot be \"{}\"", delimiter as char)));
    }
    if kind == Kind::Binary && g.header.is_some_and(|h| h != Header::No) {
        return Err(unsupported("cannot specify HEADER in BINARY mode"));
    }
    requires_csv(&g, csv, from)?;
    let quote = if csv { one_byte(g.quote.as_deref().unwrap_or("\""), "quote")? } else { b'"' };
    if csv && delimiter == quote {
        return Err(invalid("COPY delimiter and quote must be different"));
    }
    let escape = match &g.escape {
        Some(e) => one_byte(e, "escape")?,
        None => quote,
    };
    let ignore_errors = g.on_error.unwrap_or(false);
    if kind == Kind::Binary && ignore_errors {
        return Err(SqlError::syntax("only ON_ERROR STOP is allowed in BINARY mode"));
    }
    if g.reject_limit.is_some() && !ignore_errors {
        return Err(invalid("COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE"));
    }
    let o = Options {
        kind,
        delimiter,
        null,
        default: g.default,
        header: g.header.unwrap_or(Header::No),
        quote,
        escape,
        force_quote: g.force_quote,
        force_not_null: g.force_not_null,
        force_null: g.force_null,
        freeze: g.freeze.unwrap_or(false),
        ignore_errors,
        reject_limit: g.reject_limit,
        verbosity: g.verbosity.unwrap_or(Verbosity::Default),
    };
    check_strings(&o, from)?;
    Ok(o)
}

// ---------------------------------------------------------------------------------------------
// Output

/// `CopyAttributeOutText`.
fn text_attribute(o: &Options, s: &str, out: &mut Vec<u8>) {
    for &b in s.as_bytes() {
        let esc = match b {
            8 => Some(b'b'),
            12 => Some(b'f'),
            b'\n' => Some(b'n'),
            b'\r' => Some(b'r'),
            b'\t' => Some(b't'),
            11 => Some(b'v'),
            b'\\' => Some(b'\\'),
            _ if b == o.delimiter => Some(b),
            _ => None,
        };
        match esc {
            Some(e) => out.extend_from_slice(&[b'\\', e]),
            None => out.push(b),
        }
    }
}

/// `CopyAttributeOutCSV`.
fn csv_attribute(o: &Options, s: &str, force: bool, single: bool, out: &mut Vec<u8>) {
    let quote =
        force || s == o.null || (single && s == "\\.") || s.bytes().any(|b| b == o.delimiter || b == o.quote || b == b'\n' || b == b'\r');
    if !quote {
        out.extend_from_slice(s.as_bytes());
        return;
    }
    out.push(o.quote);
    for &b in s.as_bytes() {
        if b == o.quote || b == o.escape {
            out.push(o.escape);
        }
        out.push(b);
    }
    out.push(o.quote);
}

/// The header line (column names).
pub fn header_line(o: &Options, names: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push(o.delimiter);
        }
        match o.kind {
            Kind::Csv => csv_attribute(o, n, false, names.len() == 1, &mut out),
            _ => text_attribute(o, n, &mut out),
        }
    }
    out.push(b'\n');
    out
}

/// One text or CSV row (values already in their text form; None is NULL).
pub fn text_row(o: &Options, values: &[Option<String>], force: &[bool]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(o.delimiter);
        }
        match (v, o.kind) {
            (None, _) => out.extend_from_slice(o.null.as_bytes()),
            (Some(s), Kind::Csv) => csv_attribute(o, s, force.get(i).copied().unwrap_or(false), values.len() == 1, &mut out),
            (Some(s), _) => text_attribute(o, s, &mut out),
        }
    }
    out.push(b'\n');
    out
}

const SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";

pub fn binary_header() -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out
}

pub fn binary_row(values: &[Option<Vec<u8>>]) -> Vec<u8> {
    let mut out = i16::try_from(values.len()).unwrap_or(i16::MAX).to_be_bytes().to_vec();
    for v in values {
        match v {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(b) => {
                out.extend_from_slice(&i32::try_from(b.len()).unwrap_or(i32::MAX).to_be_bytes());
                out.extend_from_slice(b);
            }
        }
    }
    out
}

pub fn binary_trailer() -> Vec<u8> {
    (-1i16).to_be_bytes().to_vec()
}

// ---------------------------------------------------------------------------------------------
// Input

/// One input field of a text or CSV row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Field {
    Null,
    /// Matched the DEFAULT string: the column's default.
    Default,
    Text(String),
}

/// A row read from text or CSV input, with its line number and raw text (for messages).
pub struct InputRow {
    pub line: usize,
    pub fields: Vec<Field>,
}

fn bad_copy(msg: impl Into<String>) -> SqlError {
    SqlError::new(code::BAD_COPY_FILE_FORMAT, msg)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Eol {
    Nl,
    Cr,
    CrNl,
}

/// Checks a line terminator against the first one seen (`CopyReadLineText`).
fn check_eol(eol: &mut Option<Eol>, found: Eol, csv: bool) -> SqlResult<()> {
    let expected = *eol.get_or_insert(found);
    if expected == found {
        return Ok(());
    }
    let (msg, hint) = match (found, csv) {
        (Eol::Nl, false) => ("literal newline found in data", "Use \"\\n\" to represent newline."),
        (Eol::Nl, true) => ("unquoted newline found in data", "Use quoted CSV field to represent newline."),
        (_, false) => ("literal carriage return found in data", "Use \"\\r\" to represent carriage return."),
        (_, true) => ("unquoted carriage return found in data", "Use quoted CSV field to represent carriage return."),
    };
    Err(bad_copy(msg).with_hint(hint))
}

fn end_marker_error() -> SqlError {
    bad_copy("end-of-copy marker is not alone on its line")
}

/// Splits input into lines (quotes and backslash escapes are honoured, so a line may
/// contain escaped or quoted newlines). Stops at a `\.` line.
fn split_lines(o: &Options, data: &[u8]) -> SqlResult<Vec<Vec<u8>>> {
    let csv = o.kind == Kind::Csv;
    let mut lines = Vec::new();
    let mut eol = None;
    let mut cur = Vec::new();
    let mut in_quote = false;
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if csv && in_quote && b == o.escape && data.get(i + 1).is_some_and(|n| *n == o.quote || *n == o.escape) {
            cur.extend_from_slice(&data[i..i + 2]);
            i += 2;
            continue;
        }
        if csv && b == o.quote {
            in_quote = !in_quote;
        }
        if !in_quote && (b == b'\n' || b == b'\r') {
            let found = if b == b'\n' {
                Eol::Nl
            } else if data.get(i + 1) == Some(&b'\n') {
                Eol::CrNl
            } else {
                Eol::Cr
            };
            check_eol(&mut eol, found, csv)?;
            i += if found == Eol::CrNl { 2 } else { 1 };
            if cur == b"\\." {
                return Ok(lines);
            }
            lines.push(std::mem::take(&mut cur));
            continue;
        }
        if !csv && b == b'\\' {
            if data.get(i + 1) == Some(&b'.') {
                let next = data.get(i + 2);
                if !cur.is_empty() || !matches!(next, None | Some(b'\n' | b'\r')) {
                    return Err(end_marker_error());
                }
            }
            cur.push(b);
            if let Some(n) = data.get(i + 1) {
                // An escaped character never ends the line or the field.
                if *n != b'\n' && *n != b'\r' {
                    cur.push(*n);
                    i += 1;
                }
            }
            i += 1;
            continue;
        }
        cur.push(b);
        i += 1;
    }
    if in_quote {
        return Err(bad_copy("unterminated CSV quoted field"));
    }
    if !cur.is_empty() && cur != b"\\." {
        lines.push(cur);
    }
    Ok(lines)
}

fn utf8(bytes: Vec<u8>) -> SqlResult<String> {
    String::from_utf8(bytes).map_err(|e| {
        let bad = e.as_bytes()[e.utf8_error().valid_up_to()];
        SqlError::new(code::CHARACTER_NOT_IN_REPERTOIRE, format!("invalid byte sequence for encoding \"UTF8\": 0x{bad:02x}"))
    })
}

fn classify(o: &Options, raw: &[u8], value: Vec<u8>, quoted: bool) -> SqlResult<Field> {
    if !quoted && raw == o.null.as_bytes() {
        return Ok(Field::Null);
    }
    if !quoted && o.default.as_ref().is_some_and(|d| raw == d.as_bytes()) {
        return Ok(Field::Default);
    }
    Ok(Field::Text(utf8(value)?))
}

/// One backslash escape (`\n`, `\t`, octal, `\x` hex, or the character itself) at `line[i]`
/// (just after the backslash); returns the byte and the position after it.
fn text_escape(line: &[u8], i: usize) -> (Option<u8>, usize) {
    let Some(&c) = line.get(i) else { return (None, i) };
    let octal = |b: u8| (b'0'..=b'7').contains(&b);
    match c {
        b'0'..=b'7' => {
            let mut v = u32::from(c - b'0');
            let mut j = i + 1;
            while j < i + 3 && line.get(j).is_some_and(|b| octal(*b)) {
                v = v * 8 + u32::from(line[j] - b'0');
                j += 1;
            }
            (Some((v & 0xff) as u8), j)
        }
        b'x' if line.get(i + 1).is_some_and(u8::is_ascii_hexdigit) => {
            let mut v = 0u32;
            let mut j = i + 1;
            while j < i + 3 && line.get(j).is_some_and(u8::is_ascii_hexdigit) {
                v = v * 16 + (line[j] as char).to_digit(16).unwrap_or(0);
                j += 1;
            }
            (Some((v & 0xff) as u8), j)
        }
        b'b' => (Some(8), i + 1),
        b'f' => (Some(12), i + 1),
        b'n' => (Some(b'\n'), i + 1),
        b'r' => (Some(b'\r'), i + 1),
        b't' => (Some(b'\t'), i + 1),
        b'v' => (Some(11), i + 1),
        other => (Some(other), i + 1),
    }
}

/// `CopyReadAttributesText`.
fn text_fields(o: &Options, line: &[u8]) -> SqlResult<Vec<Field>> {
    let mut fields = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        let mut value = Vec::new();
        while i < line.len() && line[i] != o.delimiter {
            if line[i] == b'\\' {
                let (b, next) = text_escape(line, i + 1);
                value.extend(b);
                i = next;
            } else {
                value.push(line[i]);
                i += 1;
            }
        }
        fields.push(classify(o, &line[start..i], value, false)?);
        if i >= line.len() {
            return Ok(fields);
        }
        i += 1;
    }
}

/// `CopyReadAttributesCSV`: (field, was quoted).
fn csv_fields(o: &Options, line: &[u8]) -> SqlResult<Vec<(Field, bool)>> {
    let mut fields = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        let mut value = Vec::new();
        let mut quoted = false;
        let mut in_quote = false;
        while i < line.len() {
            let b = line[i];
            if in_quote {
                if b == o.escape && line.get(i + 1).is_some_and(|n| *n == o.quote || *n == o.escape) {
                    value.push(line[i + 1]);
                    i += 2;
                    continue;
                }
                if b == o.quote {
                    in_quote = false;
                } else {
                    value.push(b);
                }
            } else if b == o.delimiter {
                break;
            } else if b == o.quote {
                in_quote = true;
                quoted = true;
            } else {
                value.push(b);
            }
            i += 1;
        }
        if in_quote {
            return Err(bad_copy("unterminated CSV quoted field"));
        }
        fields.push((classify(o, &line[start..i], value, quoted)?, quoted));
        if i >= line.len() {
            return Ok(fields);
        }
        i += 1;
    }
}

fn check_header(o: &Options, fields: &[Field], columns: &[String]) -> SqlResult<()> {
    if fields.len() != columns.len() {
        return Err(bad_copy(format!("wrong number of fields in header line: got {}, expected {}", fields.len(), columns.len())));
    }
    for (i, (f, c)) in fields.iter().zip(columns).enumerate() {
        match f {
            Field::Text(s) if s == c => {}
            Field::Text(s) => {
                return Err(bad_copy(format!("column name mismatch in header line field {}: got \"{s}\", expected \"{c}\"", i + 1)));
            }
            _ => {
                return Err(bad_copy(format!(
                    "column name mismatch in header line field {}: got null value (\"{}\"), expected \"{c}\"",
                    i + 1,
                    o.null
                )));
            }
        }
    }
    Ok(())
}

fn line_fields(o: &Options, line: &[u8], columns: &[String]) -> SqlResult<Vec<Field>> {
    let mut fields = match o.kind {
        Kind::Csv => {
            let mut out = Vec::new();
            for (i, (f, quoted)) in csv_fields(o, line)?.into_iter().enumerate() {
                let col = columns.get(i).map_or("", String::as_str);
                out.push(match f {
                    Field::Null if o.force_not_null.applies(col) => Field::Text(o.null.clone()),
                    Field::Text(s) if quoted && s == o.null && o.force_null.applies(col) => Field::Null,
                    other => other,
                });
            }
            out
        }
        _ => text_fields(o, line)?,
    };
    // A table with no columns reads empty lines.
    if columns.is_empty() && fields == [Field::Text(String::new())] {
        fields.clear();
    }
    Ok(fields)
}

/// Parses text or CSV input into rows of at most `columns.len()` fields (a short row is an
/// error once the fields it has are converted: `missing_column`).
pub fn read_text(o: &Options, data: &[u8], columns: &[String]) -> SqlResult<Vec<InputRow>> {
    let lines = split_lines(o, data)?;
    let mut rows = Vec::with_capacity(lines.len());
    for (n, line) in lines.iter().enumerate() {
        let fields = line_fields(o, line, columns)?;
        if n == 0 && o.header != Header::No {
            if o.header == Header::Match {
                check_header(o, &fields, columns)?;
            }
            continue;
        }
        if fields.len() > columns.len() {
            return Err(bad_copy("extra data after last expected column"));
        }
        rows.push(InputRow { line: n + 1, fields });
    }
    Ok(rows)
}

pub fn missing_column(column: &str) -> SqlError {
    bad_copy(format!("missing data for column \"{column}\""))
}

fn take<'a>(data: &'a [u8], pos: &mut usize, n: usize) -> Option<&'a [u8]> {
    let b = data.get(*pos..*pos + n)?;
    *pos += n;
    Some(b)
}

fn be_i32(data: &[u8], pos: &mut usize) -> Option<i32> {
    take(data, pos, 4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Parses binary COPY input into rows of raw fields (None is NULL).
pub fn read_binary(data: &[u8], ncols: usize) -> SqlResult<Vec<Vec<Option<Vec<u8>>>>> {
    let mut pos = 0;
    if take(data, &mut pos, SIGNATURE.len()) != Some(&SIGNATURE[..]) {
        return Err(bad_copy("COPY file signature not recognized"));
    }
    let flags = be_i32(data, &mut pos).ok_or_else(|| bad_copy("invalid COPY file header (missing flags)"))?;
    if flags & !(1 << 16) & !0xffff != 0 {
        return Err(bad_copy("unrecognized critical flags in COPY file header"));
    }
    if flags & (1 << 16) != 0 {
        return Err(bad_copy("COPY file with OIDs is not supported"));
    }
    let ext = be_i32(data, &mut pos).ok_or_else(|| bad_copy("invalid COPY file header (missing length)"))?;
    let ext = usize::try_from(ext).map_err(|_| bad_copy("invalid COPY file header (wrong length)"))?;
    take(data, &mut pos, ext).ok_or_else(|| bad_copy("invalid COPY file header (wrong length)"))?;
    let mut rows = Vec::new();
    while pos < data.len() {
        let count =
            take(data, &mut pos, 2).map(|b| i16::from_be_bytes([b[0], b[1]])).ok_or_else(|| bad_copy("unexpected EOF in COPY data"))?;
        if count == -1 {
            if pos < data.len() {
                return Err(bad_copy("received copy data after EOF marker"));
            }
            break;
        }
        if usize::try_from(count).ok() != Some(ncols) {
            return Err(bad_copy(format!("row field count is {count}, expected {ncols}")));
        }
        let mut row = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let len = be_i32(data, &mut pos).ok_or_else(|| bad_copy("unexpected EOF in COPY data"))?;
            if len == -1 {
                row.push(None);
                continue;
            }
            let len = usize::try_from(len).map_err(|_| bad_copy("invalid field size"))?;
            row.push(Some(take(data, &mut pos, len).ok_or_else(|| bad_copy("unexpected EOF in COPY data"))?.to_vec()));
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(list: &[(&str, &str)], from: bool) -> Options {
        let list: Vec<CopyOption> =
            list.iter().map(|(n, v)| CopyOption { name: (*n).to_owned(), value: CopyOptionValue::Str((*v).to_owned()) }).collect();
        options(&list, from).expect("options")
    }

    #[test]
    fn text_round_trip() {
        let o = opts(&[], true);
        let row = text_row(&o, &[Some("a\tb\\c\nd".into()), None], &[]);
        assert_eq!(row, b"a\\tb\\\\c\\nd\t\\N\n");
        let rows = read_text(&o, &row, &["x".into(), "y".into()]).expect("read");
        assert_eq!(rows[0].fields, [Field::Text("a\tb\\c\nd".into()), Field::Null]);
        let rows = read_text(&o, b"\\101\\x42c\t\\N\n\\.\nignored\n", &["x".into(), "y".into()]).expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].fields[0], Field::Text("ABc".into()));
    }

    #[test]
    fn csv_quoting() {
        let o = opts(&[("format", "csv")], false);
        assert_eq!(text_row(&o, &[Some("x,\"y".into()), Some(String::new()), None], &[]), b"\"x,\"\"y\",\"\",\n");
        let o = opts(&[("format", "csv")], true);
        let rows = read_text(&o, b"\"a\nb\",\"\"\"\",\n", &["x".into(), "y".into(), "z".into()]).expect("read");
        assert_eq!(rows[0].fields, [Field::Text("a\nb".into()), Field::Text("\"".into()), Field::Null]);
        assert!(read_text(&o, b"\"open\n", &["x".into()]).is_err());
    }
}
