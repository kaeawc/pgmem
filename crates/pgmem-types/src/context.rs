//! The session settings that change how values are read and printed (`TimeZone`, the date
//! field order of `DateStyle`), the statement's start time for `'now'` / `'today'` input, and
//! the user-defined types visible to the transaction (enum labels, composite fields).
//!
//! Input and output functions are called from many places (casts, literal folding at bind
//! time, wire encoding) that don't carry the session, so the session installs its context for
//! the duration of each synchronous unit of work with [`enter`], and the codecs read it with
//! [`current`]. Outside any session the context is UTC with MDY order, matching pgmem's
//! defaults.

use crate::usertype::Registry;
use crate::zone::Zone;
use std::cell::RefCell;
use std::sync::Arc;

/// The order `DateStyle` gives to ambiguous numeric dates (`1/2/2024`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DateOrder {
    #[default]
    Mdy,
    Dmy,
    Ymd,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    pub zone: Zone,
    pub date_order: DateOrder,
    /// Statement start, microseconds since 2000-01-01 UTC (for `'now'`, `'today'`, ...).
    pub now: i64,
    /// User-defined types of the running transaction's catalog.
    pub types: Arc<Registry>,
    /// `bytea_output = escape` (the default is hex).
    pub bytea_escape: bool,
    pub search_path: SearchPath,
}

/// The session's `search_path` setting, as the catalog resolves names with it.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchPath {
    /// The setting's entries, unquoted (`"$user"` stays `$user`).
    pub entries: Vec<String>,
    /// The session user (`$user`).
    pub user: String,
    /// The session's temporary schema (`pg_temp_N`), once it has one.
    pub temp_schema: Option<String>,
}

impl Default for SearchPath {
    fn default() -> SearchPath {
        SearchPath { entries: vec!["$user".into(), "public".into()], user: String::new(), temp_schema: None }
    }
}

impl SearchPath {
    /// Parses a `search_path` value: comma-separated identifiers, double quotes preserving
    /// case, unquoted names folded to lower case.
    pub fn parse(setting: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut was_quoted = false;
        let mut chars = setting.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => {
                    quoted = !quoted;
                    was_quoted = true;
                }
                ',' if !quoted => {
                    push_entry(&mut out, &cur, was_quoted);
                    cur.clear();
                    was_quoted = false;
                }
                c if !quoted && c.is_whitespace() => {}
                c => cur.push(if quoted { c } else { c.to_ascii_lowercase() }),
            }
        }
        push_entry(&mut out, &cur, was_quoted);
        out
    }
}

fn push_entry(out: &mut Vec<String>, name: &str, quoted: bool) {
    if !name.is_empty() || quoted {
        out.push(name.to_owned());
    }
}

impl Default for Context {
    fn default() -> Context {
        Context {
            zone: Zone::utc(),
            date_order: DateOrder::Mdy,
            now: 0,
            types: Arc::default(),
            bytea_escape: false,
            search_path: SearchPath::default(),
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

/// The context in effect on this thread.
pub fn current() -> Context {
    CURRENT.with(|c| c.borrow().clone().unwrap_or_default())
}

/// Runs `f` with the current zone without cloning the context.
pub fn with_zone<R>(f: impl FnOnce(&Zone) -> R) -> R {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some(ctx) => f(&ctx.zone),
        None => f(&Zone::utc()),
    })
}

/// The search path in effect on this thread.
pub fn search_path() -> SearchPath {
    CURRENT.with(|c| c.borrow().as_ref().map(|ctx| ctx.search_path.clone()).unwrap_or_default())
}

/// Runs `f` with the search path replaced by `entries` (resolving a stored view's names as
/// at its creation).
pub fn with_search_path<R>(entries: &[String], f: impl FnOnce() -> R) -> R {
    let mut ctx = current();
    ctx.search_path.entries = entries.to_vec();
    let _g = enter(ctx);
    f()
}

/// Whether `bytea_output` is `escape` on this thread.
pub fn bytea_escape() -> bool {
    CURRENT.with(|c| c.borrow().as_ref().is_some_and(|ctx| ctx.bytea_escape))
}

/// Runs `f` with the current user-type registry without cloning the context.
pub fn with_types<R>(f: impl FnOnce(&Registry) -> R) -> R {
    CURRENT.with(|c| match c.borrow().as_ref() {
        Some(ctx) => f(&ctx.types),
        None => f(&Registry::default()),
    })
}

/// Restores the previous context when dropped.
#[must_use = "the context is reset when the guard is dropped"]
pub struct Guard(Option<Context>);

impl Drop for Guard {
    fn drop(&mut self) {
        let prev = self.0.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

/// Installs `ctx` on this thread until the returned guard is dropped.
pub fn enter(ctx: Context) -> Guard {
    Guard(CURRENT.with(|c| c.borrow_mut().replace(ctx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guards_nest_and_restore() {
        assert!(current().zone.is_utc());
        let ny = Zone::from_setting("America/New_York").expect("zone");
        {
            let _g = enter(Context { zone: ny.clone(), ..Context::default() });
            assert_eq!(current().zone, ny);
            {
                let _inner = enter(Context::default());
                assert!(current().zone.is_utc());
            }
            assert_eq!(current().zone, ny);
        }
        assert!(current().zone.is_utc());
    }
}
