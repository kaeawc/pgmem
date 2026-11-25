//! INSERT / UPDATE / DELETE lowering.

use crate::expr::lower_expr;
use crate::query::{lower_query, lower_table_with_joins};
use crate::{ident, object_name, unsupported};
use pgmem_ir::{
    Assignment, ConflictAction, ConflictTarget, Delete, Insert, InsertSource, ObjectName, OnConflict, SelectItem, SetExpr, TableRef, Update,
};
use pgmem_types::{SqlError, SqlResult};
use sqlparser::ast as sq;

fn lower_returning(items: Option<Vec<sq::SelectItem>>) -> SqlResult<Vec<SelectItem>> {
    let Some(items) = items else { return Ok(Vec::new()) };
    items
        .into_iter()
        .map(|item| {
            Ok(match item {
                sq::SelectItem::UnnamedExpr(e) => SelectItem::Expr { expr: lower_expr(e)?, alias: None },
                sq::SelectItem::ExprWithAlias { expr, alias } => SelectItem::Expr { expr: lower_expr(expr)?, alias: Some(ident(&alias)) },
                sq::SelectItem::Wildcard(_) => SelectItem::Wildcard,
                sq::SelectItem::QualifiedWildcard(sq::SelectItemQualifiedWildcardKind::ObjectName(n), _) => {
                    SelectItem::QualifiedWildcard(object_name(&n)?)
                }
                _ => return Err(unsupported("this RETURNING item")),
            })
        })
        .collect()
}

fn lower_assignments(items: Vec<sq::Assignment>) -> SqlResult<Vec<Assignment>> {
    items
        .into_iter()
        .map(|a| {
            let columns = match a.target {
                sq::AssignmentTarget::ColumnName(n) => vec![column_target(&n)?],
                sq::AssignmentTarget::Tuple(names) => names.iter().map(column_target).collect::<SqlResult<Vec<_>>>()?,
            };
            Ok(Assignment { columns, value: lower_expr(a.value)? })
        })
        .collect()
}

/// A SET target: the column name, then composite field names (`SET addr.zip = ...`). The
/// binder decides whether a qualified target names a field.
fn column_target(n: &sq::ObjectName) -> SqlResult<Vec<String>> {
    Ok(object_name(n)?.0)
}

pub(crate) fn lower_insert(i: sq::Insert) -> SqlResult<Insert> {
    let table = match &i.table {
        sq::TableObject::TableName(n) => object_name(n)?,
        _ => return Err(unsupported("INSERT into a table function")),
    };
    if i.ignore || i.or.is_some() || i.replace_into || !i.assignments.is_empty() {
        return Err(unsupported("this INSERT syntax"));
    }
    let columns = i.columns.iter().map(|c| object_name(c).map(|n| n.base().to_owned())).collect::<SqlResult<Vec<_>>>()?;
    let (with, source) = match i.source {
        None => (None, InsertSource::DefaultValues),
        Some(q) => {
            let q = *q;
            let plain_values =
                q.order_by.is_none() && q.limit_clause.is_none() && q.fetch.is_none() && matches!(*q.body, sq::SetExpr::Values(_));
            let mut lowered = lower_query(q)?;
            // `INSERT INTO t WITH x AS (...) SELECT ...`: the WITH is the source query's.
            let with = None;
            let plain_values = plain_values && lowered.with.is_none();
            if plain_values {
                match lowered.body {
                    SetExpr::Values(rows) => (with, InsertSource::Values(rows)),
                    other => {
                        lowered.body = other;
                        (with, InsertSource::Query(Box::new(lowered)))
                    }
                }
            } else {
                (with, InsertSource::Query(Box::new(lowered)))
            }
        }
    };
    let on_conflict = match i.on {
        None => None,
        Some(sq::OnInsert::OnConflict(oc)) => Some(OnConflict {
            target: match oc.conflict_target {
                None => ConflictTarget::None,
                Some(sq::ConflictTarget::Columns { columns, predicate }) => ConflictTarget::Columns {
                    columns: columns.iter().map(ident).collect(),
                    selection: predicate.map(lower_expr).transpose()?,
                },
                Some(sq::ConflictTarget::OnConstraint(n)) => ConflictTarget::Constraint(object_name(&n)?.base().to_owned()),
            },
            action: match oc.action {
                sq::OnConflictAction::DoNothing => ConflictAction::Nothing,
                sq::OnConflictAction::DoUpdate(du) => ConflictAction::Update {
                    assignments: lower_assignments(du.assignments)?,
                    selection: du.selection.map(lower_expr).transpose()?,
                },
            },
        }),
        Some(sq::OnInsert::DuplicateKeyUpdate(_)) => return Err(SqlError::syntax("syntax error at or near \"DUPLICATE\"")),
        Some(_) => return Err(unsupported("this ON clause")),
    };
    Ok(Insert {
        with,
        table,
        alias: i.table_alias.as_ref().map(|a| ident(&a.alias)),
        columns,
        source,
        overriding_system_value: matches!(i.overriding, Some(sq::InsertOverriding::System)),
        overriding_user_value: matches!(i.overriding, Some(sq::InsertOverriding::User)),
        on_conflict,
        returning: lower_returning(i.returning)?,
    })
}

