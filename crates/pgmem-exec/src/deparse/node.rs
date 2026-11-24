//! The analyzed expression tree: a small mirror of the Postgres nodes `ruleutils.c` prints
//! (`Const`, `Var`, `OpExpr`, `FuncExpr`, `RelabelType` / coercions, `BoolExpr`, `NullTest`,
//! `ScalarArrayOpExpr`, `CaseExpr`, `SubLink`, ...), and the analysis that builds it from the
//! IR the way Postgres' parse analysis (`parse_expr.c`, `parse_coerce.c`) would, inserting
//! the implicit coercions that later show up in the deparsed text.

use super::pg;
use super::query::Query;
use crate::bind::resolve_type_name;
use pgmem_catalog::Catalog;
use pgmem_ir as ir;
use pgmem_types::{Numeric, Type, text};

/// How a function call is printed (`COERCE_SQL_SYNTAX` functions use their SQL spelling).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Syntax {
    Call,
    Extract,
    Position,
    Substring,
    Trim(&'static str),
    Overlay,
    AtTimeZone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubLinkKind {
    Exists,
    Any,
    All,
    Expr,
    Array,
}

#[derive(Clone, Debug)]
pub(super) struct Sort {
    pub expr: Node,
    pub desc: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Clone, Debug)]
pub(super) struct WinSpec {
    pub base: Option<String>,
    pub partition_by: Vec<Node>,
    pub order_by: Vec<Sort>,
    pub frame: Option<Frame>,
}

#[derive(Clone, Debug)]
pub(super) struct Frame {
    pub units: ir::FrameUnits,
    pub start: Bound,
    pub end: Bound,
    pub exclude: ir::FrameExclude,
}

#[derive(Clone, Debug)]
pub(super) enum Bound {
    UnboundedPreceding,
    Preceding(Node),
    CurrentRow,
    Following(Node),
    UnboundedFollowing,
}

#[derive(Clone, Debug)]
pub(super) struct AggCall {
    pub name: String,
    pub args: Vec<Node>,
    pub star: bool,
    pub distinct: bool,
    pub order_by: Vec<Sort>,
    pub within_group: Vec<Sort>,
    pub filter: Option<Node>,
    /// `OVER` for window calls (`Err(name)` = a named window).
    pub over: Option<Result<WinSpec, String>>,
    pub ty: Type,
}

#[derive(Clone, Debug)]
pub(super) enum Node {
    Const {
        ty: Type,
        typmod: i32,
        /// Text output of the value; `None` is NULL.
        value: Option<String>,
    },
    Var {
        refname: String,
        name: String,
        ty: Type,
        typmod: i32,
    },
    Param {
        n: usize,
        ty: Type,
    },
    /// A binary (`left` set) or prefix operator.
    Op {
        op: String,
        left: Option<Box<Node>>,
        right: Box<Node>,
        ty: Type,
    },
    Func {
        name: String,
        args: Vec<Node>,
        ty: Type,
        syntax: Syntax,
        variadic: bool,
    },
    /// Any type coercion (`RelabelType`, cast `FuncExpr`, `CoerceViaIO`, `ArrayCoerceExpr`):
    /// they all print as `(arg)::type`, or as just `arg` when implicit at the top level.
    Coerce {
        arg: Box<Node>,
        ty: Type,
        typmod: i32,
        implicit: bool,
    },
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    NullTest {
        arg: Box<Node>,
        negated: bool,
    },
    BoolTest {
        arg: Box<Node>,
        test: &'static str,
    },
    Distinct {
        left: Box<Node>,
        right: Box<Node>,
    },
    ScalarArrayOp {
        op: String,
        any: bool,
        left: Box<Node>,
        right: Box<Node>,
    },
    Case {
        arg: Option<Box<Node>>,
        whens: Vec<(Node, Node)>,
        default: Box<Node>,
        ty: Type,
    },
    Coalesce {
        args: Vec<Node>,
        ty: Type,
    },
    MinMax {
        greatest: bool,
        args: Vec<Node>,
        ty: Type,
    },
    NullIf {
        left: Box<Node>,
        right: Box<Node>,
        ty: Type,
    },
    Array {
        elems: Vec<Node>,
        ty: Type,
    },
    Row(Vec<Node>),
    SubLink {
        kind: SubLinkKind,
        test: Option<Box<Node>>,
        op: String,
        query: Box<Query>,
        ty: Type,
    },
    /// `CURRENT_TIMESTAMP`, `CURRENT_USER`, ... (with an optional precision).
    SqlValue {
        keyword: &'static str,
        precision: Option<i64>,
        ty: Type,
    },
    Agg(Box<AggCall>),
    Subscript {
        arg: Box<Node>,
        lower: Option<Box<Node>>,
        upper: Option<Box<Node>>,
        slice: bool,
        ty: Type,
    },
    Collate {
        arg: Box<Node>,
        collation: String,
    },
    /// Text printed verbatim (shapes the deparser cannot model).
    Raw {
        text: String,
        ty: Type,
    },
}

