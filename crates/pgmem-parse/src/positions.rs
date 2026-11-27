//! Source positions: sqlparser reports `line:column` in the text it parsed, which the token
//! rewrites in `lib.rs` may have changed, while Postgres reports the 1-based character offset
//! into the query string the client sent.

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};
use std::collections::HashMap;

/// Where the tokens of the text sqlparser parsed sit in the query string as sent.
#[derive(Debug, Default)]
pub(crate) struct Positions {
    /// The character offset at which each line of the parsed text starts.
    parsed_lines: Vec<u32>,
    /// Parsed token start (0-based character offset) → the same token's start in the original.
    /// `None`: the texts are the same.
    moved: Option<HashMap<u32, u32>>,
    /// The position just past the last character of the original.
    end: u32,
}

impl Positions {
    /// `parsed` is what the parser saw, derived from `original` by rewrites.
    pub(crate) fn new(original: &str, parsed: &str) -> Positions {
        let end = count(original).saturating_add(1);
        let parsed_lines = line_starts(parsed);
        if original == parsed {
            return Positions { parsed_lines, moved: None, end };
        }
        Positions { parsed_lines, moved: Some(align(original, parsed).unwrap_or_default()), end }
    }

    /// The 1-based character offset in the original of the token starting at `at` in the
    /// parsed text; `None` for a token the rewrites introduced.
    pub(crate) fn at(&self, at: Location) -> Option<u32> {
        let line = usize::try_from(at.line).ok()?.checked_sub(1)?;
        let col = u32::try_from(at.column).ok()?.checked_sub(1)?;
        let offset = self.parsed_lines.get(line)?.saturating_add(col);
        match &self.moved {
            None => Some(offset.saturating_add(1)),
            Some(moved) => moved.get(&offset).map(|o| o.saturating_add(1)),
        }
    }

    /// [`Positions::at`] for a token starting at the 0-based character `offset` of the parsed text.
    pub(crate) fn at_offset(&self, offset: u32) -> Option<u32> {
        match &self.moved {
            None => Some(offset.saturating_add(1)),
            Some(moved) => moved.get(&offset).map(|o| o.saturating_add(1)),
        }
    }

    /// The position at the end of the query string (`syntax error at end of input`).
    pub(crate) fn end(&self) -> u32 {
        self.end
    }
}

fn count(s: &str) -> u32 {
    u32::try_from(s.chars().count()).unwrap_or(u32::MAX)
}

/// The character offset at which each line of `sql` starts (line 1 at index 0).
pub(crate) fn line_starts(sql: &str) -> Vec<u32> {
    let mut starts = vec![0u32];
    for (n, c) in sql.chars().enumerate() {
        if c == '\n' {
            starts.push(u32::try_from(n + 1).unwrap_or(u32::MAX));
        }
    }
    starts
}

/// A significant (non-whitespace, non-EOF) token and where it starts, as a 0-based character
/// offset.
struct Tok {
    token: Token,
    start: u32,
}

fn tokens(sql: &str) -> Option<Vec<Tok>> {
    let lines = line_starts(sql);
    let all = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location().ok()?;
    Some(
        all.into_iter()
            .filter(|t: &TokenWithSpan| !matches!(t.token, Token::Whitespace(_) | Token::EOF))
            .filter_map(|t| {
                let line = usize::try_from(t.span.start.line).ok()?.checked_sub(1)?;
                let col = u32::try_from(t.span.start.column).ok()?.checked_sub(1)?;
                Some(Tok { token: t.token, start: lines.get(line)?.saturating_add(col) })
            })
            .collect(),
    )
}

/// How far apart (in tokens) two texts may drift before they are considered unrelated.
const WINDOW: usize = 64;
/// How many following tokens must agree for two texts to count as back in step.
const AGREE: usize = 2;

/// Pairs up the tokens `rewritten` kept from `original` (rewrites drop, replace and insert a
/// few tokens at a time): rewritten token start → original token start.
fn align(original: &str, rewritten: &str) -> Option<HashMap<u32, u32>> {
    let (a, b) = (tokens(original)?, tokens(rewritten)?);
    let mut out = HashMap::with_capacity(b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].token == b[j].token {
            out.insert(b[j].start, a[i].start);
            i += 1;
            j += 1;
            continue;
        }
        // Out of step: the nearest place both texts agree again (fewest tokens skipped).
        let agree = |di: usize, dj: usize| {
            (0..AGREE).all(|k| match (a.get(i + di + k), b.get(j + dj + k)) {
                (Some(x), Some(y)) => x.token == y.token,
                (None, None) => true,
                _ => false,
            }) && a.get(i + di).is_some()
        };
        let found = (1..=2 * WINDOW)
            .flat_map(|total| (0..=total).map(move |di| (di, total - di)))
            .filter(|&(di, dj)| di <= WINDOW && dj <= WINDOW)
            .find(|&(di, dj)| agree(di, dj));
        match found {
            Some((di, dj)) => {
                i += di;
                j += dj;
            }
            None => break,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(original: &str, parsed: &str, line: u64, column: u64) -> Option<u32> {
        Positions::new(original, parsed).at(Location { line, column })
    }

    #[test]
    fn identical_texts_map_line_and_column_to_character_offsets() {
        assert_eq!(pos("SELECT 1", "SELECT 1", 1, 8), Some(8));
        assert_eq!(pos("SELECT 'é'\n, x", "SELECT 'é'\n, x", 2, 3), Some(14));
    }

    #[test]
    fn tokens_kept_by_a_rewrite_map_back() {
        // `f(a, VARIADIC arr)` is parsed as `f(a, __pgmem_variadic(arr))`.
        let original = "SELECT f(a, VARIADIC arr), nope FROM t";
        let parsed = "SELECT f(a, __pgmem_variadic(arr)), nope FROM t";
        let at = |text: &str, word: &str| {
            let col = text.find(word).expect("word in text") as u64 + 1;
            Location { line: 1, column: col }
        };
        let p = Positions::new(original, parsed);
        assert_eq!(p.at(at(parsed, "nope")), Some(28));
        assert_eq!(p.at(at(parsed, "arr")), Some(22));
        assert_eq!(p.at(at(parsed, "FROM")), Some(33));
        // Introduced by the rewrite: nowhere in the original.
        assert_eq!(p.at(at(parsed, "__pgmem_variadic")), None);
    }
}
