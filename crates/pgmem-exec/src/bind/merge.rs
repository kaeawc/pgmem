//! MERGE planning: every expression is bound over the target row followed by the source row.

use super::dml::{RowRules, bind_assignments, lookup_table, physical_scope};
use super::{Binder, Clause, Ctx, Level, Scope, coerce_assignment};
use crate::plan::{Expr, OutCol, Rel};
use pgmem_ir as ir;
use pgmem_types::{SqlError, SqlResult, code};

#[derive(Clone, Debug)]
pub enum MergeActionPlan {
    /// Assignments over the combined row; `None` is `SET col = DEFAULT`.
    Update(Vec<(usize, Option<Expr>)>),
    Delete,
    DoNothing,
    /// Target slots and their values (`None` = DEFAULT); `values: None` is DEFAULT VALUES.
    Insert {
        columns: Vec<usize>,
        values: Option<Vec<Option<Expr>>>,
        overriding_system_value: bool,
    },
}

#[derive(Clone, Debug)]
pub struct MergeClausePlan {
    pub kind: ir::MergeMatch,
    pub condition: Option<Expr>,
    pub action: MergeActionPlan,
}

#[derive(Clone, Debug)]
pub struct MergePlan {
    pub rules: RowRules,
    pub source: Rel,
    pub source_width: usize,
    pub on: Expr,
    pub clauses: Vec<MergeClausePlan>,
    pub returning: Vec<Expr>,
    pub returning_cols: Vec<OutCol>,
}

pub fn bind_merge(b: &Binder, m: &ir::Merge, outer: &Ctx) -> SqlResult<MergePlan> {
    let table = lookup_table(b, &m.table)?;
    let qualifier = m.alias.clone().unwrap_or_else(|| table.name.clone());
    let rules = b.bind_row_rules(&table)?;
    let target_scope = physical_scope(&table, &qualifier);
    let width = target_scope.width();
    let empty = Level::new(Scope::default());
    let (source, sscope) = b.bind_from(std::slice::from_ref(&m.source), &outer.from_item(&[&empty]))?;
    let source_width = sscope.width();
    let mut cols = target_scope.cols.clone();
    cols.extend(sscope.cols.iter().cloned());
    let mut star = target_scope.star.clone();
    star.extend(sscope.star.iter().map(|i| i + width));
    let level = Level::new(Scope { cols, star });
    let ctx = outer.from_item(&[&level]);
    let on = b.bind_predicate(&m.on, "JOIN/ON", &ctx.with_clause(Clause::JoinOn))?;
    let mut clauses = Vec::with_capacity(m.clauses.len());
    for c in &m.clauses {
        let condition = c.condition.as_ref().map(|e| b.bind_predicate(e, "WHEN", &ctx.with_clause(Clause::Where))).transpose()?;
        let action = match &c.action {
            ir::MergeAction::Delete => MergeActionPlan::Delete,
            ir::MergeAction::DoNothing => MergeActionPlan::DoNothing,
            ir::MergeAction::Update(assignments) => MergeActionPlan::Update(bind_assignments(b, &table, assignments, &ctx)?),
            ir::MergeAction::Insert { columns, values, overriding_system_value } => {
                let slots: Vec<usize> = if columns.is_empty() {
                    // Without a column list, the values fill the leading columns.
                    let n = values.as_ref().map_or(usize::MAX, Vec::len);
                    table.live_columns().map(|(i, _)| i).take(n).collect()
                } else {
                    let mut out = Vec::new();
                    for c in columns {
                        let idx = table
                            .column_index(c)
                            .ok_or_else(|| SqlError::undefined_column(&format!("\"{c}\" of relation \"{}\"", table.name)))?;
                        if out.contains(&idx) {
                            return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{c}\" specified more than once")));
                        }
                        out.push(idx);
                    }
                    out
                };
                let values = match values {
                    None => None,
                    Some(vals) => {
                        if vals.len() > slots.len() {
                            return Err(SqlError::syntax("INSERT has more expressions than target columns"));
                        }
                        if vals.len() < slots.len() {
                            return Err(SqlError::syntax("INSERT has more target columns than expressions"));
                        }
                        let vctx = ctx.with_clause(Clause::Values);
                        let mut bound = Vec::with_capacity(vals.len());
                        for (e, &slot) in vals.iter().zip(&slots) {
                            if matches!(e, ir::Expr::Default) {
                                bound.push(None);
                                continue;
                            }
                            let col = &table.columns[slot];
                            let (x, t) = b.bind_expr(e, &vctx)?;
                            bound.push(Some(coerce_assignment(b, x, &t, &col.ty, col.typmod, &col.name)?));
                        }
                        Some(bound)
                    }
                };
                MergeActionPlan::Insert { columns: slots, values, overriding_system_value: *overriding_system_value }
            }
        };
        clauses.push(MergeClausePlan { kind: c.kind, condition, action });
    }
    let (returning, returning_cols) = b.bind_returning(&m.returning, &ctx, &table)?;
    Ok(MergePlan { rules, source, source_width, on, clauses, returning, returning_cols })
}