impl Node {
    pub fn ty(&self) -> Type {
        match self {
            Node::Const { ty, .. }
            | Node::Var { ty, .. }
            | Node::Param { ty, .. }
            | Node::Op { ty, .. }
            | Node::Func { ty, .. }
            | Node::Coerce { ty, .. }
            | Node::Case { ty, .. }
            | Node::Coalesce { ty, .. }
            | Node::MinMax { ty, .. }
            | Node::NullIf { ty, .. }
            | Node::Array { ty, .. }
            | Node::SubLink { ty, .. }
            | Node::SqlValue { ty, .. }
            | Node::Subscript { ty, .. }
            | Node::Raw { ty, .. } => ty.clone(),
            Node::Agg(a) => a.ty.clone(),
            Node::Collate { arg, .. } => arg.ty(),
            Node::And(_)
            | Node::Or(_)
            | Node::Not(_)
            | Node::NullTest { .. }
            | Node::BoolTest { .. }
            | Node::Distinct { .. }
            | Node::ScalarArrayOp { .. } => Type::Bool,
            Node::Row(_) => Type::Unknown,
        }
    }

    pub fn typmod(&self) -> i32 {
        match self {
            Node::Const { typmod, .. } | Node::Var { typmod, .. } | Node::Coerce { typmod, .. } => *typmod,
            _ => -1,
        }
    }

    fn null(ty: Type) -> Node {
        Node::Const { ty, typmod: -1, value: None }
    }

    /// True for nodes that reference a column of the current query level (`contain_vars_of_level`).
    pub fn has_vars(&self) -> bool {
        match self {
            Node::Var { .. } | Node::Agg(_) => true,
            Node::Const { .. } | Node::Param { .. } | Node::SqlValue { .. } | Node::Raw { .. } | Node::SubLink { .. } => false,
            Node::Op { left, right, .. } => left.as_ref().is_some_and(|l| l.has_vars()) || right.has_vars(),
            Node::Coerce { arg, .. }
            | Node::Not(arg)
            | Node::NullTest { arg, .. }
            | Node::BoolTest { arg, .. }
            | Node::Collate { arg, .. } => arg.has_vars(),
            Node::Func { args, .. }
            | Node::And(args)
            | Node::Or(args)
            | Node::Coalesce { args, .. }
            | Node::MinMax { args, .. }
            | Node::Array { elems: args, .. }
            | Node::Row(args) => args.iter().any(Node::has_vars),
            Node::Distinct { left, right } | Node::NullIf { left, right, .. } | Node::ScalarArrayOp { left, right, .. } => {
                left.has_vars() || right.has_vars()
            }
            Node::Case { arg, whens, default, .. } => {
                arg.as_ref().is_some_and(|a| a.has_vars()) || whens.iter().any(|(w, t)| w.has_vars() || t.has_vars()) || default.has_vars()
            }
            Node::Subscript { arg, lower, upper, .. } => {
                arg.has_vars() || lower.as_ref().is_some_and(|l| l.has_vars()) || upper.as_ref().is_some_and(|u| u.has_vars())
            }
        }
    }
}

/// Output text of an untyped literal once typed as `ty` (the value Postgres stores in the
/// coerced `Const`): `'2020-01-01'::timestamp` becomes `2020-01-01 00:00:00`.
fn normalize_literal(s: &str, ty: &Type) -> String {
    match ty {
        Type::Unknown | Type::Text | Type::Varchar | Type::Bpchar | Type::Name | Type::Json => s.to_owned(),
        Type::Reg(_) => s.strip_prefix("public.").unwrap_or(s).to_owned(),
        _ => text::decode(s, ty).and_then(|v| text::encode(&v, ty)).unwrap_or_else(|_| s.to_owned()),
    }
}

/// `coerce_type` + `coerce_type_typmod`: converts `n` to `to`. Untyped literals become typed
/// constants; anything else gets a coercion node (implicit unless `explicit`).
pub(super) fn coerce(n: Node, to: &Type, typmod: i32, explicit: bool) -> Node {
    if *to == Type::Unknown {
        return n;
    }
    let from = n.ty();
    let n = match n {
        Node::Const { ty: Type::Unknown, value, .. } => {
            let value = value.map(|s| normalize_literal(&s, to));
            let c = Node::Const { ty: to.clone(), typmod: -1, value };
            // Interval input applies the typmod itself; other types get a length coercion.
            if *to == Type::Interval {
                return c;
            }
            c
        }
        Node::Param { n, ty: Type::Unknown } => Node::Param { n, ty: to.clone() },
        other => other,
    };
    let same = n.ty() == *to;
    if same && (typmod < 0 || typmod == n.typmod()) {
        return n;
    }
    if from == Type::Unknown && !matches!(n, Node::Const { .. } | Node::Param { .. }) {
        return n;
    }
    Node::Coerce { arg: Box::new(n), ty: to.clone(), typmod, implicit: !explicit }
}