/// The single target table (with optional alias) of an UPDATE or DELETE.
fn target_table(t: sq::TableWithJoins) -> SqlResult<(ObjectName, Option<String>)> {
    if !t.joins.is_empty() {
        return Err(unsupported("joins in the UPDATE/DELETE target"));
    }
    match lower_table_with_joins(t)? {
        TableRef::Table { name, alias } => {
            if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                return Err(SqlError::syntax("column aliases are not allowed here"));
            }
            Ok((name, alias.map(|a| a.name)))
        }
        _ => Err(unsupported("this UPDATE/DELETE target")),
    }
}

pub(crate) fn lower_update(u: sq::Update) -> SqlResult<Update> {
    // Postgres' grammar has neither (MySQL syntax).
    if !u.order_by.is_empty() {
        return Err(SqlError::syntax("syntax error at or near \"ORDER\""));
    }
    if u.limit.is_some() {
        return Err(SqlError::syntax("syntax error at or near \"LIMIT\""));
    }
    let (table, alias) = target_table(u.table)?;
    let from = match u.from {
        None => Vec::new(),
        Some(sq::UpdateTableFromKind::AfterSet(items) | sq::UpdateTableFromKind::BeforeSet(items)) => {
            items.into_iter().map(lower_table_with_joins).collect::<SqlResult<Vec<_>>>()?
        }
    };
    Ok(Update {
        with: None,
        table,
        alias,
        assignments: lower_assignments(u.assignments)?,
        from,
        selection: u.selection.map(lower_expr).transpose()?,
        returning: lower_returning(u.returning)?,
    })
}

pub(crate) fn lower_delete(d: sq::Delete) -> SqlResult<Delete> {
    if !d.order_by.is_empty() {
        return Err(SqlError::syntax("syntax error at or near \"ORDER\""));
    }
    if d.limit.is_some() {
        return Err(SqlError::syntax("syntax error at or near \"LIMIT\""));
    }
    if !d.tables.is_empty() {
        return Err(unsupported("this DELETE syntax"));
    }
    let mut from = match d.from {
        sq::FromTable::WithFromKeyword(items) | sq::FromTable::WithoutKeyword(items) => items,
    };
    if from.len() != 1 {
        return Err(unsupported("DELETE from several tables"));
    }
    let (table, alias) = target_table(from.remove(0))?;
    let using = match d.using {
        None => Vec::new(),
        Some(items) => items.into_iter().map(lower_table_with_joins).collect::<SqlResult<Vec<_>>>()?,
    };
    Ok(Delete {
        with: None,
        table,
        alias,
        using,
        selection: d.selection.map(lower_expr).transpose()?,
        returning: lower_returning(d.returning)?,
    })
}

/// The function name `INSERT DEFAULT VALUES` in a MERGE clause is rewritten to (sqlparser
/// lacks it): `INSERT VALUES (__pgmem_default_values())`.
pub(crate) const MERGE_DEFAULT_VALUES: &str = "__pgmem_default_values";

pub(crate) fn lower_merge(m: sq::Merge) -> SqlResult<pgmem_ir::Merge> {
    use pgmem_ir::{MergeAction, MergeMatch, MergeWhen};
    let (table, alias) = match crate::query::lower_table_factor(m.table)? {
        TableRef::Table { name, alias } => (name, alias.map(|a| a.name)),
        _ => return Err(unsupported("this MERGE target")),
    };
    let source = crate::query::lower_table_factor(m.source)?;
    let mut clauses = Vec::new();
    for c in m.clauses {
        let kind = match c.clause_kind {
            sq::MergeClauseKind::Matched => MergeMatch::Matched,
            sq::MergeClauseKind::NotMatched | sq::MergeClauseKind::NotMatchedByTarget => MergeMatch::NotMatchedByTarget,
            sq::MergeClauseKind::NotMatchedBySource => MergeMatch::NotMatchedBySource,
        };
        let action = match c.action {
            sq::MergeAction::Delete { .. } => MergeAction::Delete,
            sq::MergeAction::DoNothing { .. } => MergeAction::DoNothing,
            sq::MergeAction::Update(u) => match u.kind {
                sq::MergeUpdateKind::Set(a) => MergeAction::Update(lower_assignments(a)?),
                _ => return Err(unsupported("UPDATE * in MERGE")),
            },
            sq::MergeAction::Insert(i) => {
                let columns = i.columns.iter().map(|c| object_name(c).map(|n| n.base().to_owned())).collect::<SqlResult<Vec<_>>>()?;
                let values = match i.kind {
                    sq::MergeInsertKind::Values(v) => {
                        let mut rows = v.rows;
                        if rows.len() != 1 {
                            return Err(SqlError::syntax("syntax error at or near \",\""));
                        }
                        let row = crate::expr::lower_exprs(rows.remove(0).content)?;
                        let default_values =
                            matches!(row.as_slice(), [pgmem_ir::Expr::Function(f)] if f.name.base() == MERGE_DEFAULT_VALUES);
                        (!default_values).then_some(row)
                    }
                    _ => return Err(unsupported("this MERGE INSERT form")),
                };
                MergeAction::Insert { columns, values, overriding_system_value: false }
            }
        };
        clauses.push(MergeWhen { kind, condition: c.predicate.map(lower_expr).transpose()?, action });
    }
    let returning = match m.output {
        Some(sq::OutputClause::Returning { select_items, .. }) => lower_returning(Some(select_items))?,
        Some(sq::OutputClause::Output { .. }) => return Err(SqlError::syntax("syntax error at or near \"OUTPUT\"")),
        None => Vec::new(),
    };
    Ok(pgmem_ir::Merge { with: None, table, alias, source, on: lower_expr(*m.on)?, clauses, returning })
}
