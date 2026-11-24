//! The binder: resolves names against the catalog and scopes, infers parameter and literal
//! types, resolves operators and functions, and produces executable plans. It follows the
//! structure of Postgres' parse analysis (`parser/analyze.c`, `parse_expr.c`, `parse_agg.c`).

pub mod ddl;
pub mod dml;
mod expr;
mod select;

pub use ddl::{bind_alter_table, bind_create_index, bind_create_sequence, bind_create_table, bind_create_view};
pub use dml::{DeletePlan, InsertPlan, InsertRows, OnConflictPlan, UpdatePlan, bind_delete, bind_insert, bind_update};
pub use select::BoundQuery;

use crate::plan::{AggCall, Expr, WinCall};
use pgmem_catalog::{Catalog, Oid};
use pgmem_types::{SqlError, SqlResult, Type, code};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// One column visible in a FROM scope. Its position in the scope is its position in the row.
#[derive(Clone, Debug)]
pub struct ScopeCol {
    pub qualifier: Option<String>,
    pub name: String,
    pub ty: Type,
    pub typmod: i32,
    pub table_oid: Oid,
    pub attnum: i16,
    /// Not addressable by unqualified name (the right copy of a JOIN ... USING column, or a
    /// dropped table column).
    pub hidden_unqualified: bool,
    /// Not addressable at all (dropped columns keep their slot).
    pub dropped: bool,
}

impl ScopeCol {
    pub fn new(qualifier: Option<String>, name: impl Into<String>, ty: Type) -> ScopeCol {
        ScopeCol { qualifier, name: name.into(), ty, typmod: -1, table_oid: 0, attnum: 0, hidden_unqualified: false, dropped: false }
    }
}

/// A FROM-clause namespace.
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub cols: Vec<ScopeCol>,
    /// Column positions produced by `*`, in order (USING columns first, then the rest).
    pub star: Vec<usize>,
}

impl Scope {
    pub fn from_cols(cols: Vec<ScopeCol>) -> Scope {
        let star = cols.iter().enumerate().filter(|(_, c)| !c.dropped).map(|(i, _)| i).collect();
        Scope { cols, star }
    }

    pub fn width(&self) -> usize {
        self.cols.len()
    }

    /// Positions of `qualifier.*`.
    pub fn qualified_star(&self, qualifier: &str) -> Vec<usize> {
        self.cols.iter().enumerate().filter(|(_, c)| !c.dropped && c.qualifier.as_deref() == Some(qualifier)).map(|(i, _)| i).collect()
    }

    pub fn has_qualifier(&self, qualifier: &str) -> bool {
        self.cols.iter().any(|c| c.qualifier.as_deref() == Some(qualifier))
    }
}

/// A CTE visible by name while binding.
#[derive(Clone, Debug)]
pub struct CteBinding {
    pub name: String,
    pub id: usize,
    pub columns: Vec<(String, Type)>,
}

/// Which clause is being bound; decides whether aggregates and window functions are allowed
/// and how the error is worded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clause {
    SelectList,
    Where,
    Having,
    GroupBy,
    OrderBy,
    JoinOn,
    Values,
    FromFunction,
    Returning,
    Check,
    Default,
    /// Binding an operand of UNION/INTERSECT/EXCEPT: untyped select-list literals stay unknown
    /// until the set operation resolves the column type.
    SetOperand,
    Other,
}

impl Clause {
    fn agg_allowed(self) -> bool {
        matches!(self, Clause::SelectList | Clause::Having | Clause::OrderBy)
    }

    fn window_allowed(self) -> bool {
        matches!(self, Clause::SelectList | Clause::OrderBy)
    }

    fn name(self) -> &'static str {
        match self {
            Clause::SelectList => "SELECT",
            Clause::Where => "WHERE",
            Clause::Having => "HAVING",
            Clause::GroupBy => "GROUP BY",
            Clause::OrderBy => "ORDER BY",
            Clause::JoinOn => "JOIN conditions",
            Clause::Values => "VALUES",
            Clause::FromFunction => "functions in FROM",
            Clause::Returning => "RETURNING",
            Clause::Check => "check constraints",
            Clause::Default => "DEFAULT expressions",
            Clause::SetOperand | Clause::Other => "this context",
        }
    }
}

