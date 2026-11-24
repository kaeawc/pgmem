//! Bound plans: what the binder produces and the executor runs. Names are resolved to row
//! positions, operators and functions to implementations, and every expression has a type.

use crate::eval::EvalCtx;
use pgmem_catalog::Oid;
use pgmem_types::{SqlResult, Type, Value};
use std::fmt;

pub type Row = Vec<Value>;

/// One result column, as described in `RowDescription`.
#[derive(Clone, Debug, PartialEq)]
pub struct OutCol {
    pub name: String,
    pub ty: Type,
    /// Source table OID and attribute number when the column is a plain table column.
    pub table_oid: Oid,
    pub attnum: i16,
    pub typmod: i32,
}

impl OutCol {
    pub fn new(name: impl Into<String>, ty: Type) -> OutCol {
        OutCol { name: name.into(), ty, table_oid: 0, attnum: 0, typmod: -1 }
    }
}

/// A scalar function implementation: arguments are already evaluated.
pub type ScalarFn = fn(&[Value], &EvalCtx) -> SqlResult<Value>;
/// A set-returning function implementation producing whole rows.
pub type SrfFn = fn(&[Value], &EvalCtx) -> SqlResult<Vec<Row>>;
/// Produces the rows of a virtual catalog relation from the running transaction's catalog
/// and the session.
pub type VirtualRowsFn = fn(&EvalCtx) -> SqlResult<Vec<Row>>;

/// A virtual relation's row source (compared by function address, like [`Srf`]).
#[derive(Clone, Copy)]
pub struct VirtualRows(pub VirtualRowsFn);

impl PartialEq for VirtualRows {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::fn_addr_eq(self.0, other.0)
    }
}

impl fmt::Debug for VirtualRows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VirtualRows")
    }
}

/// A function pointer with a name, compared by name (pointer identity is not stable).
#[derive(Clone, Copy)]
pub struct Func {
    pub name: &'static str,
    pub f: ScalarFn,
    /// STRICT: any NULL argument yields NULL without calling `f`.
    pub strict: bool,
}

impl PartialEq for Func {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.strict == other.strict && std::ptr::fn_addr_eq(self.f, other.f)
    }
}

impl fmt::Debug for Func {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}()", self.name)
    }
}

#[derive(Clone, Copy)]
pub struct Srf {
    pub name: &'static str,
    pub f: SrfFn,
}

impl PartialEq for Srf {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::fn_addr_eq(self.f, other.f)
    }
}

impl fmt::Debug for Srf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}()", self.name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SubqueryKind {
    /// `(SELECT x ...)`: at most one row, one column.
    Scalar,
    /// `EXISTS (...)`.
    Exists,
    /// `probe op ANY/ALL (SELECT ...)` (IN is `= ANY`).
    Quantified { probe: Box<Expr>, cmp: Func, all: bool },
    /// `ARRAY(SELECT ...)`.
    Array,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Const(Value),
    /// A column of the current input row.
    Col(usize),
    /// A column of an enclosing query's current row; `depth` 1 is the immediate parent.
    Outer {
        depth: usize,
        idx: usize,
    },
    /// A bound parameter (0-based).
    Param(usize),
    Call {
        func: Func,
        args: Vec<Expr>,
    },
    Cast {
        expr: Box<Expr>,
        to: Type,
        typmod: i32,
    },
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    IsDistinct {
        left: Box<Expr>,
        right: Box<Expr>,
        negated: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },
    Coalesce(Vec<Expr>),
    /// GREATEST / LEAST: NULLs are ignored.
    MinMax {
        args: Vec<Expr>,
        greatest: bool,
    },
    NullIf(Box<Expr>, Box<Expr>),
    /// `probe IN (list)` with SQL three-valued semantics (comparison already resolved).
    InList {
        probe: Box<Expr>,
        list: Vec<Expr>,
        cmp: Func,
        negated: bool,
    },
    /// `left op ANY|ALL (array)`.
    AnyAll {
        left: Box<Expr>,
        array: Box<Expr>,
        cmp: Func,
        all: bool,
    },
    Subquery {
        id: usize,
        plan: Box<Rel>,
        kind: SubqueryKind,
        correlated: bool,
    },
    Array(Vec<Expr>),
    /// `arr[i]` (1-based) / `arr[lo:hi]`.
    Subscript {
        expr: Box<Expr>,
        index: Box<Expr>,
        upper: Option<Box<Expr>>,
        slice: bool,
    },
    /// A composite value built from its fields (`ROW(...)`, a whole-row reference); the type
    /// OID is a named composite type or `record`. `types` are the fields' bound types (string
    /// literals stay `unknown` until the row is coerced to a named type).
    Row {
        fields: Vec<Expr>,
        types: Vec<Type>,
        type_oid: u32,
    },
    /// `(composite).field`, by field position.
    Field {
        expr: Box<Expr>,
        idx: usize,
    },
    /// The composite `expr` with field `idx` replaced by `value` (`UPDATE ... SET col.f = v`);
    /// a NULL composite becomes a row of NULLs first, as in Postgres.
    FieldStore {
        expr: Box<Expr>,
        idx: usize,
        value: Box<Expr>,
        type_oid: u32,
        width: usize,
    },
    /// Coercion to a domain (`CoerceToDomain`): the value passes through after the domain's
    /// NOT NULL and CHECK constraints accept it.
    CoerceToDomain(Box<DomainCoercion>),
    /// Placeholder for an aggregate call while binding a grouped select list; replaced by a
    /// column reference into the aggregate's output before execution.
    AggCall(Box<AggCall>),
    /// Placeholder for a window function call; replaced like `AggCall`.
    WindowCall(Box<WinCall>),
}

