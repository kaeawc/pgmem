//! Query (SELECT / VALUES / set operation / WITH) lowering.

use crate::expr::{lower_expr, lower_exprs};
use crate::{ident, object_name, unsupported};
use pgmem_ir::{
    Cte, Distinct, Expr, FrameBound, FrameExclude, FrameUnits, FunctionCall, JoinConstraint, JoinKind, LockClause, LockStrength,
    NamedWindow, OrderByExpr, Query, Select, SelectItem, SetExpr, SetOperator, TableAlias, TableRef, WindowFrame, WindowSpec, With,
};
use pgmem_types::{SqlError, SqlResult, code};
use sqlparser::ast as sq;

pub(crate) fn lower_query(q: sq::Query) -> SqlResult<Query> {
    if !q.pipe_operators.is_empty() || q.settings.is_some() || q.format_clause.is_some() || q.for_clause.is_some() {
        return Err(unsupported("this query syntax"));
    }
    let with = q.with.map(lower_with).transpose()?;
    let body = lower_set_expr(*q.body)?;
    let order_by = match q.order_by {
        None => Vec::new(),
        Some(ob) => match ob.kind {
            sq::OrderByKind::Expressions(items) => items.into_iter().map(lower_order_by_expr).collect::<SqlResult<Vec<_>>>()?,
            sq::OrderByKind::All(_) => return Err(unsupported("ORDER BY ALL")),
        },
    };
    let (mut limit, mut offset) = (None, None);
    match q.limit_clause {
        None => {}
        Some(sq::LimitClause::LimitOffset { limit: l, offset: o, limit_by }) => {
            if !limit_by.is_empty() {
                return Err(unsupported("LIMIT BY"));
            }
            limit = l.map(lower_limit).transpose()?.flatten();
            offset = o.map(|o| lower_expr(o.value)).transpose()?;
        }
        Some(sq::LimitClause::OffsetCommaLimit { .. }) => return Err(SqlError::syntax("LIMIT #,# syntax is not supported")),
    }
    let mut with_ties = false;
    if let Some(fetch) = q.fetch {
        if fetch.percent {
            return Err(unsupported("FETCH ... PERCENT"));
        }
        if fetch.with_ties && order_by.is_empty() {
            return Err(SqlError::syntax("WITH TIES cannot be specified without ORDER BY clause"));
        }
        with_ties = fetch.with_ties;
        limit = Some(match fetch.quantity {
            Some(e) => lower_expr(e)?,
            None => Expr::Literal(pgmem_ir::Literal::Integer("1".into())),
        });
    }
    let locking = q
        .locks
        .into_iter()
        .map(|l| {
            Ok(LockClause {
                strength: match l.lock_type {
                    sq::LockType::Update => LockStrength::Update,
                    sq::LockType::Share => LockStrength::Share,
                },
                of: l.of.iter().map(object_name).collect::<SqlResult<Vec<_>>>()?,
                nowait: matches!(l.nonblock, Some(sq::NonBlock::Nowait)),
                skip_locked: matches!(l.nonblock, Some(sq::NonBlock::SkipLocked)),
            })
        })
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(Query { with, body, order_by, limit, offset, with_ties, locking })
}

/// `LIMIT ALL` and `LIMIT NULL` both mean "no limit".
fn lower_limit(e: sq::Expr) -> SqlResult<Option<Expr>> {
    if let sq::Expr::Identifier(i) = &e
        && i.quote_style.is_none()
        && i.value.eq_ignore_ascii_case("all")
    {
        return Ok(None);
    }
    Ok(Some(lower_expr(e)?))
}

fn lower_with(w: sq::With) -> SqlResult<With> {
    let ctes = w
        .cte_tables
        .into_iter()
        .map(|cte| {
            let mut c = Cte {
                name: ident(&cte.alias.name),
                columns: cte.alias.columns.iter().map(|c| ident(&c.name)).collect(),
                query: Box::new(lower_query(*cte.query)?),
                materialized: cte.materialized.map(|m| matches!(m, sq::CteAsMaterialized::Materialized)),
            };
            crate::search_cycle::apply(&mut c)?;
            Ok(c)
        })
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(With { recursive: w.recursive, ctes })
}