/// Per-query-level binding state (one per SELECT nesting level).
pub struct Level {
    pub scope: Scope,
    /// Set when an expression at this level (or deeper) referenced an enclosing level.
    /// Shared between a query's placeholder level and its real FROM-scope level.
    pub correlated: Rc<Cell<bool>>,
    /// Aggregate calls collected while binding this level's grouped clauses.
    pub aggs: RefCell<Vec<AggCall>>,
    pub windows: RefCell<Vec<WinCall>>,
}

impl Level {
    pub fn new(scope: Scope) -> Level {
        Level { scope, correlated: Rc::new(Cell::new(false)), aggs: RefCell::new(Vec::new()), windows: RefCell::new(Vec::new()) }
    }

    /// A level for the same query as `other` (sharing its correlation flag).
    pub fn sharing(scope: Scope, other: &Level) -> Level {
        Level { scope, correlated: Rc::clone(&other.correlated), aggs: RefCell::new(Vec::new()), windows: RefCell::new(Vec::new()) }
    }
}

/// Binding context: the scope stack (innermost last) and visible CTEs.
pub struct Ctx<'l> {
    pub levels: Vec<&'l Level>,
    pub ctes: Vec<CteBinding>,
    pub clause: Clause,
    /// Named windows of the current SELECT.
    pub windows: Vec<pgmem_ir::NamedWindow>,
}

impl<'l> Ctx<'l> {
    pub fn new(level: &'l Level) -> Ctx<'l> {
        Ctx { levels: vec![level], ctes: Vec::new(), clause: Clause::Other, windows: Vec::new() }
    }

    pub fn current(&self) -> &'l Level {
        self.levels[self.levels.len() - 1]
    }

    pub fn with_clause(&self, clause: Clause) -> Ctx<'l> {
        Ctx { levels: self.levels.clone(), ctes: self.ctes.clone(), clause, windows: self.windows.clone() }
    }

    /// A context for a nested query level (a subquery evaluated per row of this level).
    pub fn nested<'m>(&self, level: &'m Level) -> Ctx<'m>
    where
        'l: 'm,
    {
        let mut levels: Vec<&'m Level> = self.levels.clone();
        levels.push(level);
        Ctx { levels, ctes: self.ctes.clone(), clause: Clause::Other, windows: Vec::new() }
    }

    /// A context for an item of this level's FROM clause: the current (not yet populated)
    /// level is dropped, because FROM items are evaluated without a current row.
    pub fn from_item<'m>(&self, extra: &[&'m Level]) -> Ctx<'m>
    where
        'l: 'm,
    {
        let mut levels: Vec<&'m Level> = self.levels.clone();
        levels.pop();
        levels.extend_from_slice(extra);
        Ctx { levels, ctes: self.ctes.clone(), clause: Clause::Other, windows: Vec::new() }
    }

    /// The same query level with its FROM scope filled in.
    pub fn nested_same_level<'m>(&self, level: &'m Level) -> Ctx<'m>
    where
        'l: 'm,
    {
        let mut levels: Vec<&'m Level> = self.levels.clone();
        levels.pop();
        levels.push(level);
        Ctx { levels, ctes: self.ctes.clone(), clause: Clause::Other, windows: self.windows.clone() }
    }
}

/// Statement-wide binder state.
pub struct Binder<'a> {
    pub catalog: &'a Catalog,
    /// Parameter types: declared by the client or inferred; `Unknown` until resolved.
    pub params: RefCell<Vec<Type>>,
    next_id: Cell<usize>,
}

