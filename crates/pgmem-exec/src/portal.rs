//! Session-level named objects: prepared statements (SQL `PREPARE` and protocol `Parse`
//! share one namespace, as in Postgres' `prepare.c`) and cursors (`DECLARE`), whose FETCH /
//! MOVE positioning follows `pquery.c` (`DoPortalRunFetch`, `PortalRunSelect`).

use crate::plan::{OutCol, Row};
use pgmem_ir::{FetchDirection, Statement};
use pgmem_types::{SqlError, SqlResult, Type, code};

/// A prepared statement.
#[derive(Clone, Debug)]
pub struct PreparedStatement {
    pub stmt: Statement,
    pub param_types: Vec<Type>,
    /// Result columns; None for statements that return no rows.
    pub columns: Option<Vec<OutCol>>,
    /// The query string it was prepared from.
    pub text: String,
    pub from_sql: bool,
    pub prepare_time: i64,
}

/// A cursor row's table OID and `ctid` (what `WHERE CURRENT OF` compares with).
pub type RowIdentity = (u32, pgmem_types::Value);

/// What `pg_cursors` shows about a cursor.
#[derive(Clone, Debug)]
pub struct CursorInfo {
    pub name: String,
    pub text: String,
    pub hold: bool,
    pub binary: bool,
    pub scroll: bool,
    pub creation_time: i64,
    /// The table OID and `ctid` of the row the cursor is on (`WHERE CURRENT OF`).
    pub current: Option<RowIdentity>,
    /// `WHERE CURRENT OF` can name it: a FOR UPDATE / SHARE cursor, or a plain scan of one
    /// table (no ORDER BY / LIMIT).
    pub updatable: bool,
}

/// An open cursor over its (materialized) result.
#[derive(Debug)]
pub struct Cursor {
    pub info: CursorInfo,
    pub columns: Vec<OutCol>,
    rows: Vec<Row>,
    /// Per row, its table OID and `ctid` (a cursor over a single-table SELECT).
    row_ids: Vec<Option<RowIdentity>>,
    /// 0 = before the first row, k = on row k, `rows.len() + 1` = after the last row.
    pos: usize,
    /// Declared in the current transaction (a rollback closes it even WITH HOLD).
    pub in_txn: bool,
    /// Number of savepoints open when it was declared (ROLLBACK TO an earlier one closes it).
    pub savepoint_depth: usize,
}

/// Rows a FETCH returns (or a MOVE skips) and the count its command tag reports.
pub struct Fetched {
    pub rows: Vec<Row>,
    pub count: u64,
}

impl Cursor {
    pub fn new(info: CursorInfo, columns: Vec<OutCol>, rows: Vec<Row>, savepoint_depth: usize) -> Cursor {
        Cursor { info, columns, rows, row_ids: Vec::new(), pos: 0, in_txn: true, savepoint_depth }
    }

    /// Records each row's table OID and `ctid`.
    pub fn with_row_ids(mut self, ids: Vec<Option<RowIdentity>>) -> Cursor {
        self.row_ids = ids;
        self
    }

    /// The table OID and `ctid` of the current row.
    pub fn current_row_id(&self) -> Option<RowIdentity> {
        if self.pos == 0 || self.pos > self.rows.len() {
            return None;
        }
        self.row_ids.get(self.pos - 1).cloned().flatten()
    }

    fn end(&self) -> usize {
        self.rows.len() + 1
    }