fn lower_set_expr(body: sq::SetExpr) -> SqlResult<SetExpr> {
    Ok(match body {
        sq::SetExpr::Select(s) => SetExpr::Select(Box::new(lower_select(*s)?)),
        sq::SetExpr::Query(q) => SetExpr::Query(Box::new(lower_query(*q)?)),
        sq::SetExpr::Values(v) => SetExpr::Values(v.rows.into_iter().map(|row| lower_exprs(row.content)).collect::<SqlResult<Vec<_>>>()?),
        sq::SetExpr::Insert(st) | sq::SetExpr::Update(st) | sq::SetExpr::Delete(st) | sq::SetExpr::Merge(st) => {
            SetExpr::Dml(Box::new(crate::lower_statement(st)?))
        }
        sq::SetExpr::SetOperation { left, op, set_quantifier, right } => SetExpr::SetOp {
            op: match op {
                sq::SetOperator::Union => SetOperator::Union,
                sq::SetOperator::Intersect => SetOperator::Intersect,
                sq::SetOperator::Except | sq::SetOperator::Minus => SetOperator::Except,
            },
            all: match set_quantifier {
                sq::SetQuantifier::All => true,
                sq::SetQuantifier::Distinct | sq::SetQuantifier::None => false,
                _ => return Err(unsupported("set operation BY NAME")),
            },
            left: Box::new(lower_set_expr(*left)?),
            right: Box::new(lower_set_expr(*right)?),
        },
        sq::SetExpr::Table(t) => {
            let name = t.table_name.ok_or_else(|| SqlError::syntax("TABLE requires a name"))?;
            let mut parts = Vec::new();
            if let Some(s) = t.schema_name {
                parts.push(ident(&s));
            }
            parts.push(ident(&name));
            SetExpr::Select(Box::new(Select {
                distinct: Distinct::All,
                projection: vec![SelectItem::Wildcard],
                from: vec![TableRef::Table { name: pgmem_ir::ObjectName(parts), alias: None }],
                selection: None,
                group_by: Vec::new(),
                grouping_sets: None,
                having: None,
                windows: Vec::new(),
            }))
        }
    })
}

fn lower_select(s: sq::Select) -> SqlResult<Select> {
    if s.top.is_some() || s.into.is_some() || !s.lateral_views.is_empty() || s.qualify.is_some() || !s.connect_by.is_empty() {
        return Err(unsupported("this SELECT syntax"));
    }
    let distinct = match s.distinct {
        None | Some(sq::Distinct::All) => Distinct::All,
        Some(sq::Distinct::Distinct) => Distinct::Distinct,
        Some(sq::Distinct::On(exprs)) => Distinct::On(lower_exprs(exprs)?),
    };
    let projection = s.projection.into_iter().map(lower_select_item).collect::<SqlResult<Vec<_>>>()?;
    let from = s.from.into_iter().map(lower_table_with_joins).collect::<SqlResult<Vec<_>>>()?;
    let selection = s.selection.map(lower_expr).transpose()?;
    let (group_by, grouping_sets) = match s.group_by {
        sq::GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(unsupported("GROUP BY modifiers"));
            }
            lower_group_by(exprs)?
        }
        sq::GroupByExpr::All(_) => return Err(unsupported("GROUP BY ALL")),
    };
    let having = s.having.map(lower_expr).transpose()?;
    let windows = s
        .named_window
        .into_iter()
        .map(|w| {
            let spec = match w.1 {
                sq::NamedWindowExpr::WindowSpec(spec) => lower_window_spec(spec)?,
                sq::NamedWindowExpr::NamedWindow(n) => WindowSpec { base: Some(ident(&n)), ..Default::default() },
            };
            Ok(NamedWindow { name: ident(&w.0), spec })
        })
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(Select { distinct, projection, from, selection, group_by, grouping_sets, having, windows })
}

/// A GROUP BY list: plain expressions, or (with GROUPING SETS / ROLLUP / CUBE) the distinct
/// grouping expressions and the expanded sets over them, in Postgres' order
/// (`expand_grouping_sets`): the cross product of the items' sets, ROLLUP from longest to
/// empty, CUBE as every subset.
fn lower_group_by(exprs: Vec<sq::Expr>) -> SqlResult<(Vec<Expr>, Option<pgmem_ir::GroupingSets>)> {
    let grouped = |e: &sq::Expr| matches!(e, sq::Expr::Rollup(_) | sq::Expr::Cube(_) | sq::Expr::GroupingSets(_));
    if !exprs.iter().any(grouped) {
        return Ok((lower_exprs(exprs)?, None));
    }
    let mut distinct: Vec<Expr> = Vec::new();
    let mut sets: Vec<Vec<usize>> = vec![Vec::new()];
    for item in exprs {
        let item_sets = group_item_sets(item, &mut distinct)?;
        sets = sets
            .iter()
            .flat_map(|prefix| {
                item_sets.iter().map(move |s| {
                    let mut out = prefix.clone();
                    out.extend(s.iter().filter(|i| !prefix.contains(i)));
                    out
                })
            })
            .collect();
    }
    Ok((distinct, Some(sets)))
}