impl<'a> Binder<'a> {
    pub fn new(catalog: &'a Catalog, declared: &[Type]) -> Binder<'a> {
        Binder { catalog, params: RefCell::new(declared.to_vec()), next_id: Cell::new(1) }
    }

    pub fn fresh_id(&self) -> usize {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    /// Final parameter types. Parameters whose type could not be inferred are an error, as
    /// in Postgres (42P18), except where they only flowed into an untyped output column
    /// (which Postgres resolves to text): callers mark those via [`Binder::default_unknown_params`].
    pub fn finish_params(&self) -> SqlResult<Vec<Type>> {
        let params = self.params.borrow();
        for (i, t) in params.iter().enumerate() {
            if *t == Type::Unknown {
                return Err(SqlError::new(code::INDETERMINATE_DATATYPE, format!("could not determine data type of parameter ${}", i + 1)));
            }
        }
        Ok(params.clone())
    }

    /// Resolves still-unknown parameter `i` to `ty`.
    pub fn set_param_type(&self, i: usize, ty: &Type) {
        let mut params = self.params.borrow_mut();
        if params.len() <= i {
            params.resize(i + 1, Type::Unknown);
        }
        if params[i] == Type::Unknown {
            params[i] = ty.clone();
        }
    }

    pub fn param_type(&self, i: usize) -> Type {
        let mut params = self.params.borrow_mut();
        if params.len() <= i {
            params.resize(i + 1, Type::Unknown);
        }
        params[i].clone()
    }
}

/// Postgres' reserved keywords (those that can't be column names without quoting).
pub fn is_reserved_keyword(s: &str) -> bool {
    matches!(
        s,
        "all"
            | "analyse"
            | "analyze"
            | "and"
            | "any"
            | "array"
            | "as"
            | "asc"
            | "asymmetric"
            | "both"
            | "case"
            | "cast"
            | "check"
            | "collate"
            | "column"
            | "constraint"
            | "create"
            | "current_catalog"
            | "current_date"
            | "current_role"
            | "current_time"
            | "current_timestamp"
            | "current_user"
            | "default"
            | "deferrable"
            | "desc"
            | "distinct"
            | "do"
            | "else"
            | "end"
            | "except"
            | "false"
            | "fetch"
            | "for"
            | "foreign"
            | "from"
            | "grant"
            | "group"
            | "having"
            | "in"
            | "initially"
            | "intersect"
            | "into"
            | "lateral"
            | "leading"
            | "limit"
            | "localtime"
            | "localtimestamp"
            | "not"
            | "null"
            | "offset"
            | "on"
            | "only"
            | "or"
            | "order"
            | "placing"
            | "primary"
            | "references"
            | "returning"
            | "select"
            | "session_user"
            | "some"
            | "symmetric"
            | "system_user"
            | "table"
            | "then"
            | "to"
            | "trailing"
            | "true"
            | "union"
            | "unique"
            | "user"
            | "using"
            | "variadic"
            | "when"
            | "where"
            | "window"
            | "with"
    )
}

/// Resolves a declared type name (`int4`, `varchar(20)`, `text[]`, a user type) to a type
/// and typmod. `serial` family names are handled by CREATE TABLE before this is called.
pub fn resolve_type_name(catalog: &Catalog, t: &pgmem_ir::TypeName) -> SqlResult<(Type, i32)> {
    let base = match Type::from_sql_name(&t.name) {
        Some(b) => b,
        None => {
            let user = crate::usertypes::lookup_type(catalog, &t.name)
                .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{}\" does not exist", t.name)))?;
            if !t.modifiers.is_empty() {
                return Err(SqlError::syntax(format!("type modifier is not allowed for type \"{}\"", t.name)));
            }
            user
        }
    };
    let typmod = match (t.name.as_str(), t.modifiers.as_slice()) {
        ("varchar" | "character varying" | "bpchar" | "char" | "character", [n]) => {
            if *n < 1 {
                return Err(SqlError::syntax(format!("length for type {} must be at least 1", t.name)));
            }
            crate::cast::varchar_typmod(*n)
        }
        ("numeric" | "decimal", [p]) => crate::cast::numeric_typmod(*p, 0),
        // Fractional-second precision: timestamp(3), time(0), ...
        ("timestamp" | "timestamptz" | "time" | "timetz" | "interval", [p]) => i32::try_from(*p).unwrap_or(-1),
        ("numeric" | "decimal", [p, s]) => {
            if *p < 1 || *p > 1000 {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("NUMERIC precision {p} must be between 1 and 1000")));
            }
            if *s < -1000 || *s > 1000 {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("NUMERIC scale {s} must be between -1000 and 1000")));
            }
            crate::cast::numeric_typmod(*p, *s)
        }
        _ => -1,
    };
    let mut ty = base;
    for _ in 0..t.array_dims {
        ty = Type::array_of(ty);
    }
    Ok((ty, if t.array_dims > 0 { -1 } else { typmod }))
}

pub(crate) fn feature(what: &str) -> SqlError {
    SqlError::feature_not_supported(format!("{what} is not supported by pgmem"))
}

pub(crate) fn agg_misuse(clause: Clause, what: &str) -> SqlError {
    SqlError::new(code::GROUPING_ERROR, format!("{what} are not allowed in {}", clause.name()))
}

pub(crate) fn window_misuse(clause: Clause) -> SqlError {
    SqlError::new(code::WINDOWING_ERROR, format!("window functions are not allowed in {}", clause.name()))
}

pub use expr::{coerce, coerce_assignment, output_name};

#[allow(dead_code)]
fn unused(_: &Expr) {}
