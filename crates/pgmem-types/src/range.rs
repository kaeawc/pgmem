//! Range and multirange types (Postgres' `rangetypes.c` / `multirangetypes.c`): the six
//! builtin ranges over int4, int8, numeric, date, timestamp and timestamptz. Discrete ranges
//! (int4, int8, date) are canonicalized to `[lower, upper)`.

use crate::{SqlError, SqlResult, Type, Value, code, compare_values};
use std::cmp::Ordering;

/// A range value. Infinite bounds are `None` (and never inclusive).
#[derive(Clone, Debug, PartialEq)]
pub struct Range {
    pub empty: bool,
    pub lower: Option<Value>,
    pub upper: Option<Value>,
    pub lower_inc: bool,
    pub upper_inc: bool,
    /// The subtype (`int4` for `int4range`).
    pub sub: Type,
}

impl Range {
    pub fn empty(sub: &Type) -> Range {
        Range { empty: true, lower: None, upper: None, lower_inc: false, upper_inc: false, sub: sub.clone() }
    }
}

/// The builtin range types: (subtype, range OID, multirange OID, range array OID, multirange
/// array OID, range name, multirange name).
pub const RANGES: &[(Type, u32, u32, u32, u32, &str, &str)] = &[
    (Type::Int4, 3904, 4451, 3905, 6150, "int4range", "int4multirange"),
    (Type::Numeric, 3906, 4532, 3907, 6151, "numrange", "nummultirange"),
    (Type::Timestamp, 3908, 4533, 3909, 6152, "tsrange", "tsmultirange"),
    (Type::Timestamptz, 3910, 4534, 3911, 6153, "tstzrange", "tstzmultirange"),
    (Type::Date, 3912, 4535, 3913, 6155, "daterange", "datemultirange"),
    (Type::Int8, 3926, 4536, 3927, 6157, "int8range", "int8multirange"),
];

pub fn entry(subtype: &Type) -> Option<&'static (Type, u32, u32, u32, u32, &'static str, &'static str)> {
    RANGES.iter().find(|e| e.0 == *subtype)
}

fn is_discrete(subtype: &Type) -> bool {
    matches!(subtype, Type::Int4 | Type::Int8 | Type::Date)
}

/// The next value of a discrete subtype (`canonical` adds one).
fn successor(v: &Value) -> SqlResult<Value> {
    let overflow = |t: &str| SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("{t} out of range"));
    Ok(match v {
        Value::Int4(n) => Value::Int4(n.checked_add(1).ok_or_else(|| overflow("integer"))?),
        Value::Int8(n) => Value::Int8(n.checked_add(1).ok_or_else(|| overflow("bigint"))?),
        Value::Date(d) => Value::Date(d.checked_add(1).ok_or_else(|| overflow("date"))?),
        other => other.clone(),
    })
}

/// `daterange_canonical` leaves `infinity` / `-infinity` bounds as they are.
fn steps(v: &Value) -> bool {
    !matches!(v, Value::Date(d) if *d == crate::datetime::DATE_INFINITY || *d == crate::datetime::DATE_NEG_INFINITY)
}

/// Builds a range as `make_range` does: checks the bounds' order, canonicalizes discrete
/// ranges and collapses empty ones.
pub fn make(subtype: &Type, lower: Option<Value>, upper: Option<Value>, lower_inc: bool, upper_inc: bool) -> SqlResult<Range> {
    let lower = lower.filter(|v| !v.is_null());
    let upper = upper.filter(|v| !v.is_null());
    let (mut lower_inc, mut upper_inc) = (lower_inc && lower.is_some(), upper_inc && upper.is_some());
    if let (Some(l), Some(u)) = (&lower, &upper) {
        match compare_values(l, u)? {
            Ordering::Greater => {
                return Err(SqlError::new(code::DATA_EXCEPTION, "range lower bound must be less than or equal to range upper bound"));
            }
            Ordering::Equal if !(lower_inc && upper_inc) => return Ok(Range::empty(subtype)),
            _ => {}
        }
    }
    let (mut lower, mut upper) = (lower, upper);
    if is_discrete(subtype) {
        if let Some(l) = &lower
            && !lower_inc
            && steps(l)
        {
            lower = Some(successor(l)?);
            lower_inc = true;
        }
        if let Some(u) = &upper
            && upper_inc
            && steps(u)
        {
            upper = Some(successor(u)?);
            upper_inc = false;
        }
        if let (Some(l), Some(u)) = (&lower, &upper)
            && match compare_values(l, u)? {
                Ordering::Less => false,
                Ordering::Equal => !(lower_inc && upper_inc),
                Ordering::Greater => true,
            }
        {
            return Ok(Range::empty(subtype));
        }
    }
    Ok(Range { empty: false, lower, upper, lower_inc, upper_inc, sub: subtype.clone() })
}

