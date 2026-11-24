//! Query (SELECT / VALUES / set operation / WITH) lowering.

use crate::expr::{lower_expr, lower_exprs};
use crate::{ident, object_name, unsupported};
use pgmem_ir::{
    Cte, Distinct, Expr, FrameBound, FrameExclude, FrameUnits, FunctionCall, JoinConstraint, JoinKind, LockClause, LockStrength,
    NamedWindow, OrderByExpr, Query, Select, SelectItem, SetExpr, SetOperator, TableAlias, TableRef, WindowFrame, WindowSpec, With,
};
use pgmem_types::{SqlError, SqlResult};
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
    if let Some(fetch) = q.fetch {
        if fetch.percent || fetch.with_ties {
            return Err(unsupported("FETCH ... PERCENT / WITH TIES"));
        }
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
    Ok(Query { with, body, order_by, limit, offset, locking })
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
            Ok(Cte {
                name: ident(&cte.alias.name),
                columns: cte.alias.columns.iter().map(|c| ident(&c.name)).collect(),
                query: Box::new(lower_query(*cte.query)?),
                materialized: cte.materialized.map(|m| matches!(m, sq::CteAsMaterialized::Materialized)),
            })
        })
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(With { recursive: w.recursive, ctes })
}

fn lower_set_expr(body: sq::SetExpr) -> SqlResult<SetExpr> {
    Ok(match body {
        sq::SetExpr::Select(s) => SetExpr::Select(Box::new(lower_select(*s)?)),
        sq::SetExpr::Query(q) => SetExpr::Query(Box::new(lower_query(*q)?)),
        sq::SetExpr::Values(v) => SetExpr::Values(v.rows.into_iter().map(|row| lower_exprs(row.content)).collect::<SqlResult<Vec<_>>>()?),
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
                having: None,
                windows: Vec::new(),
            }))
        }
        _ => return Err(unsupported("data-modifying statements in WITH")),
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
    let group_by = match s.group_by {
        sq::GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(unsupported("GROUP BY modifiers"));
            }
            lower_exprs(exprs)?
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
    Ok(Select { distinct, projection, from, selection, group_by, having, windows })
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
        let right = lower_table_factor(join.relation)?;
        let (kind, constraint) = match join.join_operator {
            sq::JoinOperator::Join(c) | sq::JoinOperator::Inner(c) => (JoinKind::Inner, c),
            sq::JoinOperator::Left(c) | sq::JoinOperator::LeftOuter(c) => (JoinKind::Left, c),
            sq::JoinOperator::Right(c) | sq::JoinOperator::RightOuter(c) => (JoinKind::Right, c),
            sq::JoinOperator::FullOuter(c) => (JoinKind::Full, c),
            sq::JoinOperator::CrossJoin(c) => (JoinKind::Cross, c),
            _ => return Err(unsupported("this join type")),
        };
        let constraint = match constraint {
            sq::JoinConstraint::On(e) => JoinConstraint::On(lower_expr(e)?),
            sq::JoinConstraint::Using(cols) => {
                JoinConstraint::Using(cols.iter().map(|c| object_name(c).map(|n| n.base().to_owned())).collect::<SqlResult<Vec<_>>>()?)
            }
            sq::JoinConstraint::Natural => JoinConstraint::Natural,
            sq::JoinConstraint::None => JoinConstraint::None,
            sq::JoinConstraint::UsingAlias(..) => return Err(unsupported("USING ... AS alias")),
        };
        left = TableRef::Join { left: Box::new(left), right: Box::new(right), kind, constraint };
    }
    Ok(left)
}

fn lower_alias(a: Option<sq::TableAlias>) -> Option<TableAlias> {
    a.map(|a| TableAlias { name: ident(&a.name), columns: a.columns.iter().map(|c| ident(&c.name)).collect() })
}

fn lower_table_factor(f: sq::TableFactor) -> SqlResult<TableRef> {
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
            if alias.is_some() {
                return Err(unsupported("aliased parenthesized joins"));
            }
            lower_table_with_joins(*table_with_joins)?
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
            Ok(WindowFrame {
                units,
                start: bound(f.start_bound)?,
                end: match f.end_bound {
                    Some(e) => bound(e)?,
                    None => FrameBound::CurrentRow,
                },
                exclude: FrameExclude::NoOthers,
            })
        })
        .transpose()?;
    Ok(WindowSpec {
        base: spec.window_name.as_ref().map(ident),
        partition_by: lower_exprs(spec.partition_by)?,
        order_by: spec.order_by.into_iter().map(lower_order_by_expr).collect::<SqlResult<Vec<_>>>()?,
        frame,
    })
}