/// The index of `e` among the distinct grouping expressions.
fn group_index(e: sq::Expr, distinct: &mut Vec<Expr>) -> SqlResult<usize> {
    let e = lower_expr(e)?;
    Ok(match distinct.iter().position(|d| *d == e) {
        Some(i) => i,
        None => {
            distinct.push(e);
            distinct.len() - 1
        }
    })
}

/// A parenthesized element of ROLLUP / CUBE / GROUPING SETS: its expressions' indexes.
fn group_list(list: Vec<sq::Expr>, distinct: &mut Vec<Expr>) -> SqlResult<Vec<Vec<usize>>> {
    let mut sets: Vec<Vec<usize>> = vec![Vec::new()];
    for e in list {
        let item = group_item_sets(e, distinct)?;
        sets = sets
            .iter()
            .flat_map(|p| {
                item.iter().map(move |s| {
                    let mut out = p.clone();
                    out.extend(s.iter().filter(|i| !p.contains(i)));
                    out
                })
            })
            .collect();
    }
    Ok(sets)
}

fn group_item_sets(item: sq::Expr, distinct: &mut Vec<Expr>) -> SqlResult<Vec<Vec<usize>>> {
    let flat = |lists: Vec<Vec<sq::Expr>>, distinct: &mut Vec<Expr>| -> SqlResult<Vec<Vec<usize>>> {
        lists.into_iter().map(|l| l.into_iter().map(|e| group_index(e, distinct)).collect()).collect()
    };
    Ok(match item {
        sq::Expr::Rollup(lists) => {
            let parts = flat(lists, distinct)?;
            (0..=parts.len()).rev().map(|n| parts[..n].concat()).collect()
        }
        sq::Expr::Cube(lists) => {
            let parts = flat(lists, distinct)?;
            let n = parts.len();
            (0..1u64 << n)
                .rev()
                .map(|mask| (0..n).filter(|i| mask & (1 << (n - 1 - i)) != 0).flat_map(|i| parts[i].clone()).collect())
                .collect()
        }
        sq::Expr::GroupingSets(lists) => {
            let mut out = Vec::new();
            for l in lists {
                out.extend(group_list(l, distinct)?);
            }
            out
        }
        sq::Expr::Nested(inner) if !matches!(*inner, sq::Expr::Tuple(_)) => vec![vec![group_index(*inner, distinct)?]],
        sq::Expr::Tuple(list) => group_list(list, distinct)?,
        e => vec![vec![group_index(e, distinct)?]],
    })
}

fn lower_select_item(item: sq::SelectItem) -> SqlResult<SelectItem> {
    Ok(match item {
        sq::SelectItem::UnnamedExpr(e) => match lower_expr(e)? {
            // `(x).*`, rewritten before parsing (see `rewrite_unsupported`).
            Expr::Function(f) if f.name.base() == crate::FIELD_STAR_MARKER && f.args.len() == 1 => {
                SelectItem::FieldStar(f.args.into_iter().next().unwrap_or(Expr::Literal(pgmem_ir::Literal::Null)))
            }
            expr => SelectItem::Expr { expr, alias: None },
        },
        sq::SelectItem::ExprWithAlias { expr, alias } => SelectItem::Expr { expr: lower_expr(expr)?, alias: Some(ident(&alias)) },
        sq::SelectItem::Wildcard(_) => SelectItem::Wildcard,
        sq::SelectItem::QualifiedWildcard(sq::SelectItemQualifiedWildcardKind::ObjectName(n), _) => {
            SelectItem::QualifiedWildcard(object_name(&n)?)
        }
        _ => return Err(unsupported("this select-list item")),
    })
}