// ---------------------------------------------------------------------------------------------
// Text form
// ---------------------------------------------------------------------------------------------

fn malformed(s: &str, detail: &str) -> SqlError {
    SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("malformed range literal: \"{s}\"")).with_detail(detail.to_owned())
}

/// Parses one bound: quoted (`"..."` with `\` escapes and `""`), bare, or empty (infinite).
fn parse_bound(chars: &[char], i: &mut usize) -> (Option<String>, bool) {
    let mut out = String::new();
    let mut quoted = false;
    let mut any = false;
    while *i < chars.len() {
        let c = chars[*i];
        if !quoted && matches!(c, ',' | ')' | ']') {
            break;
        }
        any = true;
        match c {
            '\\' if *i + 1 < chars.len() => {
                *i += 1;
                out.push(chars[*i]);
            }
            '"' if quoted && chars.get(*i + 1) == Some(&'"') => {
                *i += 1;
                out.push('"');
            }
            '"' => quoted = !quoted,
            _ => out.push(c),
        }
        *i += 1;
    }
    (any.then_some(out), quoted)
}

/// `range_in`: `empty`, or `[lower,upper)` with either side omitted for infinity.
pub fn parse(s: &str, subtype: &Type, decode: &dyn Fn(&str) -> SqlResult<Value>) -> SqlResult<Range> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("empty") {
        return Ok(Range::empty(subtype));
    }
    let chars: Vec<char> = t.chars().collect();
    let lower_inc = match chars.first() {
        Some('[') => true,
        Some('(') => false,
        _ => return Err(malformed(s, "Missing left parenthesis or bracket.")),
    };
    let mut i = 1;
    let (lower, q1) = parse_bound(&chars, &mut i);
    if q1 {
        return Err(malformed(s, "Unexpected end of input."));
    }
    if chars.get(i) != Some(&',') {
        return Err(malformed(s, "Missing comma after lower bound."));
    }
    i += 1;
    let (upper, q2) = parse_bound(&chars, &mut i);
    if q2 {
        return Err(malformed(s, "Unexpected end of input."));
    }
    let upper_inc = match chars.get(i) {
        Some(']') => true,
        Some(')') => false,
        Some(',') => return Err(malformed(s, "Too many commas.")),
        _ => return Err(malformed(s, "Unexpected end of input.")),
    };
    if i + 1 != chars.len() {
        return Err(malformed(s, "Junk after right parenthesis or bracket."));
    }
    let lower = lower.map(|b| decode(&b)).transpose()?;
    let upper = upper.map(|b| decode(&b)).transpose()?;
    make(subtype, lower, upper, lower_inc, upper_inc)
}

/// Quotes a bound's text when `range_out` would (`range_bound_escape`).
fn quote_bound(s: &str) -> String {
    let needs = s.is_empty() || s.chars().any(|c| matches!(c, '"' | '\\' | '(' | ')' | '[' | ']' | ',') || c.is_whitespace());
    if !needs {
        return s.to_owned();
    }
    let mut out = String::from('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `range_out`.
pub fn format(r: &Range, encode: &dyn Fn(&Value) -> SqlResult<String>) -> SqlResult<String> {
    if r.empty {
        return Ok("empty".into());
    }
    let mut out = String::new();
    out.push(if r.lower_inc { '[' } else { '(' });
    if let Some(l) = &r.lower {
        out.push_str(&quote_bound(&encode(l)?));
    }
    out.push(',');
    if let Some(u) = &r.upper {
        out.push_str(&quote_bound(&encode(u)?));
    }
    out.push(if r.upper_inc { ']' } else { ')' });
    Ok(out)
}

/// `multirange_in`: `{range, range, ...}`.
pub fn parse_multi(s: &str, subtype: &Type, decode: &dyn Fn(&str) -> SqlResult<Value>) -> SqlResult<Vec<Range>> {
    let bad = |d: &str| {
        SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("malformed multirange literal: \"{s}\"")).with_detail(d.to_owned())
    };
    let t = s.trim();
    let inner = t.strip_prefix('{').and_then(|x| x.strip_suffix('}')).ok_or_else(|| bad("Missing left brace."))?;
    let mut ranges = Vec::new();
    let chars: Vec<char> = inner.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        if inner[start..].to_ascii_lowercase().starts_with("empty") {
            i += 5;
            continue;
        }
        let mut quoted = false;
        while i < chars.len() {
            let c = chars[i];
            if c == '"' {
                quoted = !quoted;
            } else if c == '\\' {
                i += 1;
            } else if !quoted && matches!(c, ')' | ']') {
                i += 1;
                break;
            }
            i += 1;
        }
        let piece: String = chars[start..i.min(chars.len())].iter().collect();
        ranges.push(parse(&piece, subtype, decode)?);
    }
    normalize(ranges)
}

