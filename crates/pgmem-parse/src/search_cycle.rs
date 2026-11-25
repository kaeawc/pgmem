//! `WITH RECURSIVE ... SEARCH {DEPTH | BREADTH} FIRST BY ... SET ...` and `CYCLE ... SET ...
//! [TO ... DEFAULT ...] USING ...`, which sqlparser does not parse. The token rewrite strips
//! the clauses (remembering them per CTE name); the lowering then applies Postgres' own
//! rewrite (`rewriteSearchAndCycle`): extra columns computed in both UNION terms, and for
//! CYCLE a recursive-term condition that stops at a repeated row.

use crate::expr::lower_expr;
use pgmem_ir::{AnyAllRight, Cte, Expr, FunctionCall, Literal, SelectItem, SetExpr, SetOperator, TableRef, TypeName};
use pgmem_types::{SqlError, SqlResult, code};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::Token;
use std::cell::RefCell;

#[derive(Clone, Debug)]
pub(crate) enum Clause {
    Search { breadth: bool, cols: Vec<String>, set: String },
    Cycle { cols: Vec<String>, set: String, to: Option<String>, default: Option<String>, using: String },
}

thread_local! {
    /// Clauses stripped by the rewrite of the statement being parsed, by CTE name.
    static CLAUSES: RefCell<Vec<(String, Clause)>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn clear() {
    CLAUSES.with(|c| c.borrow_mut().clear());
}

fn take(name: &str) -> Vec<Clause> {
    CLAUSES.with(|c| {
        let mut all = c.borrow_mut();
        let (mine, rest): (Vec<_>, Vec<_>) = all.drain(..).partition(|(n, _)| n == name);
        *all = rest;
        mine.into_iter().map(|(_, c)| c).collect()
    })
}

fn fold(t: &Token) -> Option<String> {
    match t {
        Token::Word(w) if w.quote_style.is_some() => Some(w.value.clone()),
        Token::Word(w) => Some(w.value.to_lowercase()),
        _ => None,
    }
}

/// At `sig[k]` (a SEARCH or CYCLE word after a CTE body), parses the clause; returns the
/// number of significant tokens it spans. `name` is the CTE's name.
pub(crate) fn strip(tokens: &[Token], sig: &[usize], k: usize, name: &str) -> Option<usize> {
    let word = |o: usize, w: &str| {
        sig.get(k + o).is_some_and(|&j| matches!(&tokens[j], Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w)))
    };
    let tok = |o: usize| sig.get(k + o).map(|&j| &tokens[j]);
    // `ident [, ident]*` from offset `o`: the names and the offset after them.
    let idents = |mut o: usize| -> Option<(Vec<String>, usize)> {
        let mut out = vec![fold(tok(o)?)?];
        o += 1;
        while tok(o) == Some(&Token::Comma) {
            out.push(fold(tok(o + 1)?)?);
            o += 2;
        }
        Some((out, o))
    };
    let (clause, len) = if word(0, "SEARCH") && (word(1, "DEPTH") || word(1, "BREADTH")) && word(2, "FIRST") && word(3, "BY") {
        let (cols, o) = idents(4)?;
        if !word(o, "SET") {
            return None;
        }
        (Clause::Search { breadth: word(1, "BREADTH"), cols, set: fold(tok(o + 1)?)? }, o + 2)
    } else if word(0, "CYCLE") {
        let (cols, mut o) = idents(1)?;
        if !word(o, "SET") {
            return None;
        }
        let set = fold(tok(o + 1)?)?;
        o += 2;
        let (mut to, mut default) = (None, None);
        if word(o, "TO") && word(o + 2, "DEFAULT") {
            to = Some(tok(o + 1)?.to_string());
            default = Some(tok(o + 3)?.to_string());
            o += 4;
        }
        if !word(o, "USING") {
            return None;
        }
        (Clause::Cycle { cols, set, to, default, using: fold(tok(o + 1)?)? }, o + 2)
    } else {
        return None;
    };
    CLAUSES.with(|c| c.borrow_mut().push((name.to_owned(), clause)));
    Some(len)
}

fn column(q: &str, c: &str) -> Expr {
    Expr::Column(vec![q.to_owned(), c.to_owned()])
}

fn literal(text: Option<&str>, fallback: bool) -> SqlResult<Expr> {
    let Some(text) = text else { return Ok(Expr::Literal(Literal::Bool(fallback))) };
    let e = sqlparser::parser::Parser::new(&PostgreSqlDialect {})
        .try_with_sql(text)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| SqlError::syntax(e.to_string()))?;
    lower_expr(e)
}

/// Where the recursive term reads the CTE: its alias (or name).
fn self_reference(from: &[TableRef], name: &str) -> Option<String> {
    fn find(t: &TableRef, name: &str) -> Option<String> {
        match t {
            TableRef::Table { name: n, alias } if n.0.len() == 1 && n.base() == name => {
                Some(alias.as_ref().map_or_else(|| name.to_owned(), |a| a.name.clone()))
            }
            TableRef::Join { left, right, .. } => find(left, name).or_else(|| find(right, name)),
            _ => None,
        }
    }
    from.iter().find_map(|t| find(t, name))
}

fn projection_expr(items: &[SelectItem], i: usize) -> SqlResult<Expr> {
    match items.get(i) {
        Some(SelectItem::Expr { expr, .. }) => Ok(expr.clone()),
        _ => Err(SqlError::feature_not_supported("SEARCH / CYCLE over a * select list is not supported by pgmem")),
    }
}