    fn forward_only() -> SqlError {
        SqlError::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "cursor can only scan forward")
            .with_hint("Declare it with SCROLL option to enable backward scan.")
    }

    /// `DoPortalRewind`.
    fn rewind(&mut self) -> SqlResult<()> {
        if !self.info.scroll && self.pos != 0 {
            return Err(Self::forward_only());
        }
        self.pos = 0;
        Ok(())
    }

    /// `PortalRunSelect`: moves `count` rows (None = all) in one direction, collecting them
    /// into `out` when given. Returns the number of rows processed.
    fn run(&mut self, forward: bool, count: Option<u64>, out: Option<&mut Vec<Row>>) -> SqlResult<u64> {
        if !forward && !self.info.scroll {
            return Err(Self::forward_only());
        }
        let n = self.rows.len();
        let at_limit = if forward { self.pos == self.end() } else { self.pos == 0 };
        if at_limit || count == Some(0) {
            return Ok(0);
        }
        let avail = if forward { n - self.pos } else { self.pos - 1 };
        let take = count.map_or(avail, |c| usize::try_from(c).unwrap_or(usize::MAX).min(avail));
        if let Some(out) = out {
            if forward {
                out.extend(self.rows[self.pos..self.pos + take].iter().cloned());
            } else {
                out.extend(self.rows[self.pos - 1 - take..self.pos - 1].iter().rev().cloned());
            }
        }
        let short = count.is_none_or(|c| (take as u64) < c);
        self.pos = match (forward, short) {
            (true, true) => self.end(),
            (true, false) => self.pos + take,
            (false, true) => 0,
            (false, false) => self.pos - take,
        };
        Ok(take as u64)
    }

    /// Postgres' `portalPos`: the current row number, `rows.len()` once past the end.
    fn portal_pos(&self) -> usize {
        self.pos.min(self.rows.len())
    }

    /// FETCH (`is_move` false) or MOVE.
    pub fn fetch(&mut self, direction: FetchDirection, is_move: bool) -> SqlResult<Fetched> {
        use FetchDirection as D;
        let mut rows = Vec::new();
        let out = if is_move { None } else { Some(&mut rows) };
        let abs = |c: i64| c.unsigned_abs();
        let count = match direction {
            D::Forward(Some(c)) if c < 0 => self.run(false, Some(abs(c)), out)?,
            D::Backward(Some(c)) if c < 0 => self.run(true, Some(abs(c)), out)?,
            D::Absolute(k) if k > 0 => {
                let k = usize::try_from(k).unwrap_or(usize::MAX);
                let pos = self.portal_pos();
                if pos >= k {
                    self.rewind()?;
                    if k > 1 {
                        self.run(true, Some(k as u64 - 1), None)?;
                    }
                } else if pos < k - 1 {
                    self.run(true, Some((k - 1 - pos) as u64), None)?;
                }
                self.run(true, Some(1), out)?
            }
            D::Absolute(k) if k < 0 => {
                self.run(true, None, None)?;
                if k < -1 {
                    self.run(false, Some(abs(k) - 1), None)?;
                }
                self.run(false, Some(1), out)?
            }
            D::Absolute(_) => {
                self.rewind()?;
                0
            }
            D::Relative(k) if k > 0 => {
                if k > 1 {
                    self.run(true, Some(abs(k) - 1), None)?;
                }
                self.run(true, Some(1), out)?
            }
            D::Relative(k) if k < 0 => {
                if k < -1 {
                    self.run(false, Some(abs(k) - 1), None)?;
                }
                self.run(false, Some(1), out)?
            }
            D::Relative(_) | D::Forward(Some(0)) => self.zero(true, is_move, out)?,
            D::Backward(Some(0)) => self.zero(false, is_move, out)?,
            D::Backward(None) if is_move => {
                // MOVE BACKWARD ALL is a rewind.
                let mut result = self.portal_pos() as u64;
                if result > 0 && self.pos != self.end() {
                    result -= 1;
                }
                self.rewind()?;
                result
            }
            D::Forward(c) => self.run(true, c.map(abs), out)?,
            D::Backward(c) => self.run(false, c.map(abs), out)?,
        };
        Ok(Fetched { rows, count })
    }

    /// A zero count re-fetches the current row, if any (MOVE 0 just reports whether there
    /// is one).
    fn zero(&mut self, forward: bool, is_move: bool, out: Option<&mut Vec<Row>>) -> SqlResult<u64> {
        let on_row = self.pos != 0 && self.pos != self.end();
        if is_move {
            return Ok(u64::from(on_row));
        }
        if on_row {
            self.run(false, Some(1), None)?;
            return self.run(true, Some(1), out);
        }
        self.run(forward, Some(0), out)
    }
}

