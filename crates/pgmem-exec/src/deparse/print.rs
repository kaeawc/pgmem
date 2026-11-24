//! The printer: a transcription of `ruleutils.c`'s `get_rule_expr` / `get_query_def` for the
//! `PRETTYFLAG_INDENT` mode that `pg_get_expr`, `pg_get_constraintdef`, `pg_get_indexdef`
//! and `pg_get_viewdef` use (indentation and line breaks on, minimal parentheses off).

use super::node::{AggCall, Bound, Frame, Node, Sort, SubLinkKind, Syntax, WinSpec};
use super::pg::{format_type, quote_ident, quote_literal};
use super::query::{Body, DistinctKind, FromItem, Query, Select, SetTree, Target};
use pgmem_ir as ir;
use pgmem_types::Type;

const INDENT_STD: i32 = 8;
const INDENT_JOIN: i32 = 4;
const INDENT_VAR: i32 = 4;
const INDENT_LIMIT: i32 = 40;

#[derive(Default)]
pub(super) struct Printer {
    pub buf: String,
    indent: i32,
    varprefix: bool,
}

/// Prints an expression on its own (top level: implicit coercions hidden).
pub(super) fn expr_text(n: &Node) -> String {
    let mut p = Printer::default();
    p.expr(n, false);
    p.buf
}

/// `get_const_expr`. `showtype` -1: never label; 0: label unless the literal reads back as
/// the right type; 1: always label.
fn const_text(ty: &Type, typmod: i32, value: Option<&str>, showtype: i32) -> String {
    let label = |s: String| format!("{s}::{}", format_type(ty, typmod));
    let Some(v) = value else {
        return if showtype >= 0 { label("NULL".into()) } else { "NULL".into() };
    };
    let (text, mut needlabel) = match ty {
        Type::Int4 if !v.starts_with('-') => (v.to_owned(), false),
        Type::Int4 => (quote_literal(v), true),
        Type::Numeric if v.starts_with(|c: char| c.is_ascii_digit()) && v.contains(['e', 'E', '.']) => (v.to_owned(), false),
        Type::Numeric => (quote_literal(v), true),
        Type::Bool => ((if v == "t" || v == "true" { "true" } else { "false" }).to_owned(), false),
        _ => (quote_literal(v), true),
    };
    if showtype < 0 {
        return text;
    }
    match ty {
        Type::Bool | Type::Unknown => needlabel = false,
        Type::Int4 => {}
        Type::Numeric => needlabel |= typmod >= 0,
        _ => needlabel = true,
    }
    if needlabel || showtype > 0 { label(text) } else { text }
}

/// `looks_like_function`: index keys that need no extra parentheses.
pub(super) fn looks_like_function(n: &Node) -> bool {
    matches!(n, Node::Func { .. } | Node::Coalesce { .. } | Node::MinMax { .. } | Node::NullIf { .. } | Node::SqlValue { .. })
}

impl Printer {
    pub fn new(varprefix: bool) -> Printer {
        Printer { buf: String::new(), indent: 0, varprefix }
    }

    fn push(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    fn trim_trailing_spaces(&mut self) {
        let len = self.buf.trim_end_matches(' ').len();
        self.buf.truncate(len);
    }

    /// `appendContextKeyword`.
    fn keyword(&mut self, s: &str, before: i32, after: i32, plus: i32) {
        self.indent += before;
        self.trim_trailing_spaces();
        self.buf.push('\n');
        let amount = if self.indent < INDENT_LIMIT {
            self.indent.max(0) + plus
        } else {
            (INDENT_LIMIT + (self.indent - INDENT_LIMIT) / (INDENT_STD / 2)) % INDENT_LIMIT + plus
        };
        self.buf.extend(std::iter::repeat_n(' ', usize::try_from(amount).unwrap_or(0)));
        self.buf.push_str(s);
        self.indent = (self.indent + after).max(0);
    }

    /// Runs `f` against an empty buffer and returns what it printed.
    fn capture(&mut self, f: impl FnOnce(&mut Self)) -> String {
        let saved = std::mem::take(&mut self.buf);
        f(self);
        std::mem::replace(&mut self.buf, saved)
    }

    fn list(&mut self, items: &[Node], showimplicit: bool) {
        for (i, n) in items.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.expr(n, showimplicit);
        }
    }