pub fn format_multi(rs: &[Range], encode: &dyn Fn(&Value) -> SqlResult<String>) -> SqlResult<String> {
    let parts = rs.iter().map(|r| format(r, encode)).collect::<SqlResult<Vec<_>>>()?;
    Ok(format!("{{{}}}", parts.join(",")))
}

// ---------------------------------------------------------------------------------------------
// Bound comparison (`range_cmp_bounds`)
// ---------------------------------------------------------------------------------------------

/// A bound for comparisons: value (None = infinite), inclusive, is-lower.
#[derive(Clone, Copy)]
struct Bound<'a> {
    val: Option<&'a Value>,
    inc: bool,
    lower: bool,
}

fn lower_of(r: &Range) -> Bound<'_> {
    Bound { val: r.lower.as_ref(), inc: r.lower_inc, lower: true }
}

fn upper_of(r: &Range) -> Bound<'_> {
    Bound { val: r.upper.as_ref(), inc: r.upper_inc, lower: false }
}

fn cmp_bounds(a: Bound, b: Bound) -> SqlResult<Ordering> {
    let side = |lower: bool| if lower { Ordering::Less } else { Ordering::Greater };
    let (x, y) = match (a.val, b.val) {
        (None, None) => return Ok(if a.lower == b.lower { Ordering::Equal } else { side(a.lower) }),
        (None, Some(_)) => return Ok(side(a.lower)),
        (Some(_), None) => return Ok(side(b.lower).reverse()),
        (Some(x), Some(y)) => (x, y),
    };
    let c = compare_values(x, y)?;
    if c != Ordering::Equal {
        return Ok(c);
    }
    Ok(match (a.inc, b.inc) {
        (false, false) if a.lower == b.lower => Ordering::Equal,
        (false, false) | (false, true) => side(a.lower).reverse(),
        (true, false) => side(b.lower),
        (true, true) => Ordering::Equal,
    })
}

/// `range_cmp`: empty sorts first, then by lower bound, then upper bound.
pub fn compare(a: &Range, b: &Range) -> SqlResult<Ordering> {
    match (a.empty, b.empty) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Less),
        (false, true) => return Ok(Ordering::Greater),
        _ => {}
    }
    let c = cmp_bounds(lower_of(a), lower_of(b))?;
    if c != Ordering::Equal {
        return Ok(c);
    }
    cmp_bounds(upper_of(a), upper_of(b))
}

pub fn compare_multi(a: &[Range], b: &[Range]) -> SqlResult<Ordering> {
    for (x, y) in a.iter().zip(b) {
        let c = compare(x, y)?;
        if c != Ordering::Equal {
            return Ok(c);
        }
    }
    Ok(a.len().cmp(&b.len()))
}

// ---------------------------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------------------------

pub fn contains_elem(r: &Range, v: &Value) -> SqlResult<bool> {
    if r.empty {
        return Ok(false);
    }
    let e = Bound { val: Some(v), inc: true, lower: true };
    let eu = Bound { val: Some(v), inc: true, lower: false };
    Ok(cmp_bounds(lower_of(r), e)? != Ordering::Greater && cmp_bounds(eu, upper_of(r))? != Ordering::Greater)
}

pub fn contains(a: &Range, b: &Range) -> SqlResult<bool> {
    if b.empty {
        return Ok(true);
    }
    if a.empty {
        return Ok(false);
    }
    Ok(cmp_bounds(lower_of(a), lower_of(b))? != Ordering::Greater && cmp_bounds(upper_of(a), upper_of(b))? != Ordering::Less)
}

pub fn overlaps(a: &Range, b: &Range) -> SqlResult<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    Ok(cmp_bounds(lower_of(a), upper_of(b))? != Ordering::Greater && cmp_bounds(lower_of(b), upper_of(a))? != Ordering::Greater)
}

