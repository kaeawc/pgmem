//! The `pg_trgm` extension (`trgm_op.c`): trigram extraction, similarity, word similarity
//! and the `%` / `<%` / `%>` / `<->` operators.

use super::{Resolved, resolved, strict};
use crate::eval::EvalCtx;
use pgmem_types::{SqlResult, Type, Value};

/// The trigrams of one word, padded `"  word "`.
fn word_trigrams(word: &str, out: &mut Vec<String>) {
    let padded: Vec<char> = format!("  {word} ").chars().collect();
    for w in padded.windows(3) {
        out.push(w.iter().collect());
    }
}

/// Trigrams in order of appearance (words are runs of letters and digits, lowercased).
fn ordered_trigrams(s: &str) -> Vec<String> {
    let lower = s.to_lowercase();
    let mut out = Vec::new();
    for word in lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        word_trigrams(word, &mut out);
    }
    out
}

/// The sorted, unique trigram set (`generate_trgm`).
pub fn trigrams(s: &str) -> Vec<String> {
    let mut t = ordered_trigrams(s);
    t.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    t.dedup();
    t
}

fn common(a: &[String], b: &[String]) -> usize {
    a.iter().filter(|x| b.binary_search_by(|y| y.as_bytes().cmp(x.as_bytes())).is_ok()).count()
}

#[allow(clippy::cast_precision_loss)]
pub fn similarity(a: &str, b: &str) -> f32 {
    let (ta, tb) = (trigrams(a), trigrams(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let c = common(&ta, &tb);
    c as f32 / (ta.len() + tb.len() - c) as f32
}

/// The greatest similarity between `a`'s trigrams and any contiguous extent of `b`'s.
#[allow(clippy::cast_precision_loss)]
pub fn word_similarity(a: &str, b: &str) -> f32 {
    let ta = trigrams(a);
    let ob = ordered_trigrams(b);
    if ta.is_empty() || ob.is_empty() {
        return 0.0;
    }
    let mut best = 0.0f32;
    for i in 0..ob.len() {
        let mut extent: Vec<String> = Vec::new();
        for t in &ob[i..] {
            if let Err(pos) = extent.binary_search_by(|y| y.as_bytes().cmp(t.as_bytes())) {
                extent.insert(pos, t.clone());
            }
            let c = common(&ta, &extent);
            let s = c as f32 / (ta.len() + extent.len() - c) as f32;
            if s > best {
                best = s;
            }
        }
    }
    best
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn threshold(ctx: &EvalCtx, name: &str, default: f32) -> f32 {
    ctx.state.setting(ctx.env, name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    use Type::{Float4, Text};
    match (name, args.len()) {
        ("similarity", 2) => {
            resolved(strict("similarity", |a, _| Ok(Value::Float4(similarity(text(&a[0]), text(&a[1]))))), vec![Text, Text], Float4)
        }
        ("word_similarity" | "strict_word_similarity", 2) => resolved(
            strict("word_similarity", |a, _| Ok(Value::Float4(word_similarity(text(&a[0]), text(&a[1]))))),
            vec![Text, Text],
            Float4,
        ),
        ("show_trgm", 1) => resolved(
            strict("show_trgm", |a, _| Ok(Value::Array(trigrams(text(&a[0])).into_iter().map(Value::Text).collect()))),
            vec![Text],
            Type::array_of(Text),
        ),
        ("show_limit", 0) => {
            resolved(strict("show_limit", |_, c| Ok(Value::Float4(threshold(c, "pg_trgm.similarity_threshold", 0.3)))), vec![], Float4)
        }
        _ => Ok(None),
    }
}

pub fn resolve_operator(op: &str, l: &Type, r: &Type) -> SqlResult<Option<Resolved>> {
    let stringy = |t: &Type| t.is_string() || *t == Type::Unknown || *t == Type::CiText;
    if !(stringy(l) && stringy(r)) {
        return Ok(None);
    }
    use Type::{Bool, Float4, Text};
    match op {
        "%" => resolved(
            strict("%", |a, c| Ok(Value::Bool(similarity(text(&a[0]), text(&a[1])) >= threshold(c, "pg_trgm.similarity_threshold", 0.3)))),
            vec![Text, Text],
            Bool,
        ),
        "<%" => resolved(
            strict("<%", |a, c| {
                Ok(Value::Bool(word_similarity(text(&a[0]), text(&a[1])) >= threshold(c, "pg_trgm.word_similarity_threshold", 0.6)))
            }),
            vec![Text, Text],
            Bool,
        ),
        "%>" => resolved(
            strict("%>", |a, c| {
                Ok(Value::Bool(word_similarity(text(&a[1]), text(&a[0])) >= threshold(c, "pg_trgm.word_similarity_threshold", 0.6)))
            }),
            vec![Text, Text],
            Bool,
        ),
        "<->" => resolved(strict("<->", |a, _| Ok(Value::Float4(1.0 - similarity(text(&a[0]), text(&a[1]))))), vec![Text, Text], Float4),
        _ => Ok(None),
    }
}