fn implicit(n: Node, to: &Type) -> Node {
    coerce(n, to, -1, false)
}

/// One column visible in a FROM scope.
#[derive(Clone, Debug)]
pub(super) struct ScopeCol {
    pub name: String,
    pub ty: Type,
    pub typmod: i32,
    /// The right-hand copy of a `JOIN ... USING` column: not addressable unqualified.
    pub hidden: bool,
}

/// A range-table entry of a query level.
#[derive(Clone, Debug)]
pub(super) struct Rte {
    /// The name qualified references use (alias or relation name, as written).
    pub qualifier: String,
    /// The name printed (made unique across enclosing levels, like `set_rtable_names`).
    pub refname: String,
    pub cols: Vec<ScopeCol>,
}

/// A CTE visible by name.
#[derive(Clone, Debug)]
pub(super) struct CteScope {
    pub name: String,
    pub cols: Vec<ScopeCol>,
}

#[derive(Default)]
pub(super) struct Level {
    pub rtes: Vec<Rte>,
    pub names: Vec<String>,
    pub ctes: Vec<CteScope>,
    pub windows: Vec<ir::NamedWindow>,
}

pub(super) struct Analyzer<'a> {
    pub catalog: &'a Catalog,
    pub levels: Vec<Level>,
}

impl Analyzer<'_> {
    pub fn expr(&mut self, e: &ir::Expr) -> Node {
        match e {
            ir::Expr::Literal(l) => literal(l),
            ir::Expr::Column(parts) => self.column(parts),
            ir::Expr::Param(n) => Node::Param { n: *n, ty: Type::Unknown },
            ir::Expr::TypedString { ty, value } => typed_string(self.catalog, ty, value),
            ir::Expr::Unary { op, expr } => self.unary(*op, expr),
            ir::Expr::Binary { op, left, right } => {
                let (l, r) = (self.expr(left), self.expr(right));
                make_op(op, l, r)
            }
            ir::Expr::And(..) | ir::Expr::Or(..) => self.bool_expr(e),
            ir::Expr::Not(inner) => Node::Not(Box::new(self.boolean(inner))),
            ir::Expr::IsNull { expr, negated } => Node::NullTest { arg: Box::new(self.expr(expr)), negated: *negated },
            ir::Expr::IsBool { expr, value, negated } => {
                let test = match (value, negated) {
                    (Some(true), false) => "IS TRUE",
                    (Some(true), true) => "IS NOT TRUE",
                    (Some(false), false) => "IS FALSE",
                    (Some(false), true) => "IS NOT FALSE",
                    (None, false) => "IS UNKNOWN",
                    (None, true) => "IS NOT UNKNOWN",
                };
                Node::BoolTest { arg: Box::new(self.boolean(expr)), test }
            }
            ir::Expr::IsDistinctFrom { left, right, negated } => {
                let (l, r) = (self.expr(left), self.expr(right));
                let (lt, rt, _) = pg::resolve_binary("=", &l.ty(), &r.ty());
                let d = Node::Distinct { left: Box::new(implicit(l, &lt)), right: Box::new(implicit(r, &rt)) };
                if *negated { Node::Not(Box::new(d)) } else { d }
            }
            ir::Expr::Between { expr, low, high, negated, symmetric } => self.between(expr, low, high, *negated, *symmetric),
            ir::Expr::InList { expr, list, negated } => self.in_list(expr, list, *negated),
            _ => self.expr2(e),
        }
    }

    fn expr2(&mut self, e: &ir::Expr) -> Node {
        match e {
            ir::Expr::InSubquery { expr, query, negated } => {
                let s = self.sublink_test(expr, "=", query, SubLinkKind::Any);
                if *negated { Node::Not(Box::new(s)) } else { s }
            }
            ir::Expr::Exists { query, negated } => {
                let q = self.subquery(query);
                let s = Node::SubLink { kind: SubLinkKind::Exists, test: None, op: String::new(), query: Box::new(q), ty: Type::Bool };
                if *negated { Node::Not(Box::new(s)) } else { s }
            }
            ir::Expr::Subquery(query) => {
                let q = self.subquery(query);
                let ty = q.columns.first().map_or(Type::Unknown, |c| c.ty.clone());
                Node::SubLink { kind: SubLinkKind::Expr, test: None, op: String::new(), query: Box::new(q), ty }
            }
            ir::Expr::ArraySubquery(query) => {
                let q = self.subquery(query);
                let ty = Type::array_of(q.columns.first().map_or(Type::Text, |c| c.ty.clone()));
                Node::SubLink { kind: SubLinkKind::Array, test: None, op: String::new(), query: Box::new(q), ty }
            }
            ir::Expr::AnyAll { left, op, right, all } => self.any_all(left, op, right, *all),
            ir::Expr::Like { expr, pattern, escape, negated, case_insensitive } => {
                let op = match (negated, case_insensitive) {
                    (false, false) => "~~",
                    (true, false) => "!~~",
                    (false, true) => "~~*",
                    (true, true) => "!~~*",
                };
                let l = self.expr(expr);
                let p = self.expr(pattern);
                let p = match escape {
                    Some(esc) => {
                        let esc = self.expr(esc);
                        Node::Func {
                            name: "like_escape".into(),
                            args: vec![implicit(p, &Type::Text), implicit(esc, &Type::Text)],
                            ty: Type::Text,
                            syntax: Syntax::Call,
                            variadic: false,
                        }
                    }
                    None => p,
                };
                make_op(op, l, p)
            }
            ir::Expr::SimilarTo { expr, pattern, escape, negated } => {
                let l = self.expr(expr);
                let mut args = vec![implicit(self.expr(pattern), &Type::Text)];
                if let Some(esc) = escape {
                    args.push(implicit(self.expr(esc), &Type::Text));
                }
                let p = Node::Func { name: "similar_to_escape".into(), args, ty: Type::Text, syntax: Syntax::Call, variadic: false };
                make_op(if *negated { "!~" } else { "~" }, l, p)
            }
            ir::Expr::Case { operand, whens, else_result } => self.case(operand.as_deref(), whens, else_result.as_deref()),
            ir::Expr::Cast { expr, ty } => self.cast(expr, ty),
            ir::Expr::Function(f) => self.function(f),
            _ => self.expr3(e),
        }
    }

    fn expr3(&mut self, e: &ir::Expr) -> Node {
        match e {
            ir::Expr::Array(items) => {
                let elems: Vec<Node> = items.iter().map(|i| self.expr(i)).collect();
                let t = pg::common_type(&elems.iter().map(Node::ty).collect::<Vec<_>>());
                let elems = elems.into_iter().map(|n| implicit(n, &t)).collect();
                Node::Array { elems, ty: Type::array_of(t) }
            }
            ir::Expr::Subscript { expr, index } => {
                let arg = self.expr(expr);
                let at = arg.ty();
                let int = |a: &mut Self, e: &ir::Expr| Box::new(implicit(a.expr(e), &Type::Int4));
                match index {
                    ir::Subscript::Index(i) => {
                        let ty = at.element().cloned().unwrap_or(Type::Unknown);
                        Node::Subscript { arg: Box::new(arg), lower: None, upper: Some(int(self, i)), slice: false, ty }
                    }
                    ir::Subscript::Slice { lower, upper } => {
                        let lower = lower.as_ref().map(|l| int(self, l));
                        let upper = upper.as_ref().map(|u| int(self, u));
                        Node::Subscript { arg: Box::new(arg), lower, upper, slice: true, ty: at }
                    }
                }
            }
            ir::Expr::Row(items) => Node::Row(items.iter().map(|i| self.expr(i)).collect()),
            ir::Expr::Collate { expr, collation } => Node::Collate { arg: Box::new(self.expr(expr)), collation: collation.clone() },
            ir::Expr::AtTimeZone { expr, zone } => {
                let ts = self.expr(expr);
                let z = self.expr(zone);
                let zt = if z.ty() == Type::Interval { Type::Interval } else { Type::Text };
                let (src, ret) = match ts.ty() {
                    Type::Timestamp => (Type::Timestamp, Type::Timestamptz),
                    Type::Time => (Type::Time, Type::TimeTz),
                    Type::TimeTz => (Type::TimeTz, Type::TimeTz),
                    _ => (Type::Timestamptz, Type::Timestamp),
                };
                let args = vec![implicit(z, &zt), implicit(ts, &src)];
                Node::Func { name: "timezone".into(), args, ty: ret, syntax: Syntax::AtTimeZone, variadic: false }
            }
            ir::Expr::Default => Node::Raw { text: "DEFAULT".into(), ty: Type::Unknown },
            // Handled by `expr` / `expr2`.
            _ => Node::Raw { text: String::new(), ty: Type::Unknown },
        }
    }

    /// A boolean context (`AND`, `NOT`, `WHERE`, ...): unknown literals become boolean.
    pub fn boolean(&mut self, e: &ir::Expr) -> Node {
        let n = self.expr(e);
        implicit(n, &Type::Bool)
    }

    fn column(&mut self, parts: &[String]) -> Node {
        let (qualifier, name) = match parts {
            [n] => (None, n.as_str()),
            [q, n] => (Some(q.as_str()), n.as_str()),
            [.., q, n] => (Some(q.as_str()), n.as_str()),
            [] => (None, ""),
        };
        for level in self.levels.iter().rev() {
            for rte in &level.rtes {
                if qualifier.is_some_and(|q| q != rte.qualifier) {
                    continue;
                }
                if let Some(c) = rte.cols.iter().find(|c| c.name == name && (qualifier.is_some() || !c.hidden)) {
                    return Node::Var { refname: rte.refname.clone(), name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod };
                }
            }
        }
        Node::Var { refname: qualifier.unwrap_or_default().to_owned(), name: name.to_owned(), ty: Type::Unknown, typmod: -1 }
    }

    fn unary(&mut self, op: ir::UnaryOp, expr: &ir::Expr) -> Node {
        let name = match op {
            ir::UnaryOp::Minus => "-",
            ir::UnaryOp::Plus => "+",
            ir::UnaryOp::BitNot => "~",
            ir::UnaryOp::SquareRoot => "|/",
            ir::UnaryOp::CubeRoot => "||/",
            ir::UnaryOp::Abs => "@",
        };
        // The grammar folds a minus sign into a numeric literal (`doNegate`).
        if let (ir::UnaryOp::Minus, ir::Expr::Literal(l @ (ir::Literal::Integer(_) | ir::Literal::Decimal(_)))) = (op, expr) {
            let negated = match l {
                ir::Literal::Integer(s) => ir::Literal::Integer(format!("-{s}")),
                ir::Literal::Decimal(s) => ir::Literal::Decimal(format!("-{s}")),
                other => other.clone(),
            };
            return literal(&negated);
        }
        let arg = self.expr(expr);
        let (at, ret) = pg::resolve_unary(name, &arg.ty());
        Node::Op { op: name.to_owned(), left: None, right: Box::new(implicit(arg, &at)), ty: ret }
    }

    /// `a AND b AND c` is one node: the grammar flattens a left-nested chain (`makeAndExpr`).
    fn bool_expr(&mut self, e: &ir::Expr) -> Node {
        let and = matches!(e, ir::Expr::And(..));
        let mut operands = Vec::new();
        let mut cur = e;
        loop {
            match (cur, and) {
                (ir::Expr::And(l, r), true) | (ir::Expr::Or(l, r), false) => {
                    operands.push(r.as_ref());
                    cur = l;
                }
                _ => {
                    operands.push(cur);
                    break;
                }
            }
        }
        operands.reverse();
        let args = operands.into_iter().map(|o| self.boolean(o)).collect();
        if and { Node::And(args) } else { Node::Or(args) }
    }

    fn between(&mut self, expr: &ir::Expr, low: &ir::Expr, high: &ir::Expr, negated: bool, symmetric: bool) -> Node {
        let pair = |a: &mut Self, lo: &ir::Expr, hi: &ir::Expr| {
            let (x1, l) = (a.expr(expr), a.expr(lo));
            let (x2, h) = (a.expr(expr), a.expr(hi));
            if negated {
                Node::Or(vec![make_op("<", x1, l), make_op(">", x2, h)])
            } else {
                Node::And(vec![make_op(">=", x1, l), make_op("<=", x2, h)])
            }
        };
        let first = pair(self, low, high);
        if !symmetric {
            return first;
        }
        let second = pair(self, high, low);
        if negated { Node::And(vec![first, second]) } else { Node::Or(vec![first, second]) }
    }

    /// `transformAExprIn`: constant items become `= ANY (ARRAY[...])`, the rest `=` comparisons
    /// OR-ed (or `<> ALL` / AND-ed for NOT IN).
    fn in_list(&mut self, expr: &ir::Expr, list: &[ir::Expr], negated: bool) -> Node {
        let op = if negated { "<>" } else { "=" };
        let lexpr = self.expr(expr);
        let items: Vec<Node> = list.iter().map(|i| self.expr(i)).collect();
        let nonvars: Vec<&Node> = items.iter().filter(|n| !n.has_vars()).collect();
        let mut result: Option<Node> = None;
        let rest = if nonvars.len() > 1 {
            let mut types = vec![lexpr.ty()];
            types.extend(nonvars.iter().map(|n| n.ty()));
            let t = pg::common_type(&types);
            let (vars, nonvars): (Vec<Node>, Vec<Node>) = items.into_iter().partition(Node::has_vars);
            let elems = nonvars.into_iter().map(|n| implicit(n, &t)).collect();
            let array = Node::Array { elems, ty: Type::array_of(t) };
            result = Some(scalar_array_op(op, !negated, lexpr.clone(), array));
            vars
        } else {
            items
        };
        for item in rest {
            let cmp = make_op(op, lexpr.clone(), item);
            result = Some(match result {
                None => cmp,
                Some(prev) if negated => Node::And(vec![prev, cmp]),
                Some(prev) => Node::Or(vec![prev, cmp]),
            });
        }
        result.unwrap_or_else(|| Node::Const { ty: Type::Bool, typmod: -1, value: Some(if negated { "t" } else { "f" }.into()) })
    }

    fn any_all(&mut self, left: &ir::Expr, op: &str, right: &ir::AnyAllRight, all: bool) -> Node {
        match right {
            ir::AnyAllRight::Array(arr) => {
                let l = self.expr(left);
                let a = self.expr(arr);
                scalar_array_op(op, !all, l, a)
            }
            ir::AnyAllRight::Subquery(q) => self.sublink_test(left, op, q, if all { SubLinkKind::All } else { SubLinkKind::Any }),
        }
    }

    fn sublink_test(&mut self, left: &ir::Expr, op: &str, query: &ir::Query, kind: SubLinkKind) -> Node {
        let l = self.expr(left);
        let q = self.subquery(query);
        let ct = q.columns.first().map_or(Type::Unknown, |c| c.ty.clone());
        let (lt, _, _) = pg::resolve_binary(op, &l.ty(), &ct);
        let op = if op == "!=" { "<>" } else { op };
        Node::SubLink { kind, test: Some(Box::new(implicit(l, &lt))), op: op.to_owned(), query: Box::new(q), ty: Type::Bool }
    }

    fn case(&mut self, operand: Option<&ir::Expr>, whens: &[(ir::Expr, ir::Expr)], else_result: Option<&ir::Expr>) -> Node {
        let arg = operand.map(|o| self.expr(o));
        let mut conds = Vec::new();
        let mut results = Vec::new();
        for (w, t) in whens {
            let cond = match &arg {
                // `CaseTestExpr = w`: only the (coerced) right-hand side is printed.
                Some(a) => {
                    let w = self.expr(w);
                    let (_, rt, _) = pg::resolve_binary("=", &a.ty(), &w.ty());
                    implicit(w, &rt)
                }
                None => self.boolean(w),
            };
            conds.push(cond);
            results.push(self.expr(t));
        }
        let default = else_result.map(|e| self.expr(e));
        let mut types: Vec<Type> = default.iter().map(Node::ty).collect();
        types.extend(results.iter().map(Node::ty));
        let ty = pg::common_type(&types);
        let whens = conds.into_iter().zip(results).map(|(c, r)| (c, implicit(r, &ty))).collect();
        let default = default.map_or_else(|| Node::null(ty.clone()), |d| implicit(d, &ty));
        Node::Case { arg: arg.map(Box::new), whens, default: Box::new(default), ty }
    }

    fn cast(&mut self, expr: &ir::Expr, ty: &ir::TypeName) -> Node {
        let Ok((t, typmod)) = resolve_type_name(self.catalog, ty) else {
            let inner = self.expr(expr);
            let text = format!("({})::{}", super::print::expr_text(&inner), ty.name);
            return Node::Raw { text, ty: Type::Unknown };
        };
        // A cast applied to ARRAY[...] is pushed down to the elements (`transformArrayExpr`).
        if let (ir::Expr::Array(items), Type::Array(elem)) = (expr, &t) {
            let elems = items.iter().map(|i| coerce(self.expr(i), elem, -1, true)).collect();
            return Node::Array { elems, ty: t.clone() };
        }
        let inner = self.expr(expr);
        coerce(inner, &t, typmod, true)
    }

    pub fn function(&mut self, f: &ir::FunctionCall) -> Node {
        let name = f.name.base().to_owned();
        if let Some(n) = self.sql_value(&name, &f.args) {
            return n;
        }
        if f.over.is_some() || pg::is_aggregate(&name) {
            return self.aggregate(&name, f);
        }
        match name.as_str() {
            "coalesce" | "greatest" | "least" => {
                let args: Vec<Node> = f.args.iter().map(|a| self.expr(a)).collect();
                let ty = pg::common_type(&args.iter().map(Node::ty).collect::<Vec<_>>());
                let args = args.into_iter().map(|a| implicit(a, &ty)).collect();
                match name.as_str() {
                    "coalesce" => Node::Coalesce { args, ty },
                    g => Node::MinMax { greatest: g == "greatest", args, ty },
                }
            }
            "nullif" if f.args.len() == 2 => {
                let (l, r) = (self.expr(&f.args[0]), self.expr(&f.args[1]));
                let (lt, rt, _) = pg::resolve_binary("=", &l.ty(), &r.ty());
                Node::NullIf { left: Box::new(implicit(l, &lt)), right: Box::new(implicit(r, &rt)), ty: lt }
            }
            _ => self.plain_function(&name, f),
        }
    }

    fn sql_value(&mut self, name: &str, args: &[ir::Expr]) -> Option<Node> {
        let (keyword, ty) = match name {
            "current_timestamp" => ("CURRENT_TIMESTAMP", Type::Timestamptz),
            "current_date" => ("CURRENT_DATE", Type::Date),
            "current_time" => ("CURRENT_TIME", Type::TimeTz),
            "localtimestamp" => ("LOCALTIMESTAMP", Type::Timestamp),
            "localtime" => ("LOCALTIME", Type::Time),
            "current_user" | "user" | "current_role" => ("CURRENT_USER", Type::Name),
            "session_user" => ("SESSION_USER", Type::Name),
            "current_catalog" => ("CURRENT_CATALOG", Type::Name),
            "current_schema" => ("CURRENT_SCHEMA", Type::Name),
            _ => return None,
        };
        let precision = match args {
            [] => None,
            [ir::Expr::Literal(ir::Literal::Integer(p))] if ty != Type::Name => p.parse().ok(),
            _ => return None,
        };
        Some(Node::SqlValue { keyword, precision, ty })
    }

    fn plain_function(&mut self, name: &str, f: &ir::FunctionCall) -> Node {
        let args: Vec<Node> = f.args.iter().map(|a| self.expr(a)).collect();
        let types: Vec<Type> = args.iter().map(Node::ty).collect();
        let (targets, ret) = function_signature(name, &types);
        let args: Vec<Node> =
            args.into_iter().zip(targets.iter().chain(std::iter::repeat(&Type::Unknown))).map(|(a, t)| implicit(a, t)).collect();
        let syntax = match (name, args.len()) {
            ("extract", 2) if matches!(&args[0], Node::Const { ty: Type::Text, value: Some(_), .. }) => Syntax::Extract,
            ("position", 2) => Syntax::Position,
            ("substring", 2 | 3) => Syntax::Substring,
            ("btrim", 1 | 2) => Syntax::Trim("BOTH"),
            ("overlay", 3 | 4) => Syntax::Overlay,
            _ => Syntax::Call,
        };
        Node::Func { name: name.to_owned(), args, ty: ret, syntax, variadic: f.variadic }
    }

    fn aggregate(&mut self, name: &str, f: &ir::FunctionCall) -> Node {
        let args: Vec<Node> = f.args.iter().map(|a| self.expr(a)).collect();
        let types: Vec<Type> = args.iter().map(Node::ty).collect();
        let (targets, ty) = pg::window_function(name, &types).unwrap_or_else(|| pg::aggregate(name, &types));
        let args = args.into_iter().zip(targets.iter().chain(std::iter::repeat(&Type::Unknown))).map(|(a, t)| implicit(a, t)).collect();
        let order_by = f.order_by.iter().map(|o| self.sort(o)).collect();
        let within_group = f.within_group.iter().map(|o| self.sort(o)).collect();
        let filter = f.filter.as_ref().map(|fl| self.boolean(fl));
        let over = f.over.as_ref().map(|spec| {
            match (&spec.base, spec.partition_by.is_empty() && spec.order_by.is_empty() && spec.frame.is_none()) {
                (Some(name), true) => Err(name.clone()),
                _ => Ok(self.window_spec(spec)),
            }
        });
        let ty = if name == "count" { Type::Int8 } else { ty };
        Node::Agg(Box::new(AggCall {
            name: name.to_owned(),
            args,
            star: f.star,
            distinct: f.distinct,
            order_by,
            within_group,
            filter,
            over,
            ty,
        }))
    }

    pub fn sort(&mut self, o: &ir::OrderByExpr) -> Sort {
        Sort { expr: self.expr(&o.expr), desc: o.desc, nulls_first: o.nulls_first }
    }

    pub fn window_spec(&mut self, spec: &ir::WindowSpec) -> WinSpec {
        let partition_by = spec.partition_by.iter().map(|p| self.expr(p)).collect();
        let order_by = spec.order_by.iter().map(|o| self.sort(o)).collect();
        let frame = spec.frame.as_ref().map(|fr| {
            let mut bound = |b: &ir::FrameBound| match b {
                ir::FrameBound::UnboundedPreceding => Bound::UnboundedPreceding,
                ir::FrameBound::Preceding(e) => Bound::Preceding(self.frame_offset(fr.units, e)),
                ir::FrameBound::CurrentRow => Bound::CurrentRow,
                ir::FrameBound::Following(e) => Bound::Following(self.frame_offset(fr.units, e)),
                ir::FrameBound::UnboundedFollowing => Bound::UnboundedFollowing,
            };
            Frame { units: fr.units, start: bound(&fr.start), end: bound(&fr.end), exclude: fr.exclude }
        });
        WinSpec { base: spec.base.clone(), partition_by, order_by, frame }
    }

    fn frame_offset(&mut self, units: ir::FrameUnits, e: &ir::Expr) -> Node {
        let n = self.expr(e);
        if units == ir::FrameUnits::Range { n } else { implicit(n, &Type::Int8) }
    }
}