/// `<<`: every point of a is below every point of b.
pub fn before(a: &Range, b: &Range) -> SqlResult<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    Ok(cmp_bounds(upper_of(a), lower_of(b))? == Ordering::Less)
}

/// `&<`: a does not extend to the right of b.
pub fn overleft(a: &Range, b: &Range) -> SqlResult<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    Ok(cmp_bounds(upper_of(a), upper_of(b))? != Ordering::Greater)
}

/// `&>`: a does not extend to the left of b.
pub fn overright(a: &Range, b: &Range) -> SqlResult<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    Ok(cmp_bounds(lower_of(a), lower_of(b))? != Ordering::Less)
}

/// Whether bound `upper` touches bound `lower` with no gap (`bounds_adjacent`; discrete
/// ranges are canonical, so equal values with one side exclusive is the only case).
fn bounds_adjacent(upper: Bound, lower: Bound) -> SqlResult<bool> {
    let (Some(u), Some(l)) = (upper.val, lower.val) else { return Ok(false) };
    Ok(compare_values(u, l)? == Ordering::Equal && upper.inc != lower.inc)
}

/// `-|-`.
pub fn adjacent(a: &Range, b: &Range) -> SqlResult<bool> {
    if a.empty || b.empty {
        return Ok(false);
    }
    Ok(bounds_adjacent(upper_of(a), lower_of(b))? || bounds_adjacent(upper_of(b), lower_of(a))?)
}

fn min_lower<'a>(a: &'a Range, b: &'a Range) -> SqlResult<&'a Range> {
    Ok(if cmp_bounds(lower_of(a), lower_of(b))? != Ordering::Greater { a } else { b })
}

fn max_upper<'a>(a: &'a Range, b: &'a Range) -> SqlResult<&'a Range> {
    Ok(if cmp_bounds(upper_of(a), upper_of(b))? != Ordering::Less { a } else { b })
}

/// `+` (`range_union`); `strict` errors when the ranges neither overlap nor touch
/// (`range_merge` passes false and spans the gap).
pub fn union(a: &Range, b: &Range, strict: bool) -> SqlResult<Range> {
    if a.empty {
        return Ok(b.clone());
    }
    if b.empty {
        return Ok(a.clone());
    }
    if strict && !overlaps(a, b)? && !adjacent(a, b)? {
        return Err(SqlError::new(code::DATA_EXCEPTION, "result of range union would not be contiguous"));
    }
    let l = min_lower(a, b)?;
    let u = max_upper(a, b)?;
    Ok(Range {
        empty: false,
        lower: l.lower.clone(),
        lower_inc: l.lower_inc,
        upper: u.upper.clone(),
        upper_inc: u.upper_inc,
        sub: a.sub.clone(),
    })
}

/// `*`.
pub fn intersect(a: &Range, b: &Range) -> SqlResult<Range> {
    if !overlaps(a, b)? {
        return Ok(Range::empty(&a.sub));
    }
    let l = if cmp_bounds(lower_of(a), lower_of(b))? == Ordering::Greater { a } else { b };
    let u = if cmp_bounds(upper_of(a), upper_of(b))? == Ordering::Less { a } else { b };
    Ok(Range {
        empty: false,
        lower: l.lower.clone(),
        lower_inc: l.lower_inc,
        upper: u.upper.clone(),
        upper_inc: u.upper_inc,
        sub: a.sub.clone(),
    })
}

/// `-` (`range_minus`).
pub fn minus(a: &Range, b: &Range) -> SqlResult<Range> {
    if a.empty || b.empty || !overlaps(a, b)? {
        return Ok(a.clone());
    }
    let cmp_l1l2 = cmp_bounds(lower_of(a), lower_of(b))?;
    let cmp_u1u2 = cmp_bounds(upper_of(a), upper_of(b))?;
    if cmp_l1l2 == Ordering::Less && cmp_u1u2 == Ordering::Greater {
        return Err(SqlError::new(code::DATA_EXCEPTION, "result of range difference would not be contiguous"));
    }
    if cmp_l1l2 != Ordering::Less && cmp_u1u2 != Ordering::Greater {
        return Ok(Range::empty(&a.sub));
    }
    if cmp_l1l2 == Ordering::Less {
        // a's left part, up to b's lower bound.
        return Ok(Range {
            empty: false,
            lower: a.lower.clone(),
            lower_inc: a.lower_inc,
            upper: b.lower.clone(),
            upper_inc: !b.lower_inc,
            sub: a.sub.clone(),
        });
    }
    Ok(Range {
        empty: false,
        lower: b.upper.clone(),
        lower_inc: !b.upper_inc,
        upper: a.upper.clone(),
        upper_inc: a.upper_inc,
        sub: a.sub.clone(),
    })
}