fn output_names(columns: &[String], items: &[SelectItem]) -> Vec<String> {
    if !columns.is_empty() {
        return columns.to_vec();
    }
    items
        .iter()
        .map(|it| match it {
            SelectItem::Expr { alias: Some(a), .. } => a.clone(),
            SelectItem::Expr { expr: Expr::Column(parts), .. } => parts.last().cloned().unwrap_or_default(),
            _ => String::new(),
        })
        .collect()
}

/// Applies the SEARCH / CYCLE clauses recorded for `cte`.
pub(crate) fn apply(cte: &mut Cte) -> SqlResult<()> {
    let clauses = take(&cte.name);
    if clauses.is_empty() {
        return Ok(());
    }
    let name = cte.name.clone();
    let columns = cte.columns.clone();
    let SetExpr::SetOp { op: SetOperator::Union, left, right, .. } = &mut cte.query.body else {
        return Err(SqlError::syntax(format!("with a SEARCH or CYCLE clause, the recursive query \"{name}\" must be a UNION")));
    };
    let (SetExpr::Select(l), SetExpr::Select(r)) = (left.as_mut(), right.as_mut()) else {
        return Err(SqlError::feature_not_supported("with a SEARCH or CYCLE clause, both sides of the UNION must be a SELECT"));
    };
    let names = output_names(&columns, &l.projection);
    let q = self_reference(&r.from, &name).ok_or_else(|| SqlError::syntax("SEARCH / CYCLE needs a recursive reference"))?;
    let rows = |cols: &[String], kind: &str| -> SqlResult<(Expr, Expr)> {
        let mut lv = Vec::new();
        let mut rv = Vec::new();
        for c in cols {
            let i = names
                .iter()
                .position(|n| n == c)
                .ok_or_else(|| SqlError::new(code::UNDEFINED_COLUMN, format!("{kind} column \"{c}\" not in WITH query column list")))?;
            lv.push(projection_expr(&l.projection, i)?);
            rv.push(projection_expr(&r.projection, i)?);
        }
        Ok((Expr::Row(lv), Expr::Row(rv)))
    };
    let mut added = Vec::new();
    let mut extra_left = Vec::new();
    let mut extra_right = Vec::new();
    let mut condition = None;
    for clause in clauses {
        match clause {
            Clause::Search { breadth, cols, set } => {
                let (lrow, rrow) = rows(&cols, "search")?;
                if breadth {
                    let (Expr::Row(mut lv), Expr::Row(mut rv)) = (lrow, rrow) else { unreachable!() };
                    lv.insert(0, Expr::Literal(Literal::Integer("0".into())));
                    let depth = Expr::Function(Box::new(FunctionCall::new(crate::SEARCH_DEPTH_FN, vec![column(&q, &set)])));
                    rv.insert(
                        0,
                        Expr::Binary {
                            op: "+".into(),
                            left: Box::new(depth),
                            right: Box::new(Expr::Literal(Literal::Integer("1".into()))),
                        },
                    );
                    extra_left.push((Expr::Row(lv), set.clone()));
                    extra_right.push((Expr::Row(rv), set.clone()));
                } else {
                    extra_left.push((Expr::Array(vec![lrow]), set.clone()));
                    let path = Expr::Binary { op: "||".into(), left: Box::new(column(&q, &set)), right: Box::new(Expr::Array(vec![rrow])) };
                    extra_right.push((path, set.clone()));
                }
                added.push(set);
            }
            Clause::Cycle { cols, set, to, default, using } => {
                let (lrow, rrow) = rows(&cols, "cycle")?;
                let to = literal(to.as_deref(), true)?;
                let default = literal(default.as_deref(), false)?;
                let text = |e: &Expr| match e {
                    Expr::Literal(Literal::String(_)) => Expr::Cast { expr: Box::new(e.clone()), ty: TypeName::simple("text") },
                    other => other.clone(),
                };
                extra_left.push((text(&default), set.clone()));
                extra_left.push((Expr::Array(vec![lrow]), using.clone()));
                let seen = Expr::AnyAll {
                    left: Box::new(rrow.clone()),
                    op: "=".into(),
                    right: AnyAllRight::Array(Box::new(column(&q, &using))),
                    all: false,
                };
                let mark = Expr::Case { operand: None, whens: vec![(seen, text(&to))], else_result: Some(Box::new(text(&default))) };
                extra_right.push((mark, set.clone()));
                let path = Expr::Binary { op: "||".into(), left: Box::new(column(&q, &using)), right: Box::new(Expr::Array(vec![rrow])) };
                extra_right.push((path, using.clone()));
                condition = Some(Expr::Binary { op: "<>".into(), left: Box::new(column(&q, &set)), right: Box::new(text(&to)) });
                added.push(set);
                added.push(using);
            }
        }
    }
    for (e, alias) in extra_left {
        l.projection.push(SelectItem::Expr { expr: e, alias: Some(alias) });
    }
    for (e, alias) in extra_right {
        r.projection.push(SelectItem::Expr { expr: e, alias: Some(alias) });
    }
    if let Some(c) = condition {
        r.selection = Some(match r.selection.take() {
            Some(w) => Expr::And(Box::new(w), Box::new(c)),
            None => c,
        });
    }
    if !cte.columns.is_empty() {
        cte.columns.extend(added);
    }
    Ok(())
}
