//! `json` values are text kept exactly as written. Operators and functions on them return
//! pieces of that text verbatim (`'{"a": [1, 2]}'::json -> 'a'` is `[1, 2]`, spacing included),
//! so this scanner records where every value starts and ends instead of building a tree of
//! normalized values the way jsonb does.

use crate::{SqlError, SqlResult, code};

/// A parsed `json` value; `text` is the exact source slice of the value.
#[derive(Clone, Debug, PartialEq)]
pub enum Node<'a> {
    /// Members in source order; duplicate keys are kept.
    Object {
        text: &'a str,
        members: Vec<(String, Node<'a>)>,
    },
    Array {
        text: &'a str,
        items: Vec<Node<'a>>,
    },
    /// The unescaped string and its quoted source.
    String {
        text: &'a str,
        value: String,
    },
    Number {
        text: &'a str,
    },
    Bool {
        text: &'a str,
        value: bool,
    },
    Null {
        text: &'a str,
    },
}

impl<'a> Node<'a> {
    pub fn text(&self) -> &'a str {
        match self {
            Node::Object { text, .. }
            | Node::Array { text, .. }
            | Node::String { text, .. }
            | Node::Number { text }
            | Node::Bool { text, .. }
            | Node::Null { text } => text,
        }
    }

    /// `json_typeof`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Node::Object { .. } => "object",
            Node::Array { .. } => "array",
            Node::String { .. } => "string",
            Node::Number { .. } => "number",
            Node::Bool { .. } => "boolean",
            Node::Null { .. } => "null",
        }
    }

    /// The member named `key` (the last one, as Postgres' json operators find it).
    pub fn field(&self, key: &str) -> Option<&Node<'a>> {
        match self {
            Node::Object { members, .. } => members.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Array element `i`; a negative index counts from the end.
    pub fn element(&self, i: i64) -> Option<&Node<'a>> {
        match self {
            Node::Array { items, .. } => {
                let len = i64::try_from(items.len()).ok()?;
                let idx = if i < 0 { len + i } else { i };
                usize::try_from(idx).ok().and_then(|i| items.get(i))
            }
            _ => None,
        }
    }

    /// `->>` text: strings unescaped, JSON null as SQL NULL, everything else as written.
    pub fn as_text(&self) -> Option<String> {
        match self {
            Node::String { value, .. } => Some(value.clone()),
            Node::Null { .. } => None,
            other => Some(other.text().to_owned()),
        }
    }
}

/// Parses a `json` value, keeping source spans.
pub fn parse(text: &str) -> SqlResult<Node<'_>> {
    let mut p = Parser { s: text, b: text.as_bytes(), i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(p.error());
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn error(&self) -> SqlError {
        SqlError::new(code::INVALID_TEXT_REPRESENTATION, "invalid input syntax for type json")
    }

    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn value(&mut self) -> SqlResult<Node<'a>> {
        let start = self.i;
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut members = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        let Node::String { value: key, .. } = self.string()? else { return Err(self.error()) };
                        self.ws();
                        if self.b.get(self.i) != Some(&b':') {
                            return Err(self.error());
                        }
                        self.i += 1;
                        self.ws();
                        let v = self.value()?;
                        members.push((key, v));
                        self.ws();
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b'}') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err(self.error()),
                        }
                    }
                }
                Ok(Node::Object { text: &self.s[start..self.i], members })
            }
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        items.push(self.value()?);
                        self.ws();
                        match self.b.get(self.i) {
                            Some(b',') => self.i += 1,
                            Some(b']') => {
                                self.i += 1;
                                break;
                            }
                            _ => return Err(self.error()),
                        }
                    }
                }
                Ok(Node::Array { text: &self.s[start..self.i], items })
            }
            Some(b'"') => self.string(),
            Some(b't') => self.word("true").map(|text| Node::Bool { text, value: true }),
            Some(b'f') => self.word("false").map(|text| Node::Bool { text, value: false }),
            Some(b'n') => self.word("null").map(|text| Node::Null { text }),
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                while self.i < self.b.len() && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    self.i += 1;
                }
                let text = &self.s[start..self.i];
                serde_json::from_str::<serde_json::Number>(text).map_err(|_| self.error())?;
                Ok(Node::Number { text })
            }
            _ => Err(self.error()),
        }
    }

    fn word(&mut self, w: &'static str) -> SqlResult<&'a str> {
        if self.s[self.i..].starts_with(w) {
            let start = self.i;
            self.i += w.len();
            Ok(&self.s[start..self.i])
        } else {
            Err(self.error())
        }
    }

    fn string(&mut self) -> SqlResult<Node<'a>> {
        let start = self.i;
        if self.b.get(self.i) != Some(&b'"') {
            return Err(self.error());
        }
        self.i += 1;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'\\' => self.i += 2,
                b'"' => {
                    self.i += 1;
                    let text = &self.s[start..self.i];
                    let value: String = serde_json::from_str(text).map_err(|_| self.error())?;
                    return Ok(Node::String { text, value });
                }
                _ => self.i += 1,
            }
        }
        Err(self.error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_source_text() {
        let doc = parse(r#"{"a": [1,  2], "b": "x\"y", "a": {"c": null}}"#).expect("parse");
        assert_eq!(doc.field("a").map(Node::text), Some(r#"{"c": null}"#));
        assert_eq!(doc.field("b").and_then(Node::as_text), Some("x\"y".to_owned()));
        let arr = parse("[true, [1]]").expect("parse");
        assert_eq!(arr.element(-1).map(Node::text), Some("[1]"));
        assert!(parse("[1,").is_err());
    }
}
