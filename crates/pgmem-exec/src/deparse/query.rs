//! Query analysis for view definitions: FROM scopes with Postgres' range-table names
//! (`set_rtable_names`: unique across enclosing levels), `*` expansion, GROUP BY / ORDER BY
//! references to the select list, set operations and CTEs.

use super::node::{Analyzer, CteScope, Level, Node, Rte, ScopeCol, Sort, WinSpec, coerce};
use pgmem_ir as ir;
use pgmem_types::Type;
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub(super) struct OutCol {
    pub name: String,
    pub ty: Type,
    pub typmod: i32,
}

#[derive(Clone, Debug)]
pub(super) struct Cte {
    pub name: String,
    pub colnames: Vec<String>,
    pub materialized: Option<bool>,
    pub query: Query,
}

#[derive(Clone, Debug)]
pub(super) struct Query {
    pub ctes: Vec<Cte>,
    pub recursive: bool,
    pub body: Body,
    pub order_by: Vec<Sort>,
    pub limit: Option<Node>,
    pub offset: Option<Node>,
    /// Whether column references print qualified (`ruleutils`' `varprefix`).
    pub varprefix: bool,
    pub columns: Vec<OutCol>,
}

impl Query {
    /// True when a set-operation leaf needs parentheses (it has its own WITH / ORDER BY / LIMIT).
    pub fn needs_parens_as_leaf(&self) -> bool {
        !self.ctes.is_empty()
            || !self.order_by.is_empty()
            || self.limit.is_some()
            || self.offset.is_some()
            || matches!(self.body, Body::SetOp(_))
    }
}

#[derive(Clone, Debug)]
pub(super) enum Body {
    Select(Box<Select>),
    SetOp(Box<SetTree>),
    Values(Vec<Vec<Node>>),
}

#[derive(Clone, Debug)]
pub(super) enum SetTree {
    Leaf(Box<Query>),
    Op { op: ir::SetOperator, all: bool, left: Box<SetTree>, right: Box<SetTree> },
}

#[derive(Clone, Debug)]
pub(super) enum DistinctKind {
    None,
    All,
    On(Vec<Node>),
}

#[derive(Clone, Debug)]
pub(super) struct Target {
    pub expr: Node,
    pub resname: String,
}

#[derive(Clone, Debug)]
pub(super) struct Select {
    pub distinct: DistinctKind,
    pub targets: Vec<Target>,
    pub from: Vec<FromItem>,
    pub selection: Option<Node>,
    pub group_by: Vec<Node>,
    pub having: Option<Node>,
    pub windows: Vec<(String, WinSpec)>,
}

#[derive(Clone, Debug)]
pub(super) enum FromItem {
    Rel { relname: String, refname: String, print_alias: bool, colaliases: Vec<String> },
    Sub { query: Box<Query>, refname: String, colaliases: Vec<String>, lateral: bool },
    Func { call: Node, refname: String, colnames: Vec<String>, lateral: bool, ordinality: bool },
    Join { left: Box<FromItem>, right: Box<FromItem>, kind: ir::JoinKind, on: Option<Node>, using: Vec<String> },
}

/// A FROM item's analysis result: the item, its `*` columns (rte index, column index) and the
/// number of range-table entries it adds (joins count too).
struct Built {
    item: FromItem,
    star: Vec<(usize, usize)>,
    rtes: usize,
}

fn base_refname(t: &ir::TableRef, out: &mut Vec<String>) {
    match t {
        ir::TableRef::Table { name, alias } => out.push(alias.as_ref().map_or_else(|| name.base().to_owned(), |a| a.name.clone())),
        ir::TableRef::Subquery { alias, .. } => out.push(alias.as_ref().map_or_else(|| "unnamed_subquery".to_owned(), |a| a.name.clone())),
        ir::TableRef::Function { call, alias, .. } => {
            out.push(alias.as_ref().map_or_else(|| call.name.base().to_owned(), |a| a.name.clone()))
        }
        ir::TableRef::Join { left, right, .. } => {
            base_refname(left, out);
            base_refname(right, out);
        }
    }
}