pub(crate) fn lower_table_with_joins(t: sq::TableWithJoins) -> SqlResult<TableRef> {
    let mut left = lower_table_factor(t.relation)?;
    for join in t.joins {
        // `a JOIN b` needs ON/USING (sqlparser accepts it bare).
        if matches!(
            join.join_operator,
            sq::JoinOperator::Join(sq::JoinConstraint::None)
                | sq::JoinOperator::Inner(sq::JoinConstraint::None)
                | sq::JoinOperator::Left(sq::JoinConstraint::None)
                | sq::JoinOperator::LeftOuter(sq::JoinConstraint::None)
                | sq::JoinOperator::Right(sq::JoinConstraint::None)
                | sq::JoinOperator::RightOuter(sq::JoinConstraint::None)
                | sq::JoinOperator::FullOuter(sq::JoinConstraint::None)
        ) {
            use sqlparser::ast::Spanned;
            return Err(crate::syntax_error_at(join.relation.span().end));
        }
        let right = lower_table_factor(join.relation)?;
        let (kind, constraint) = match join.join_operator {
            sq::JoinOperator::Join(c) | sq::JoinOperator::Inner(c) => (JoinKind::Inner, c),
            sq::JoinOperator::Left(c) | sq::JoinOperator::LeftOuter(c) => (JoinKind::Left, c),
            sq::JoinOperator::Right(c) | sq::JoinOperator::RightOuter(c) => (JoinKind::Right, c),
            sq::JoinOperator::FullOuter(c) => (JoinKind::Full, c),
            sq::JoinOperator::CrossJoin(c) => (JoinKind::Cross, c),
            _ => return Err(unsupported("this join type")),
        };
        let mut using_alias = None;
        let constraint = match constraint {
            sq::JoinConstraint::On(e) => JoinConstraint::On(lower_expr(e)?),
            sq::JoinConstraint::Using(cols) => {
                JoinConstraint::Using(cols.iter().map(|c| object_name(c).map(|n| n.base().to_owned())).collect::<SqlResult<Vec<_>>>()?)
            }
            sq::JoinConstraint::Natural => JoinConstraint::Natural,
            sq::JoinConstraint::None => JoinConstraint::None,
            sq::JoinConstraint::UsingAlias(cols, alias) => {
                using_alias = Some(ident(&alias));
                JoinConstraint::Using(cols.iter().map(|c| object_name(c).map(|n| n.base().to_owned())).collect::<SqlResult<Vec<_>>>()?)
            }
        };
        left = TableRef::Join { left: Box::new(left), right: Box::new(right), kind, constraint, using_alias, alias: None };
    }
    Ok(left)
}

fn lower_alias(a: Option<sq::TableAlias>) -> Option<TableAlias> {
    let a = a?;
    let column_types = if a.columns.iter().all(|c| c.data_type.is_some()) && !a.columns.is_empty() {
        a.columns.iter().filter_map(|c| c.data_type.as_ref()).filter_map(|t| crate::expr::lower_type(t).ok()).collect()
    } else {
        Vec::new()
    };
    Some(TableAlias { name: ident(&a.name), columns: a.columns.iter().map(|c| ident(&c.name)).collect(), column_types })
}

pub(crate) fn lower_table_factor(f: sq::TableFactor) -> SqlResult<TableRef> {
    Ok(match f {
        sq::TableFactor::Table { name, alias, args, with_ordinality, .. } => {
            let name = object_name(&name)?;
            match args {
                None => TableRef::Table { name, alias: lower_alias(alias) },
                Some(args) => {
                    let mut call = FunctionCall::new(name.base(), Vec::new());
                    call.name = name;
                    for a in args.args {
                        match a {
                            sq::FunctionArg::Unnamed(sq::FunctionArgExpr::Expr(e)) => call.args.push(lower_expr(e)?),
                            _ => return Err(unsupported("this table function argument")),
                        }
                    }
                    TableRef::Function { call, alias: lower_alias(alias), lateral: false, with_ordinality }
                }
            }
        }
        sq::TableFactor::Derived { lateral, subquery, alias, .. } => {
            TableRef::Subquery { query: Box::new(lower_query(*subquery)?), alias: lower_alias(alias), lateral }
        }
        sq::TableFactor::Function { lateral, name, args, with_ordinality, alias } => {
            let name = object_name(&name)?;
            let mut call = FunctionCall::new(name.base(), Vec::new());
            call.name = name;
            for a in args {
                match a {
                    sq::FunctionArg::Unnamed(sq::FunctionArgExpr::Expr(e)) => call.args.push(lower_expr(e)?),
                    _ => return Err(unsupported("this table function argument")),
                }
            }
            TableRef::Function { call, alias: lower_alias(alias), lateral, with_ordinality }
        }
        sq::TableFactor::UNNEST { alias, array_exprs, with_ordinality, with_offset, .. } => {
            if with_offset {
                return Err(unsupported("UNNEST WITH OFFSET"));
            }
            let call = FunctionCall::new("unnest", lower_exprs(array_exprs)?);
            TableRef::Function { call, alias: lower_alias(alias), lateral: false, with_ordinality }
        }
        sq::TableFactor::TableFunction { expr, alias } => match lower_expr(expr)? {
            Expr::Function(call) => TableRef::Function { call: *call, alias: lower_alias(alias), lateral: false, with_ordinality: false },
            _ => return Err(unsupported("this table function")),
        },
        sq::TableFactor::NestedJoin { table_with_joins, alias } => {
            let inner = lower_table_with_joins(*table_with_joins)?;
            match (inner, lower_alias(alias)) {
                (TableRef::Join { left, right, kind, constraint, using_alias, .. }, Some(a)) => {
                    TableRef::Join { left, right, kind, constraint, using_alias, alias: Some(a) }
                }
                (inner, None) => inner,
                (_, Some(_)) => return Err(unsupported("aliased parenthesized FROM item")),
            }
        }
        _ => return Err(unsupported("this FROM item")),
    })
}