/// Argument coercion targets and result type of a plain function call.
fn function_signature(name: &str, types: &[Type]) -> (Vec<Type>, Type) {
    if let Some(sig) = pg::known_function(name, types) {
        return sig;
    }
    if let Ok(Some(r)) = crate::funcs::resolve_function(name, types)
        && r.args.len() == types.len()
    {
        // The executor's resolver widens unknown to its own choice; polymorphic arguments
        // (anyarray, anyelement) keep the argument's type.
        let args = r
            .args
            .iter()
            .zip(types)
            .map(|(target, actual)| if is_polymorphic_match(target, actual) { actual.clone() } else { target.clone() })
            .collect();
        return (args, r.ret);
    }
    (types.to_vec(), types.first().cloned().filter(|t| *t != Type::Unknown).unwrap_or(Type::Text))
}

fn is_polymorphic_match(target: &Type, actual: &Type) -> bool {
    matches!((target, actual), (Type::Array(_), Type::Array(_))) && target != actual && !actual.element().is_some_and(Type::is_string)
}

fn literal(l: &ir::Literal) -> Node {
    let c = |ty: Type, v: String| Node::Const { ty, typmod: -1, value: Some(v) };
    match l {
        ir::Literal::Null => Node::null(Type::Unknown),
        ir::Literal::Bool(b) => c(Type::Bool, if *b { "t" } else { "f" }.into()),
        ir::Literal::Integer(s) => match s.parse::<i64>() {
            Ok(n) => match i32::try_from(n) {
                Ok(n) => c(Type::Int4, n.to_string()),
                Err(_) => c(Type::Int8, n.to_string()),
            },
            Err(_) => c(Type::Numeric, Numeric::parse(s).map_or_else(|_| s.clone(), |n| n.to_string())),
        },
        ir::Literal::Decimal(s) => c(Type::Numeric, Numeric::parse(s).map_or_else(|_| s.clone(), |n| n.to_string())),
        ir::Literal::String(s) => c(Type::Unknown, s.clone()),
    }
}