/// A multirange's canonical form: sorted, empty ranges dropped, overlapping or touching
/// ranges merged.
pub fn normalize(mut ranges: Vec<Range>) -> SqlResult<Vec<Range>> {
    ranges.retain(|r| !r.empty);
    let mut err = None;
    ranges.sort_by(|a, b| {
        compare(a, b).unwrap_or_else(|e| {
            err = Some(e);
            Ordering::Equal
        })
    });
    if let Some(e) = err {
        return Err(e);
    }
    let mut out: Vec<Range> = Vec::new();
    for r in ranges {
        if let Some(last) = out.last_mut()
            && (overlaps(last, &r)? || adjacent(last, &r)?)
        {
            *last = union(last, &r, false)?;
            continue;
        }
        out.push(r);
    }
    Ok(out)
}

/// Re-canonicalizes a range computed from canonical inputs (bounds flipped by `-`).
pub fn canonical(subtype: &Type, r: Range) -> SqlResult<Range> {
    if r.empty {
        return Ok(r);
    }
    make(subtype, r.lower, r.upper, r.lower_inc, r.upper_inc)
}

/// The range value inside `v`.
pub fn of(v: &Value) -> SqlResult<&Range> {
    match v {
        Value::Range(r) => Ok(r),
        _ => Err(SqlError::internal("range value expected")),
    }
}

// ---------------------------------------------------------------------------------------------
// Multirange operators (`multirangetypes.c`)
// ---------------------------------------------------------------------------------------------

/// `a - b` as up to two pieces (`range_minus_internal` without the contiguity check).
fn minus_pieces(a: &Range, b: &Range) -> SqlResult<Vec<Range>> {
    if a.empty || b.empty || !overlaps(a, b)? {
        return Ok(vec![a.clone()]);
    }
    let mut out = Vec::new();
    if cmp_bounds(lower_of(a), lower_of(b))? == Ordering::Less {
        let left = Range { upper: b.lower.clone(), upper_inc: !b.lower_inc, ..a.clone() };
        out.push(canonical(&a.sub, left)?);
    }
    if cmp_bounds(upper_of(a), upper_of(b))? == Ordering::Greater {
        let right = Range { lower: b.upper.clone(), lower_inc: !b.upper_inc, ..a.clone() };
        out.push(canonical(&a.sub, right)?);
    }
    Ok(out)
}

/// `multirange + multirange`.
pub fn multi_union(a: &[Range], b: &[Range]) -> SqlResult<Vec<Range>> {
    normalize(a.iter().chain(b).cloned().collect())
}

/// `multirange * multirange`.
pub fn multi_intersect(a: &[Range], b: &[Range]) -> SqlResult<Vec<Range>> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            out.push(intersect(x, y)?);
        }
    }
    normalize(out)
}

/// `multirange - multirange`.
pub fn multi_minus(a: &[Range], b: &[Range]) -> SqlResult<Vec<Range>> {
    let mut out = Vec::new();
    for x in a {
        let mut pieces = vec![x.clone()];
        for y in b {
            let mut next = Vec::new();
            for p in &pieces {
                next.extend(minus_pieces(p, y)?);
            }
            pieces = next;
        }
        out.extend(pieces);
    }
    normalize(out)
}

/// The smallest range containing every range of a multirange (empty for an empty one).
pub fn hull(rs: &[Range], sub: &Type) -> SqlResult<Range> {
    match (rs.first(), rs.last()) {
        (Some(f), Some(l)) => union(f, l, false),
        _ => Ok(Range::empty(sub)),
    }
}

/// Whether every range of `b` lies inside one range of `a` (`@>` between multiranges).
pub fn multi_contains(a: &[Range], b: &[Range]) -> SqlResult<bool> {
    for y in b {
        let mut inside = false;
        for x in a {
            if contains(x, y)? {
                inside = true;
                break;
            }
        }
        if !inside {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether any range of `a` overlaps any range of `b`.
pub fn multi_overlaps(a: &[Range], b: &[Range]) -> SqlResult<bool> {
    for x in a {
        for y in b {
            if overlaps(x, y)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