pub(crate) fn lower_order_by_expr(o: sq::OrderByExpr) -> SqlResult<OrderByExpr> {
    if o.with_fill.is_some() {
        return Err(unsupported("WITH FILL"));
    }
    Ok(OrderByExpr {
        expr: lower_expr(o.expr)?,
        desc: matches!(o.options.sort, Some(sq::OrderBySort::Desc)),
        nulls_first: o.options.nulls_first,
    })
}

pub(crate) fn lower_window_spec(spec: sq::WindowSpec) -> SqlResult<WindowSpec> {
    let frame = spec
        .window_frame
        .map(|f| {
            let units = match f.units {
                sq::WindowFrameUnits::Rows => FrameUnits::Rows,
                sq::WindowFrameUnits::Range => FrameUnits::Range,
                sq::WindowFrameUnits::Groups => FrameUnits::Groups,
            };
            let bound = |b: sq::WindowFrameBound| -> SqlResult<FrameBound> {
                Ok(match b {
                    sq::WindowFrameBound::CurrentRow => FrameBound::CurrentRow,
                    sq::WindowFrameBound::Preceding(None) => FrameBound::UnboundedPreceding,
                    sq::WindowFrameBound::Preceding(Some(e)) => FrameBound::Preceding(Box::new(lower_expr(*e)?)),
                    sq::WindowFrameBound::Following(None) => FrameBound::UnboundedFollowing,
                    sq::WindowFrameBound::Following(Some(e)) => FrameBound::Following(Box::new(lower_expr(*e)?)),
                })
            };
            let start = bound(f.start_bound)?;
            let end = match f.end_bound {
                Some(e) => bound(e)?,
                None => FrameBound::CurrentRow,
            };
            // The frame-bound combinations Postgres' grammar rejects (42P20).
            let invalid = match (&start, &end) {
                (FrameBound::UnboundedFollowing, _) => Some("frame start cannot be UNBOUNDED FOLLOWING"),
                (_, FrameBound::UnboundedPreceding) => Some("frame end cannot be UNBOUNDED PRECEDING"),
                (FrameBound::CurrentRow, FrameBound::Preceding(_)) => Some("frame starting from current row cannot have preceding rows"),
                (FrameBound::Following(_), FrameBound::CurrentRow) => Some("frame starting from following row cannot end with current row"),
                (FrameBound::Following(_), FrameBound::Preceding(_)) => {
                    Some("frame starting from following row cannot have preceding rows")
                }
                _ => None,
            };
            if let Some(msg) = invalid {
                return Err(SqlError::new(code::WINDOWING_ERROR, msg));
            }
            Ok(WindowFrame { units, start, end, exclude: FrameExclude::NoOthers })
        })
        .transpose()?;
    let mut frame = frame;
    let mut partition_by = lower_exprs(spec.partition_by)?;
    // The frame's EXCLUDE clause, carried as a marker in PARTITION BY (see `rewrite_unsupported`).
    if let Some(i) = partition_by.iter().position(|e| matches!(e, Expr::Function(f) if f.name.base() == crate::FRAME_EXCLUDE_MARKER)) {
        let Expr::Function(f) = partition_by.remove(i) else { unreachable!() };
        let kind = match f.args.first() {
            Some(Expr::Literal(pgmem_ir::Literal::String(k))) => k.clone(),
            _ => String::new(),
        };
        let fr = frame.as_mut().ok_or_else(|| SqlError::syntax("syntax error at or near \"EXCLUDE\""))?;
        fr.exclude = match kind.as_str() {
            "current row" => FrameExclude::CurrentRow,
            "group" => FrameExclude::Group,
            "ties" => FrameExclude::Ties,
            _ => FrameExclude::NoOthers,
        };
    }
    Ok(WindowSpec {
        base: spec.window_name.as_ref().map(ident),
        partition_by,
        order_by: spec.order_by.into_iter().map(lower_order_by_expr).collect::<SqlResult<Vec<_>>>()?,
        frame,
    })
}