fn typed_string(catalog: &Catalog, ty: &ir::TypeName, value: &str) -> Node {
    match resolve_type_name(catalog, ty) {
        Ok((t, typmod)) => coerce(Node::Const { ty: Type::Unknown, typmod: -1, value: Some(value.to_owned()) }, &t, typmod, true),
        Err(_) => Node::Raw { text: format!("{}::{}", pg::quote_literal(value), ty.name), ty: Type::Unknown },
    }
}

/// `make_op`: resolves the operator and coerces both operands to its argument types.
pub(super) fn make_op(op: &str, l: Node, r: Node) -> Node {
    let op = if op == "!=" { "<>" } else { op };
    let (lt, rt, ret) = pg::resolve_binary(op, &l.ty(), &r.ty());
    Node::Op { op: op.to_owned(), left: Some(Box::new(implicit(l, &lt))), right: Box::new(implicit(r, &rt)), ty: ret }
}

/// `make_scalar_array_op`: `left op ANY|ALL (array)`.
fn scalar_array_op(op: &str, any: bool, left: Node, array: Node) -> Node {
    let op = if op == "!=" { "<>" } else { op };
    let at = array.ty();
    let elem = match &at {
        Type::Array(e) => (**e).clone(),
        _ => left.ty(),
    };
    let (lt, rt, _) = pg::resolve_binary(op, &left.ty(), &elem);
    let rt = if rt == Type::Unknown { Type::Text } else { rt };
    Node::ScalarArrayOp {
        op: op.to_owned(),
        any,
        left: Box::new(implicit(left, &lt)),
        right: Box::new(implicit(array, &Type::array_of(rt))),
    }
}