/// `pg_notify` / NOTIFY argument checks (`async.c` `Async_Notify`).
pub fn check_notify(channel: &str, payload: &str) -> SqlResult<()> {
    if channel.is_empty() {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "channel name cannot be empty"));
    }
    if channel.len() >= 64 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "channel name too long"));
    }
    if payload.len() >= 8000 {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "payload string too long"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgmem_types::Value;

    fn cursor(n: i32, scroll: bool) -> Cursor {
        let info = CursorInfo {
            name: "c".into(),
            text: String::new(),
            hold: false,
            binary: false,
            scroll,
            creation_time: 0,
            current: None,
            updatable: false,
        };
        Cursor::new(info, Vec::new(), (1..=n).map(|i| vec![Value::Int4(i)]).collect(), 0)
    }

    fn ids(f: &Fetched) -> Vec<i32> {
        f.rows.iter().map(|r| if let Value::Int4(i) = r[0] { i } else { 0 }).collect()
    }

    #[test]
    fn scroll_cursor_moves_like_postgres() {
        use FetchDirection as D;
        let mut c = cursor(5, true);
        assert_eq!(c.fetch(D::Forward(Some(2)), true).expect("fetch").count, 2);
        assert_eq!(c.fetch(D::Forward(None), true).expect("fetch").count, 3);
        assert_eq!(ids(&c.fetch(D::Backward(Some(1)), false).expect("fetch")), [5]);
        assert_eq!(ids(&c.fetch(D::Relative(0), false).expect("fetch")), [5]);
        assert_eq!(ids(&c.fetch(D::Forward(Some(0)), false).expect("fetch")), [5]);
        assert!(ids(&c.fetch(D::Absolute(0), false).expect("fetch")).is_empty());
        assert_eq!(ids(&c.fetch(D::Absolute(-2), false).expect("fetch")), [4]);
        assert_eq!(ids(&c.fetch(D::Forward(Some(-1)), false).expect("fetch")), [3]);
        assert_eq!(ids(&c.fetch(D::Backward(None), false).expect("fetch")), [2, 1]);
        assert_eq!(c.fetch(D::Absolute(9), true).expect("fetch").count, 0);
        assert_eq!(c.fetch(D::Relative(-1), true).expect("fetch").count, 1);
    }

    #[test]
    fn move_backward_all_counts_rows_before_the_current_one() {
        use FetchDirection as D;
        let mut c = cursor(5, true);
        c.fetch(D::Forward(Some(2)), false).expect("fetch");
        assert_eq!(c.fetch(D::Backward(None), true).expect("fetch").count, 1);
        assert!(ids(&c.fetch(D::Forward(Some(-1)), false).expect("fetch")).is_empty());
        assert_eq!(ids(&c.fetch(D::Forward(Some(2)), false).expect("fetch")), [1, 2]);
        assert_eq!(ids(&c.fetch(D::Backward(Some(-1)), false).expect("fetch")), [3]);
    }

    #[test]
    fn no_scroll_cursor_only_moves_forward() {
        use FetchDirection as D;
        let mut c = cursor(5, false);
        assert!(ids(&c.fetch(D::Absolute(0), false).expect("fetch")).is_empty());
        assert_eq!(ids(&c.fetch(D::Absolute(1), false).expect("fetch")), [1]);
        assert_eq!(ids(&c.fetch(D::Absolute(3), false).expect("fetch")), [3]);
        assert!(c.fetch(D::Absolute(2), false).is_err());
        assert!(c.fetch(D::Relative(0), false).is_err());
        assert!(c.fetch(D::Backward(Some(1)), false).is_err());
        let mut fresh = cursor(5, false);
        assert_eq!(fresh.fetch(D::Forward(Some(0)), true).expect("fetch").count, 0);
        assert!(fresh.fetch(D::Forward(Some(-1)), false).is_err());
    }
}