impl Analyzer<'_> {
    fn level(&mut self) -> &mut Level {
        let n = self.levels.len();
        &mut self.levels[n - 1]
    }

    /// Assigns this level's range-table names, unique across all enclosing levels.
    fn assign_names(&mut self, from: &[ir::TableRef]) -> VecDeque<String> {
        let mut bases = Vec::new();
        from.iter().for_each(|t| base_refname(t, &mut bases));
        let mut out = VecDeque::new();
        for base in bases {
            let taken = |n: &str, a: &Self| a.levels.iter().any(|l| l.names.iter().any(|x| x == n));
            let mut name = base.clone();
            let mut i = 1;
            while taken(&name, self) {
                name = format!("{base}_{i}");
                i += 1;
            }
            self.level().names.push(name.clone());
            out.push_back(name);
        }
        out
    }

    /// A subquery (sublink) of the current level.
    pub fn subquery(&mut self, q: &ir::Query) -> Query {
        self.query(q, true)
    }

    pub fn query(&mut self, q: &ir::Query, has_parent: bool) -> Query {
        self.levels.push(Level::default());
        let names = match &q.body {
            ir::SetExpr::Select(s) => self.assign_names(&s.from),
            _ => VecDeque::new(),
        };
        let (ctes, recursive) = self.ctes(q.with.as_ref());
        let mut out = match &q.body {
            ir::SetExpr::Select(s) => self.select_query(s, q, names),
            ir::SetExpr::SetOp { .. } => self.setop_query(&q.body, q),
            ir::SetExpr::Values(rows) => self.values_query(rows),
            ir::SetExpr::Query(inner) => {
                self.levels.pop();
                let mut inner = self.query(inner, has_parent);
                if !q.order_by.is_empty() || q.limit.is_some() || q.offset.is_some() {
                    inner.limit = q.limit.as_ref().map(|l| self.limit_expr(l)).or(inner.limit);
                    inner.offset = q.offset.as_ref().map(|l| self.limit_expr(l)).or(inner.offset);
                }
                return inner;
            }
        };
        out.ctes = ctes;
        out.recursive = recursive;
        out.varprefix |= has_parent;
        self.levels.pop();
        out
    }

    fn ctes(&mut self, with: Option<&ir::With>) -> (Vec<Cte>, bool) {
        let Some(with) = with else { return (Vec::new(), false) };
        let mut out = Vec::new();
        for cte in &with.ctes {
            if with.recursive {
                // The recursive term refers to the CTE itself: its columns come from the
                // non-recursive term.
                let mut first = &cte.query.body;
                while let ir::SetExpr::SetOp { left, .. } = first {
                    first = left;
                }
                let seed = self.query(&ir::Query::from_body(first.clone()), true);
                let cols = cte_cols(&seed, &cte.columns);
                self.level().ctes.push(CteScope { name: cte.name.clone(), cols });
            }
            let query = self.query(&cte.query, true);
            let cols = cte_cols(&query, &cte.columns);
            let level = self.level();
            level.ctes.retain(|c| c.name != cte.name);
            level.ctes.push(CteScope { name: cte.name.clone(), cols });
            out.push(Cte { name: cte.name.clone(), colnames: cte.columns.clone(), materialized: cte.materialized, query });
        }
        (out, with.recursive)
    }

    fn limit_expr(&mut self, e: &ir::Expr) -> Node {
        let n = self.expr(e);
        coerce(n, &Type::Int8, -1, false)
    }

    fn select_query(&mut self, s: &ir::Select, q: &ir::Query, mut names: VecDeque<String>) -> Query {
        let mut from = Vec::new();
        let mut star = Vec::new();
        let mut rtes = 0;
        for t in &s.from {
            let b = self.build_from_item(t, &mut names);
            from.push(b.item);
            star.extend(b.star);
            rtes += b.rtes;
        }
        self.level().windows = s.windows.clone();
        let targets = self.targets(&s.projection, &star);
        let selection = s.selection.as_ref().map(|w| self.boolean(w));
        let group_by = s.group_by.iter().map(|g| self.group_key(g, &targets)).collect();
        let having = s.having.as_ref().map(|h| self.boolean(h));
        let distinct = match &s.distinct {
            ir::Distinct::All => DistinctKind::None,
            ir::Distinct::Distinct => DistinctKind::All,
            ir::Distinct::On(exprs) => DistinctKind::On(exprs.iter().map(|e| self.sort_key(e, &targets)).collect()),
        };
        let windows = s.windows.iter().map(|w| (w.name.clone(), self.window_spec(&w.spec))).collect();
        let order_by =
            q.order_by.iter().map(|o| Sort { expr: self.sort_key(&o.expr, &targets), desc: o.desc, nulls_first: o.nulls_first }).collect();
        let limit = q.limit.as_ref().map(|l| self.limit_expr(l));
        let offset = q.offset.as_ref().map(|o| self.limit_expr(o));
        let columns = targets.iter().map(|t| OutCol { name: t.resname.clone(), ty: t.expr.ty(), typmod: t.expr.typmod() }).collect();
        let select = Select { distinct, targets, from, selection, group_by, having, windows };
        Query {
            ctes: Vec::new(),
            recursive: false,
            body: Body::Select(Box::new(select)),
            order_by,
            limit,
            offset,
            varprefix: rtes != 1,
            columns,
        }
    }

    fn setop_query(&mut self, body: &ir::SetExpr, q: &ir::Query) -> Query {
        let tree = self.set_tree(body);
        let mut first = &tree;
        while let SetTree::Op { left, .. } = first {
            first = left;
        }
        let columns = match first {
            SetTree::Leaf(l) => l.columns.clone(),
            SetTree::Op { .. } => Vec::new(),
        };
        let order_by = q
            .order_by
            .iter()
            .map(|o| {
                let colno = match &o.expr {
                    ir::Expr::Literal(ir::Literal::Integer(n)) => n.clone(),
                    ir::Expr::Column(parts) => {
                        columns.iter().position(|c| Some(&c.name) == parts.last()).map_or_else(|| parts.join("."), |i| (i + 1).to_string())
                    }
                    other => super::print::expr_text(&self.expr(other)),
                };
                Sort { expr: Node::Raw { text: colno, ty: Type::Unknown }, desc: o.desc, nulls_first: o.nulls_first }
            })
            .collect();
        let limit = q.limit.as_ref().map(|l| self.limit_expr(l));
        let offset = q.offset.as_ref().map(|o| self.limit_expr(o));
        Query { ctes: Vec::new(), recursive: false, body: Body::SetOp(Box::new(tree)), order_by, limit, offset, varprefix: true, columns }
    }

    fn set_tree(&mut self, body: &ir::SetExpr) -> SetTree {
        match body {
            ir::SetExpr::SetOp { op, all, left, right } => {
                SetTree::Op { op: *op, all: *all, left: Box::new(self.set_tree(left)), right: Box::new(self.set_tree(right)) }
            }
            ir::SetExpr::Query(q) => SetTree::Leaf(Box::new(self.query(q, true))),
            other => SetTree::Leaf(Box::new(self.query(&ir::Query::from_body(other.clone()), true))),
        }
    }

    fn values_query(&mut self, rows: &[Vec<ir::Expr>]) -> Query {
        let mut rows: Vec<Vec<Node>> = rows.iter().map(|r| r.iter().map(|e| self.expr(e)).collect()).collect();
        let width = rows.first().map_or(0, Vec::len);
        let mut columns = Vec::new();
        for i in 0..width {
            let types: Vec<Type> = rows.iter().filter_map(|r| r.get(i).map(Node::ty)).collect();
            let t = super::pg::common_type(&types);
            for r in &mut rows {
                if let Some(slot) = r.get_mut(i) {
                    let n = std::mem::replace(slot, Node::Raw { text: String::new(), ty: Type::Unknown });
                    *slot = coerce(n, &t, -1, false);
                }
            }
            columns.push(OutCol { name: format!("column{}", i + 1), ty: t, typmod: -1 });
        }
        Query {
            ctes: Vec::new(),
            recursive: false,
            body: Body::Values(rows),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            varprefix: false,
            columns,
        }
    }

    fn push_rte(&mut self, qualifier: String, refname: String, cols: Vec<ScopeCol>) -> (usize, Vec<(usize, usize)>) {
        let level = self.level();
        let idx = level.rtes.len();
        let star = (0..cols.len()).map(|c| (idx, c)).collect();
        level.rtes.push(Rte { qualifier, refname, cols });
        (idx, star)
    }

    fn build_from_item(&mut self, t: &ir::TableRef, names: &mut VecDeque<String>) -> Built {
        match t {
            ir::TableRef::Table { name, alias } => {
                let refname = names.pop_front().unwrap_or_default();
                let relname = name.base().to_owned();
                let mut cols = self.relation_columns(name);
                let colaliases = alias.as_ref().map(|a| a.columns.clone()).unwrap_or_default();
                rename(&mut cols, &colaliases);
                let qualifier = alias.as_ref().map_or_else(|| relname.clone(), |a| a.name.clone());
                let print_alias = alias.is_some() || refname != relname;
                let (_, star) = self.push_rte(qualifier, refname.clone(), cols);
                Built { item: FromItem::Rel { relname, refname, print_alias, colaliases }, star, rtes: 1 }
            }
            ir::TableRef::Subquery { query, alias, lateral } => {
                let refname = names.pop_front().unwrap_or_default();
                let q = self.query(query, true);
                let mut cols: Vec<ScopeCol> = q
                    .columns
                    .iter()
                    .map(|c| ScopeCol { name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod, hidden: false })
                    .collect();
                let aliases = alias.as_ref().map(|a| a.columns.clone()).unwrap_or_default();
                let changed = aliases.iter().zip(&cols).any(|(a, c)| *a != c.name);
                rename(&mut cols, &aliases);
                let colaliases = if changed { cols.iter().map(|c| c.name.clone()).collect() } else { Vec::new() };
                let qualifier = alias.as_ref().map_or_else(|| refname.clone(), |a| a.name.clone());
                let (_, star) = self.push_rte(qualifier, refname.clone(), cols);
                Built { item: FromItem::Sub { query: Box::new(q), refname, colaliases, lateral: *lateral }, star, rtes: 1 }
            }
            ir::TableRef::Function { call, alias, lateral, with_ordinality } => {
                let refname = names.pop_front().unwrap_or_default();
                let node = self.function(call);
                let ty = match node.ty() {
                    // A set-returning function over an array yields its elements.
                    Type::Array(e) if call.name.base() == "unnest" => *e,
                    t => t,
                };
                let mut colnames = alias.as_ref().map(|a| a.columns.clone()).unwrap_or_default();
                if colnames.is_empty() {
                    colnames.push(alias.as_ref().map_or_else(|| call.name.base().to_owned(), |a| a.name.clone()));
                }
                let mut cols: Vec<ScopeCol> =
                    colnames.iter().map(|n| ScopeCol { name: n.clone(), ty: ty.clone(), typmod: -1, hidden: false }).collect();
                if *with_ordinality {
                    cols.push(ScopeCol { name: "ordinality".into(), ty: Type::Int8, typmod: -1, hidden: false });
                    colnames.push("ordinality".into());
                }
                let qualifier = alias.as_ref().map_or_else(|| refname.clone(), |a| a.name.clone());
                let (_, star) = self.push_rte(qualifier, refname.clone(), cols);
                let item = FromItem::Func { call: node, refname, colnames, lateral: *lateral, ordinality: *with_ordinality };
                Built { item, star, rtes: 1 }
            }
            ir::TableRef::Join { left, right, kind, constraint } => self.join(left, right, *kind, constraint, names),
        }
    }

    fn join(
        &mut self,
        left: &ir::TableRef,
        right: &ir::TableRef,
        kind: ir::JoinKind,
        c: &ir::JoinConstraint,
        names: &mut VecDeque<String>,
    ) -> Built {
        let l = self.build_from_item(left, names);
        let first_right = self.level().rtes.len();
        let r = self.build_from_item(right, names);
        let col_name = |a: &Self, (rte, col): (usize, usize)| a.level_ref().rtes[rte].cols[col].name.clone();
        let using: Vec<String> = match c {
            ir::JoinConstraint::Using(cols) => cols.clone(),
            ir::JoinConstraint::Natural => {
                let right_names: Vec<String> = r.star.iter().map(|&s| col_name(self, s)).collect();
                l.star.iter().map(|&s| col_name(self, s)).filter(|n| right_names.contains(n)).collect()
            }
            _ => Vec::new(),
        };
        // The right-hand copies of USING columns are only addressable qualified.
        let level = self.level();
        for rte in &mut level.rtes[first_right..] {
            for col in &mut rte.cols {
                if using.contains(&col.name) {
                    col.hidden = true;
                }
            }
        }
        let mut star: Vec<(usize, usize)> = Vec::new();
        for u in &using {
            if let Some(&s) = l.star.iter().find(|&&s| col_name(self, s) == *u) {
                star.push(s);
            }
        }
        star.extend(l.star.iter().copied().filter(|&s| !using.contains(&col_name(self, s))));
        star.extend(r.star.iter().copied().filter(|&s| !using.contains(&col_name(self, s))));
        let on = match c {
            ir::JoinConstraint::On(e) => Some(self.boolean(e)),
            _ => None,
        };
        let kind = if kind == ir::JoinKind::Inner && on.is_none() && using.is_empty() { ir::JoinKind::Cross } else { kind };
        let item = FromItem::Join { left: Box::new(l.item), right: Box::new(r.item), kind, on, using };
        Built { item, star, rtes: l.rtes + r.rtes + 1 }
    }

    fn level_ref(&self) -> &Level {
        &self.levels[self.levels.len() - 1]
    }

    /// Columns of a relation named in FROM: a CTE, a table or a view.
    fn relation_columns(&self, name: &ir::ObjectName) -> Vec<ScopeCol> {
        if name.0.len() == 1
            && let Some(cte) = self.levels.iter().rev().find_map(|l| l.ctes.iter().find(|c| c.name == name.base()))
        {
            return cte.cols.clone();
        }
        let base = name.base();
        if let Some(t) = self.catalog.table(base) {
            return t
                .live_columns()
                .map(|(_, c)| ScopeCol { name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod, hidden: false })
                .collect();
        }
        if let Some(v) = self.catalog.view(base) {
            let binder = crate::bind::Binder::new(self.catalog, &[]);
            let top = crate::bind::Level::new(crate::bind::Scope::default());
            let types = binder.bind_query(&v.query, &crate::bind::Ctx::new(&top)).map(|b| b.columns).unwrap_or_default();
            return v
                .columns
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let (ty, typmod) = types.get(i).map_or((Type::Unknown, -1), |c| (c.ty.clone(), c.typmod));
                    ScopeCol { name: n.clone(), ty, typmod, hidden: false }
                })
                .collect();
        }
        Vec::new()
    }

    fn targets(&mut self, items: &[ir::SelectItem], star: &[(usize, usize)]) -> Vec<Target> {
        let mut out = Vec::new();
        for item in items {
            match item {
                ir::SelectItem::Wildcard => {
                    for &(rte, col) in star {
                        out.push(self.var_target(rte, col));
                    }
                }
                ir::SelectItem::QualifiedWildcard(name) => {
                    let level = self.level_ref();
                    if let Some(rte) = level.rtes.iter().position(|r| r.qualifier == name.base()) {
                        for col in 0..level.rtes[rte].cols.len() {
                            out.push(self.var_target(rte, col));
                        }
                    }
                }
                ir::SelectItem::FieldStar(expr) => {
                    let inner = self.expr(expr);
                    let text = format!("({}).*", super::print::expr_text(&inner));
                    out.push(Target { expr: Node::Raw { text, ty: Type::Unknown }, resname: "?column?".into() });
                }
                ir::SelectItem::Expr { expr, alias } => {
                    let n = self.expr(expr);
                    // Unknown-typed outputs resolve to text (`resolveTargetListUnknowns`).
                    let n = if n.ty() == Type::Unknown && matches!(n, Node::Const { .. }) { coerce(n, &Type::Text, -1, false) } else { n };
                    let resname = alias.clone().unwrap_or_else(|| crate::bind::output_name(expr));
                    out.push(Target { expr: n, resname });
                }
            }
        }
        out
    }

    fn var_target(&self, rte: usize, col: usize) -> Target {
        let r = &self.level_ref().rtes[rte];
        let c = &r.cols[col];
        Target {
            expr: Node::Var { refname: r.refname.clone(), name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod },
            resname: c.name.clone(),
        }
    }

    fn resolvable_in_from(&self, name: &str) -> bool {
        self.level_ref().rtes.iter().any(|r| r.cols.iter().any(|c| c.name == name && !c.hidden))
    }

    /// A GROUP BY item: a select-list position, an input column, or an output column name.
    fn group_key(&mut self, e: &ir::Expr, targets: &[Target]) -> Node {
        match e {
            ir::Expr::Literal(ir::Literal::Integer(n)) => {
                if let Some(t) = n.parse::<usize>().ok().and_then(|i| targets.get(i.wrapping_sub(1))) {
                    return t.expr.clone();
                }
            }
            ir::Expr::Column(parts) if parts.len() == 1 && !self.resolvable_in_from(&parts[0]) => {
                if let Some(t) = targets.iter().find(|t| t.resname == parts[0]) {
                    return t.expr.clone();
                }
            }
            _ => {}
        }
        self.expr(e)
    }

    /// An ORDER BY / DISTINCT ON item: a select-list position or output name, else an expression.
    fn sort_key(&mut self, e: &ir::Expr, targets: &[Target]) -> Node {
        match e {
            ir::Expr::Literal(ir::Literal::Integer(n)) => {
                if let Some(t) = n.parse::<usize>().ok().and_then(|i| targets.get(i.wrapping_sub(1))) {
                    return t.expr.clone();
                }
            }
            ir::Expr::Column(parts) if parts.len() == 1 => {
                if let Some(t) = targets.iter().find(|t| t.resname == parts[0]) {
                    return t.expr.clone();
                }
            }
            _ => {}
        }
        self.expr(e)
    }
}

fn rename(cols: &mut [ScopeCol], aliases: &[String]) {
    for (c, a) in cols.iter_mut().zip(aliases) {
        c.name = a.clone();
    }
}

fn cte_cols(q: &Query, aliases: &[String]) -> Vec<ScopeCol> {
    let mut cols: Vec<ScopeCol> =
        q.columns.iter().map(|c| ScopeCol { name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod, hidden: false }).collect();
    rename(&mut cols, aliases);
    cols
}