/// A domain's constraints, bound for [`Expr::CoerceToDomain`]. The CHECK expressions see the
/// value as column 0 of a one-column row (`VALUE`).
#[derive(Clone, Debug, PartialEq)]
pub struct DomainCoercion {
    pub expr: Expr,
    pub domain: String,
    pub not_null: bool,
    pub checks: Vec<(String, Expr)>,
}

impl Expr {
    pub fn null() -> Expr {
        Expr::Const(Value::Null)
    }

    pub fn call(func: Func, args: Vec<Expr>) -> Expr {
        Expr::Call { func, args }
    }

    /// True when the expression references the current row or an enclosing row.
    pub fn is_constant(&self) -> bool {
        let mut constant = true;
        self.walk(&mut |e| {
            if matches!(e, Expr::Col(_) | Expr::Outer { .. } | Expr::Subquery { .. } | Expr::AggCall(_) | Expr::WindowCall(_)) {
                constant = false;
            }
        });
        constant
    }

    /// Pre-order traversal of this expression (not descending into subquery plans).
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        match self {
            Expr::Const(_) | Expr::Col(_) | Expr::Outer { .. } | Expr::Param(_) => {}
            Expr::Call { args, .. }
            | Expr::And(args)
            | Expr::Or(args)
            | Expr::Coalesce(args)
            | Expr::Array(args)
            | Expr::MinMax { args, .. } => {
                args.iter().for_each(|a| a.walk(f));
            }
            Expr::Cast { expr, .. } | Expr::Not(expr) | Expr::IsNull { expr, .. } | Expr::Field { expr, .. } => expr.walk(f),
            Expr::Row { fields, .. } => fields.iter().for_each(|a| a.walk(f)),
            Expr::FieldStore { expr, value, .. } => {
                expr.walk(f);
                value.walk(f);
            }
            // The CHECK expressions run against their own one-column row.
            Expr::CoerceToDomain(d) => d.expr.walk(f),
            Expr::IsDistinct { left, right, .. } | Expr::NullIf(left, right) => {
                left.walk(f);
                right.walk(f);
            }
            Expr::Case { operand, whens, else_result } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (w, t) in whens {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(e) = else_result {
                    e.walk(f);
                }
            }
            Expr::InList { probe, list, .. } => {
                probe.walk(f);
                list.iter().for_each(|a| a.walk(f));
            }
            Expr::AnyAll { left, array, .. } => {
                left.walk(f);
                array.walk(f);
            }
            Expr::Subquery { kind, .. } => {
                if let SubqueryKind::Quantified { probe, .. } = kind {
                    probe.walk(f);
                }
            }
            Expr::Subscript { expr, index, upper, .. } => {
                expr.walk(f);
                index.walk(f);
                if let Some(u) = upper {
                    u.walk(f);
                }
            }
            Expr::AggCall(a) => {
                a.args.iter().for_each(|x| x.walk(f));
                if let Some(fl) = &a.filter {
                    fl.walk(f);
                }
            }
            Expr::WindowCall(w) => w.args.iter().for_each(|x| x.walk(f)),
        }
    }

    /// Rewrites the expression bottom-up (children first, then `f` on the node).
    pub fn transform(self, f: &mut dyn FnMut(Expr) -> SqlResult<Expr>) -> SqlResult<Expr> {
        let boxed = |e: Box<Expr>, f: &mut dyn FnMut(Expr) -> SqlResult<Expr>| -> SqlResult<Box<Expr>> { Ok(Box::new(e.transform(f)?)) };
        let vec = |v: Vec<Expr>, f: &mut dyn FnMut(Expr) -> SqlResult<Expr>| -> SqlResult<Vec<Expr>> {
            v.into_iter().map(|e| e.transform(f)).collect()
        };
        let node = match self {
            e @ (Expr::Const(_) | Expr::Col(_) | Expr::Outer { .. } | Expr::Param(_) | Expr::AggCall(_) | Expr::WindowCall(_)) => e,
            Expr::Call { func, args } => Expr::Call { func, args: vec(args, f)? },
            Expr::Cast { expr, to, typmod } => Expr::Cast { expr: boxed(expr, f)?, to, typmod },
            Expr::And(v) => Expr::And(vec(v, f)?),
            Expr::Or(v) => Expr::Or(vec(v, f)?),
            Expr::Not(e) => Expr::Not(boxed(e, f)?),
            Expr::IsNull { expr, negated } => Expr::IsNull { expr: boxed(expr, f)?, negated },
            Expr::IsDistinct { left, right, negated } => Expr::IsDistinct { left: boxed(left, f)?, right: boxed(right, f)?, negated },
            Expr::Case { operand, whens, else_result } => Expr::Case {
                operand: operand.map(|o| boxed(o, f)).transpose()?,
                whens: whens.into_iter().map(|(w, t)| Ok((w.transform(f)?, t.transform(f)?))).collect::<SqlResult<Vec<_>>>()?,
                else_result: else_result.map(|e| boxed(e, f)).transpose()?,
            },
            Expr::Coalesce(v) => Expr::Coalesce(vec(v, f)?),
            Expr::MinMax { args, greatest } => Expr::MinMax { args: vec(args, f)?, greatest },
            Expr::NullIf(a, b) => Expr::NullIf(boxed(a, f)?, boxed(b, f)?),
            Expr::InList { probe, list, cmp, negated } => Expr::InList { probe: boxed(probe, f)?, list: vec(list, f)?, cmp, negated },
            Expr::AnyAll { left, array, cmp, all } => Expr::AnyAll { left: boxed(left, f)?, array: boxed(array, f)?, cmp, all },
            Expr::Subquery { id, plan, kind, correlated } => {
                let kind = match kind {
                    SubqueryKind::Quantified { probe, cmp, all } => SubqueryKind::Quantified { probe: boxed(probe, f)?, cmp, all },
                    k => k,
                };
                Expr::Subquery { id, plan, kind, correlated }
            }
            Expr::Array(v) => Expr::Array(vec(v, f)?),
            Expr::Subscript { expr, index, upper, slice } => {
                Expr::Subscript { expr: boxed(expr, f)?, index: boxed(index, f)?, upper: upper.map(|u| boxed(u, f)).transpose()?, slice }
            }
            Expr::Row { fields, types, type_oid } => Expr::Row { fields: vec(fields, f)?, types, type_oid },
            Expr::Field { expr, idx } => Expr::Field { expr: boxed(expr, f)?, idx },
            Expr::FieldStore { expr, idx, value, type_oid, width } => {
                Expr::FieldStore { expr: boxed(expr, f)?, idx, value: boxed(value, f)?, type_oid, width }
            }
            Expr::CoerceToDomain(mut d) => {
                d.expr = d.expr.transform(f)?;
                Expr::CoerceToDomain(d)
            }
        };
        f(node)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggKind {
    CountStar,
    Count,
    Sum,
    Avg,
    Min,
    Max,
    BoolAnd,
    BoolOr,
    StringAgg,
    ArrayAgg,
    JsonbAgg,
    JsonbObjectAgg,
    /// `json_agg` / `json_object_agg`: json text output.
    JsonAgg,
    JsonObjectAgg,
    /// The `_strict` variants skip NULL values.
    JsonbAggStrict,
    JsonAggStrict,
    JsonbObjectAggStrict,
    JsonObjectAggStrict,
    StddevSamp,
    StddevPop,
    VarSamp,
    VarPop,
    BitAnd,
    BitOr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AggCall {
    pub kind: AggKind,
    pub args: Vec<Expr>,
    pub distinct: bool,
    pub filter: Option<Expr>,
    pub order_by: Vec<SortKey>,
    /// Input (argument) type and result type.
    pub arg_ty: Type,
    pub ty: Type,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinKind {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue,
    Agg(AggKind),
}

#[derive(Clone, Debug, PartialEq)]
pub struct WinCall {
    pub kind: WinKind,
    pub args: Vec<Expr>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<SortKey>,
    pub frame: Frame,
    pub ty: Type,
    /// For aggregate window functions.
    pub agg: Option<AggCall>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Expr),
    CurrentRow,
    Following(Expr),
    UnboundedFollowing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUnits {
    Rows,
    Range,
    Groups,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub units: FrameUnits,
    pub start: FrameBound,
    pub end: FrameBound,
}

impl Frame {
    /// The SQL default: RANGE BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW.
    pub fn default_frame() -> Frame {
        Frame { units: FrameUnits::Range, start: FrameBound::UnboundedPreceding, end: FrameBound::CurrentRow }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SortKey {
    pub expr: Expr,
    pub desc: bool,
    pub nulls_first: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Rel {
    /// Literal rows (also the single empty row of a FROM-less SELECT).
    Values {
        rows: Vec<Vec<Expr>>,
    },
    /// A `pg_catalog` / `information_schema` relation, computed when the statement runs.
    Virtual {
        rows: VirtualRows,
    },
    /// A table's live columns: physical slot of each output column, and the value rows
    /// shorter than the slot read back as.
    Scan {
        table: Oid,
        slots: Vec<usize>,
        missing: Vec<Value>,
    },
    FuncScan {
        srf: Srf,
        args: Vec<Expr>,
        ordinality: bool,
    },
    Filter {
        input: Box<Rel>,
        pred: Expr,
    },
    Project {
        input: Box<Rel>,
        exprs: Vec<Expr>,
    },
    Join {
        kind: JoinKind,
        left: Box<Rel>,
        right: Box<Rel>,
        on: Option<Expr>,
        lateral: bool,
        left_width: usize,
        right_width: usize,
    },
    /// Output: group keys, then one column per aggregate. With no keys, exactly one row.
    Aggregate {
        input: Box<Rel>,
        group: Vec<Expr>,
        aggs: Vec<AggCall>,
    },
    /// Output: input columns, then one column per window call.
    Window {
        input: Box<Rel>,
        calls: Vec<WinCall>,
    },
    Sort {
        input: Box<Rel>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Box<Rel>,
        limit: Option<Expr>,
        offset: Option<Expr>,
        /// WITH TIES: also emit following rows equal to the last one on these keys.
        ties: Option<Vec<SortKey>>,
    },
    Distinct {
        input: Box<Rel>,
        on: Option<Vec<Expr>>,
    },
    SetOp {
        op: SetOpKind,
        all: bool,
        left: Box<Rel>,
        right: Box<Rel>,
    },
    /// Rows of a CTE materialized earlier in the statement (or the working table of a
    /// recursive CTE while it iterates).
    CteScan {
        id: usize,
    },
    With {
        ctes: Vec<CtePlan>,
        body: Box<Rel>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CtePlan {
    pub id: usize,
    pub plan: Rel,
    /// For `WITH RECURSIVE`: the recursive term (reading `CteScan { id }` as the working
    /// table) and whether it is UNION ALL.
    pub recursive: Option<(Rel, bool)>,
}