    /// `get_rule_expr`.
    pub fn expr(&mut self, n: &Node, showimplicit: bool) {
        match n {
            Node::Const { ty, typmod, value } => {
                let s = const_text(ty, *typmod, value.as_deref(), 0);
                self.push(&s);
            }
            Node::Var { refname, name, .. } => {
                if self.varprefix && !refname.is_empty() {
                    self.push(&quote_ident(refname));
                    self.push(".");
                }
                self.push(&quote_ident(name));
            }
            Node::Param { n, .. } => self.push(&format!("${n}")),
            Node::Op { op, left, right, .. } => {
                self.push("(");
                if let Some(l) = left {
                    self.expr(l, true);
                    self.push(&format!(" {op} "));
                } else {
                    self.push(&format!("{op} "));
                }
                self.expr(right, true);
                self.push(")");
            }
            Node::Func { name, args, syntax, variadic, .. } => self.func(name, args, *syntax, *variadic),
            Node::Coerce { arg, ty, typmod, implicit } => {
                if *implicit && !showimplicit {
                    self.expr(arg, false);
                } else {
                    self.coercion(arg, ty, *typmod);
                }
            }
            Node::And(args) | Node::Or(args) => {
                let sep = if matches!(n, Node::And(_)) { " AND " } else { " OR " };
                self.push("(");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.push(sep);
                    }
                    self.expr(a, false);
                }
                self.push(")");
            }
            Node::Not(arg) => {
                self.push("(NOT ");
                self.expr(arg, false);
                self.push(")");
            }
            _ => self.expr2(n),
        }
    }

    fn expr2(&mut self, n: &Node) {
        match n {
            Node::NullTest { arg, negated } => {
                self.push("(");
                self.expr(arg, true);
                self.push(if *negated { " IS NOT NULL)" } else { " IS NULL)" });
            }
            Node::BoolTest { arg, test } => {
                self.push("(");
                self.expr(arg, false);
                self.push(&format!(" {test})"));
            }
            Node::Distinct { left, right } => {
                self.push("(");
                self.expr(left, true);
                self.push(" IS DISTINCT FROM ");
                self.expr(right, true);
                self.push(")");
            }
            Node::ScalarArrayOp { op, any, left, right } => {
                self.push("(");
                self.expr(left, true);
                self.push(&format!(" {op} {} (", if *any { "ANY" } else { "ALL" }));
                self.expr(right, true);
                if let Node::SubLink { kind: SubLinkKind::Expr, ty, .. } = right.as_ref() {
                    self.push(&format!("::{}", format_type(ty, -1)));
                }
                self.push("))");
            }
            Node::Case { arg, whens, default, .. } => self.case(arg.as_deref(), whens, default),
            Node::Coalesce { args, .. } => {
                self.push("COALESCE(");
                self.list(args, true);
                self.push(")");
            }
            Node::MinMax { greatest, args, .. } => {
                self.push(if *greatest { "GREATEST(" } else { "LEAST(" });
                self.list(args, true);
                self.push(")");
            }
            Node::NullIf { left, right, .. } => {
                self.push("NULLIF(");
                self.expr(left, true);
                self.push(", ");
                self.expr(right, true);
                self.push(")");
            }
            Node::Array { elems, ty } => {
                self.push("ARRAY[");
                self.list(elems, true);
                self.push("]");
                if elems.is_empty() {
                    self.push(&format!("::{}", format_type(ty, -1)));
                }
            }
            _ => self.expr3(n),
        }
    }

    fn expr3(&mut self, n: &Node) {
        match n {
            Node::Row(items) => {
                self.push("ROW(");
                self.list(items, true);
                self.push(")");
            }
            Node::SubLink { kind, test, op, query, .. } => self.sublink(*kind, test.as_deref(), op, query),
            Node::SqlValue { keyword, precision, .. } => {
                self.push(keyword);
                if let Some(p) = precision {
                    self.push(&format!("({p})"));
                }
            }
            Node::Agg(a) => self.agg(a),
            Node::Subscript { arg, lower, upper, slice, .. } => {
                let parens = !matches!(arg.as_ref(), Node::Var { .. });
                if parens {
                    self.push("(");
                }
                self.expr(arg, true);
                if parens {
                    self.push(")");
                }
                self.push("[");
                if *slice {
                    if let Some(l) = lower {
                        self.expr(l, false);
                    }
                    self.push(":");
                }
                if let Some(u) = upper {
                    self.expr(u, false);
                }
                self.push("]");
            }
            Node::Collate { arg, collation } => {
                self.push("(");
                self.expr(arg, true);
                self.push(&format!(" COLLATE {})", quote_ident(collation)));
            }
            Node::Raw { text, .. } => self.push(text),
            _ => {}
        }
    }

    /// `get_coercion_expr`: `(arg)::type`, or `literal::type` for an undecorated constant.
    fn coercion(&mut self, arg: &Node, ty: &Type, typmod: i32) {
        match arg {
            Node::Const { ty: cty, typmod: -1, value } if cty == ty => {
                let s = const_text(cty, -1, value.as_deref(), -1);
                self.push(&s);
            }
            _ => {
                self.push("(");
                self.expr(arg, false);
                self.push(")");
            }
        }
        self.push(&format!("::{}", format_type(ty, typmod)));
    }

    fn func(&mut self, name: &str, args: &[Node], syntax: Syntax, variadic: bool) {
        match (syntax, args) {
            (Syntax::Extract, [Node::Const { value: Some(field), .. }, src]) => {
                self.push(&format!("EXTRACT({} FROM ", quote_ident(field)));
                self.expr(src, false);
                self.push(")");
            }
            (Syntax::Position, [s, sub]) => {
                self.push("POSITION((");
                self.expr(sub, false);
                self.push(") IN (");
                self.expr(s, false);
                self.push("))");
            }
            (Syntax::Substring, [s, from, rest @ ..]) => {
                self.push("SUBSTRING(");
                self.expr(s, false);
                self.push(" FROM ");
                self.expr(from, false);
                if let [len] = rest {
                    self.push(" FOR ");
                    self.expr(len, false);
                }
                self.push(")");
            }
            (Syntax::Trim(how), [s, chars @ ..]) => {
                self.push(&format!("TRIM({how}"));
                if let [c] = chars {
                    self.push(" ");
                    self.expr(c, false);
                }
                self.push(" FROM ");
                self.expr(s, false);
                self.push(")");
            }
            (Syntax::Overlay, [s, placing, from, rest @ ..]) => {
                self.push("OVERLAY(");
                self.expr(s, false);
                self.push(" PLACING ");
                self.expr(placing, false);
                self.push(" FROM ");
                self.expr(from, false);
                if let [len] = rest {
                    self.push(" FOR ");
                    self.expr(len, false);
                }
                self.push(")");
            }
            (Syntax::AtTimeZone, [zone, ts]) => {
                self.push("(");
                self.expr(ts, false);
                self.push(" AT TIME ZONE ");
                self.expr(zone, false);
                self.push(")");
            }
            _ => {
                self.push(&format!("{}(", quote_ident(name)));
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    if variadic && i + 1 == args.len() {
                        self.push("VARIADIC ");
                    }
                    self.expr(a, true);
                }
                self.push(")");
            }
        }
    }

    fn case(&mut self, arg: Option<&Node>, whens: &[(Node, Node)], default: &Node) {
        self.keyword("CASE", 0, INDENT_VAR, 0);
        if let Some(a) = arg {
            self.push(" ");
            self.expr(a, true);
        }
        for (w, t) in whens {
            self.keyword("WHEN ", 0, 0, 0);
            self.expr(w, false);
            self.push(" THEN ");
            self.expr(t, true);
        }
        self.keyword("ELSE ", 0, 0, 0);
        self.expr(default, true);
        self.keyword("END", -INDENT_VAR, 0, 0);
    }

    fn sublink(&mut self, kind: SubLinkKind, test: Option<&Node>, op: &str, query: &Query) {
        self.push(if kind == SubLinkKind::Array { "ARRAY(" } else { "(" });
        if let Some(t) = test {
            self.expr(t, true);
        }
        let need_paren = match kind {
            SubLinkKind::Exists => {
                self.push("EXISTS ");
                true
            }
            SubLinkKind::Any if op == "=" => {
                self.push(" IN ");
                true
            }
            SubLinkKind::Any => {
                self.push(&format!(" {op} ANY "));
                true
            }
            SubLinkKind::All => {
                self.push(&format!(" {op} ALL "));
                true
            }
            SubLinkKind::Expr | SubLinkKind::Array => false,
        };
        if need_paren {
            self.push("(");
        }
        let indent = self.indent;
        self.query_def(query, None, false, indent);
        self.push(if need_paren { "))" } else { ")" });
    }

    fn agg(&mut self, a: &AggCall) {
        self.push(&format!("{}({}", quote_ident(&a.name), if a.distinct { "DISTINCT " } else { "" }));
        if !a.within_group.is_empty() {
            self.list(&a.args, true);
            self.push(") WITHIN GROUP (ORDER BY ");
            self.orderby(&a.within_group);
        } else {
            if a.star {
                self.push("*");
            } else {
                self.list(&a.args, true);
            }
            if !a.order_by.is_empty() {
                self.push(" ORDER BY ");
                self.orderby(&a.order_by);
            }
        }
        if let Some(f) = &a.filter {
            self.push(") FILTER (WHERE ");
            self.expr(f, false);
        }
        self.push(")");
        match &a.over {
            None => {}
            Some(Err(name)) => self.push(&format!(" OVER {}", quote_ident(name))),
            Some(Ok(spec)) => {
                self.push(" OVER ");
                self.window_spec(spec);
            }
        }
    }

    /// `get_rule_sortgroupclause`: constants are labelled, function-like keys parenthesized.
    fn sortgroup(&mut self, n: &Node) {
        match n {
            Node::Const { ty, typmod, value } => {
                let s = const_text(ty, *typmod, value.as_deref(), 1);
                self.push(&s);
            }
            Node::Var { .. } | Node::Raw { .. } => self.expr(n, false),
            Node::Func { .. } | Node::Agg(_) => {
                self.push("(");
                self.expr(n, true);
                self.push(")");
            }
            _ => self.expr(n, true),
        }
    }

    /// `get_rule_orderby`.
    fn orderby(&mut self, items: &[Sort]) {
        for (i, s) in items.iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.sortgroup(&s.expr);
            match (s.desc, s.nulls_first) {
                (false, Some(true)) => self.push(" NULLS FIRST"),
                (false, _) => {}
                (true, Some(false)) => self.push(" DESC NULLS LAST"),
                (true, _) => self.push(" DESC"),
            }
        }
    }

    fn window_spec(&mut self, w: &WinSpec) {
        self.push("(");
        let mut need_space = false;
        if let Some(base) = &w.base {
            self.push(&quote_ident(base));
            need_space = true;
        }
        if !w.partition_by.is_empty() && w.base.is_none() {
            if need_space {
                self.push(" ");
            }
            self.push("PARTITION BY ");
            for (i, p) in w.partition_by.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.sortgroup(p);
            }
            need_space = true;
        }
        if !w.order_by.is_empty() {
            if need_space {
                self.push(" ");
            }
            self.push("ORDER BY ");
            self.orderby(&w.order_by);
            need_space = true;
        }
        if let Some(f) = &w.frame {
            if need_space {
                self.push(" ");
            }
            self.frame(f);
        }
        self.push(")");
    }

    fn frame(&mut self, f: &Frame) {
        self.push(match f.units {
            ir::FrameUnits::Rows => "ROWS ",
            ir::FrameUnits::Range => "RANGE ",
            ir::FrameUnits::Groups => "GROUPS ",
        });
        self.push("BETWEEN ");
        self.bound(&f.start);
        self.push("AND ");
        self.bound(&f.end);
        self.push(match f.exclude {
            ir::FrameExclude::NoOthers => "",
            ir::FrameExclude::CurrentRow => "EXCLUDE CURRENT ROW ",
            ir::FrameExclude::Group => "EXCLUDE GROUP ",
            ir::FrameExclude::Ties => "EXCLUDE TIES ",
        });
        self.buf.pop();
    }

    fn bound(&mut self, b: &Bound) {
        match b {
            Bound::UnboundedPreceding => self.push("UNBOUNDED PRECEDING "),
            Bound::UnboundedFollowing => self.push("UNBOUNDED FOLLOWING "),
            Bound::CurrentRow => self.push("CURRENT ROW "),
            Bound::Preceding(e) => {
                self.expr(e, false);
                self.push(" PRECEDING ");
            }
            Bound::Following(e) => {
                self.expr(e, false);
                self.push(" FOLLOWING ");
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------------------------

    /// `get_query_def`: prints `q` with its own indentation and qualification settings.
    pub fn query_def(&mut self, q: &Query, result_desc: Option<&[String]>, colnames_visible: bool, start_indent: i32) {
        let saved = (self.indent, self.varprefix);
        self.indent = start_indent;
        self.varprefix = q.varprefix;
        self.with_clause(q);
        match &q.body {
            Body::Select(s) => self.basic_select(s, result_desc, colnames_visible),
            Body::SetOp(tree) => self.setop(tree, result_desc, colnames_visible),
            Body::Values(rows) => {
                self.indent += INDENT_STD;
                self.push(" VALUES ");
                for (i, r) in rows.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.push("(");
                    for (j, n) in r.iter().enumerate() {
                        if j > 0 {
                            self.push(",");
                        }
                        self.expr(n, false);
                    }
                    self.push(")");
                }
            }
        }
        if !q.order_by.is_empty() {
            self.keyword(" ORDER BY ", -INDENT_STD, INDENT_STD, 1);
            self.orderby(&q.order_by);
        }
        if let Some(o) = &q.offset {
            self.keyword(" OFFSET ", -INDENT_STD, INDENT_STD, 0);
            self.expr(o, false);
        }
        if let Some(l) = &q.limit {
            self.keyword(" LIMIT ", -INDENT_STD, INDENT_STD, 0);
            self.expr(l, false);
        }
        (self.indent, self.varprefix) = saved;
    }

    fn with_clause(&mut self, q: &Query) {
        if q.ctes.is_empty() {
            return;
        }
        self.indent += INDENT_STD;
        self.push(" ");
        for (i, cte) in q.ctes.iter().enumerate() {
            self.push(if i > 0 {
                ", "
            } else if q.recursive {
                "WITH RECURSIVE "
            } else {
                "WITH "
            });
            self.push(&quote_ident(&cte.name));
            if !cte.colnames.is_empty() {
                let names: Vec<String> = cte.colnames.iter().map(|c| quote_ident(c)).collect();
                self.push(&format!("({})", names.join(", ")));
            }
            self.push(" AS ");
            match cte.materialized {
                Some(true) => self.push("MATERIALIZED "),
                Some(false) => self.push("NOT MATERIALIZED "),
                None => {}
            }
            self.push("(");
            self.keyword("", 0, 0, 0);
            let indent = self.indent;
            self.query_def(&cte.query, None, true, indent);
            self.keyword("", 0, 0, 0);
            self.push(")");
        }
        self.indent -= INDENT_STD;
        self.keyword("", 0, 0, 0);
    }

    fn basic_select(&mut self, s: &Select, result_desc: Option<&[String]>, colnames_visible: bool) {
        self.indent += INDENT_STD;
        self.push(" SELECT");
        match &s.distinct {
            DistinctKind::None => {}
            DistinctKind::All => self.push(" DISTINCT"),
            DistinctKind::On(exprs) => {
                self.push(" DISTINCT ON (");
                for (i, e) in exprs.iter().enumerate() {
                    if i > 0 {
                        self.push(", ");
                    }
                    self.sortgroup(e);
                }
                self.push(")");
            }
        }
        self.target_list(&s.targets, result_desc, colnames_visible);
        self.print_from_clause(&s.from);
        if let Some(w) = &s.selection {
            self.keyword(" WHERE ", -INDENT_STD, INDENT_STD, 1);
            self.expr(w, false);
        }
        if !s.group_by.is_empty() {
            self.keyword(" GROUP BY ", -INDENT_STD, INDENT_STD, 1);
            for (i, g) in s.group_by.iter().enumerate() {
                if i > 0 {
                    self.push(", ");
                }
                self.sortgroup(g);
            }
        }
        if let Some(h) = &s.having {
            self.keyword(" HAVING ", -INDENT_STD, INDENT_STD, 0);
            self.expr(h, false);
        }
        for (i, (name, spec)) in s.windows.iter().enumerate() {
            if i == 0 {
                self.keyword(" WINDOW ", -INDENT_STD, INDENT_STD, 1);
            } else {
                self.push(", ");
            }
            self.push(&format!("{} AS ", quote_ident(name)));
            self.window_spec(spec);
        }
    }

    /// `get_target_list`, including its line-wrapping rules (wrap column 0: one column per line).
    fn target_list(&mut self, targets: &[Target], result_desc: Option<&[String]>, colnames_visible: bool) {
        let mut sep = " ";
        for (i, t) in targets.iter().enumerate() {
            self.push(sep);
            sep = ", ";
            let item = self.capture(|p| {
                let attname = match &t.expr {
                    Node::Var { name, .. } => {
                        p.expr(&t.expr, false);
                        Some(name.as_str())
                    }
                    other => {
                        p.expr(other, true);
                        if colnames_visible { None } else { Some("?column?") }
                    }
                };
                let colname = result_desc.and_then(|d| d.get(i)).unwrap_or(&t.resname);
                if attname != Some(colname.as_str()) {
                    p.push(&format!(" AS {}", quote_ident(colname)));
                }
            });
            if item.starts_with('\n') {
                self.trim_trailing_spaces();
            } else if i > 0 {
                self.keyword("", -INDENT_STD, INDENT_STD, INDENT_VAR);
            }
            self.push(&item);
        }
    }

    fn print_from_clause(&mut self, from: &[FromItem]) {
        for (i, item) in from.iter().enumerate() {
            if i == 0 {
                self.keyword(" FROM ", -INDENT_STD, INDENT_STD, 2);
                self.print_from_item(item);
                continue;
            }
            self.push(", ");
            let text = self.capture(|p| p.print_from_item(item));
            if text.starts_with('\n') {
                self.trim_trailing_spaces();
            } else {
                self.keyword("", -INDENT_STD, INDENT_STD, INDENT_VAR);
            }
            self.push(&text);
        }
    }

    fn column_aliases(&mut self, names: &[String]) {
        if !names.is_empty() {
            let names: Vec<String> = names.iter().map(|c| quote_ident(c)).collect();
            self.push(&format!("({})", names.join(", ")));
        }
    }

    fn print_from_item(&mut self, item: &FromItem) {
        match item {
            FromItem::Rel { relname, refname, print_alias, colaliases } => {
                self.push(&quote_ident(relname));
                if *print_alias || !colaliases.is_empty() {
                    self.push(&format!(" {}", quote_ident(refname)));
                }
                self.column_aliases(colaliases);
            }
            FromItem::Sub { query, refname, colaliases, lateral } => {
                if *lateral {
                    self.push("LATERAL ");
                }
                self.push("(");
                let indent = self.indent;
                self.query_def(query, None, true, indent);
                self.push(&format!(") {}", quote_ident(refname)));
                self.column_aliases(colaliases);
            }
            FromItem::Func { call, refname, colnames, lateral, ordinality } => {
                if *lateral {
                    self.push("LATERAL ");
                }
                self.expr(call, true);
                if *ordinality {
                    self.push(" WITH ORDINALITY");
                }
                self.push(&format!(" {}", quote_ident(refname)));
                self.column_aliases(colnames);
            }
            FromItem::Join { left, right, kind, on, using } => {
                self.push("(");
                self.print_from_item(left);
                let kw = match kind {
                    ir::JoinKind::Inner => " JOIN ",
                    ir::JoinKind::Cross => " CROSS JOIN ",
                    ir::JoinKind::Left => " LEFT JOIN ",
                    ir::JoinKind::Right => " RIGHT JOIN ",
                    ir::JoinKind::Full => " FULL JOIN ",
                };
                self.keyword(kw, -INDENT_STD, INDENT_STD, INDENT_JOIN);
                self.print_from_item(right);
                if !using.is_empty() {
                    let names: Vec<String> = using.iter().map(|c| quote_ident(c)).collect();
                    self.push(&format!(" USING ({})", names.join(", ")));
                } else if let Some(on) = on {
                    self.push(" ON (");
                    self.expr(on, false);
                    self.push(")");
                } else if *kind != ir::JoinKind::Cross && *kind != ir::JoinKind::Inner {
                    self.push(" ON TRUE");
                }
                self.push(")");
            }
        }
    }

    /// `get_setop_query`.
    fn setop(&mut self, tree: &SetTree, result_desc: Option<&[String]>, colnames_visible: bool) {
        match tree {
            SetTree::Leaf(q) => {
                let paren = q.needs_parens_as_leaf();
                if paren {
                    self.push("(");
                }
                let indent = self.indent;
                self.query_def(q, result_desc, colnames_visible, indent);
                if paren {
                    self.push(")");
                }
            }
            SetTree::Op { op, all, left, right } => {
                let need_paren = matches!(left.as_ref(), SetTree::Op { op: lop, all: lall, .. } if lop != op || lall != all);
                let sub = if need_paren {
                    self.push("(");
                    self.keyword("", INDENT_STD, 0, 0);
                    INDENT_STD
                } else {
                    0
                };
                self.setop(left, result_desc, colnames_visible);
                if need_paren {
                    self.keyword(") ", -sub, 0, 0);
                } else {
                    self.keyword("", -sub, 0, 0);
                }
                self.push(match op {
                    ir::SetOperator::Union => "UNION ",
                    ir::SetOperator::Intersect => "INTERSECT ",
                    ir::SetOperator::Except => "EXCEPT ",
                });
                if *all {
                    self.push("ALL ");
                }
                let right_paren = matches!(right.as_ref(), SetTree::Op { .. });
                let sub = if right_paren {
                    self.push("(");
                    INDENT_STD
                } else {
                    0
                };
                self.keyword("", sub, 0, 0);
                self.setop(right, result_desc, false);
                self.indent -= sub;
                if right_paren {
                    self.keyword(")", 0, 0, 0);
                }
            }
        }
    }
}
