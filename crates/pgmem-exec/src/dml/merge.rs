//! MERGE execution (Postgres' `ExecMerge`): each source row is joined with the target rows
//! it matches; the first WHEN clause of the matching kind whose condition holds decides the
//! action. Effects are computed against the statement's snapshot, validated, then applied.

use super::{
    DmlResult, apply_generated, check_checks, check_foreign_keys, check_not_null, check_uniques, identity_always_error, pad,
    referential_actions, returning_row,
};
use crate::bind::merge::{MergeActionPlan, MergeClausePlan, MergePlan};
use crate::eval::{EvalCtx, StatementState, eval, eval_pred};
use crate::plan::Row;
use crate::run::run;
use crate::session::SessionEnv;
use pgmem_catalog::Identity;
use pgmem_ir::MergeMatch;
use pgmem_storage::{RowId, Txn};
use pgmem_types::{SqlError, SqlResult, Value, code};
use std::collections::HashSet;
use std::sync::Arc;

enum Effect {
    Insert(Row),
    Update(RowId, Row, Row),
    Delete(RowId, Row),
}

struct Merger<'a, 'c> {
    plan: &'a MergePlan,
    ctx: &'a EvalCtx<'c>,
    width: usize,
    effects: Vec<Effect>,
    returning: Vec<Row>,
    /// Target rows already updated or deleted by this command.
    touched: HashSet<RowId>,
}

impl<'a> Merger<'a, '_> {
    /// The first clause of `kind` whose condition holds for `combined`.
    fn clause(&self, kind: MergeMatch, combined: &Row) -> SqlResult<Option<&'a MergeClausePlan>> {
        let plan: &'a MergePlan = self.plan;
        for c in plan.clauses.iter().filter(|c| c.kind == kind) {
            match &c.condition {
                Some(cond) if !eval_pred(cond, combined, self.ctx)? => {}
                _ => return Ok(Some(c)),
            }
        }
        Ok(None)
    }

    fn emit(&mut self, action: &'static str, current: &Row, old: Option<&Row>, new: Option<&Row>) -> SqlResult<()> {
        if self.plan.returning.is_empty() {
            return Ok(());
        }
        self.ctx.state.merge_action.set(action);
        let r = returning_row(current, old, new, self.width);
        self.returning.push(self.plan.returning.iter().map(|e| eval(e, &r, self.ctx)).collect::<SqlResult<Vec<_>>>()?);
        Ok(())
    }

    /// WHEN MATCHED / NOT MATCHED BY SOURCE for target row `id` (`combined` = target ++ source).
    fn target_action(&mut self, kind: MergeMatch, id: RowId, target: &Row, combined: &Row) -> SqlResult<()> {
        let Some(clause) = self.clause(kind, combined)? else { return Ok(()) };
        let source = combined[self.width..].to_vec();
        match &clause.action {
            MergeActionPlan::DoNothing | MergeActionPlan::Insert { .. } => return Ok(()),
            MergeActionPlan::Update(_) | MergeActionPlan::Delete if !self.touched.insert(id) => {
                return Err(SqlError::new(code::CARDINALITY_VIOLATION, "MERGE command cannot affect row a second time")
                    .with_hint("Ensure that not more than one source row matches any one target row."));
            }
            MergeActionPlan::Delete => {
                self.emit("DELETE", combined, Some(target), None)?;
                self.effects.push(Effect::Delete(id, target.clone()));
            }
            MergeActionPlan::Update(assignments) => {
                let mut new_row = target.clone();
                for (slot, e) in assignments {
                    new_row[*slot] = match e {
                        Some(e) => eval(e, combined, self.ctx)?,
                        None => match &self.plan.rules.defaults[*slot] {
                            Some(d) => eval(d, &[], self.ctx)?,
                            None => Value::Null,
                        },
                    };
                }
                apply_generated(&self.plan.rules, &mut new_row, self.ctx)?;
                check_not_null(&self.plan.rules.table, &new_row)?;
                check_checks(&self.plan.rules, &new_row, self.ctx)?;
                let mut current = new_row.clone();
                current.extend(source);
                self.emit("UPDATE", &current, Some(target), Some(&new_row))?;
                self.effects.push(Effect::Update(id, target.clone(), new_row));
            }
        }
        Ok(())
    }

    /// WHEN NOT MATCHED [BY TARGET] for a source row (`combined` = NULL target ++ source).
    fn source_action(&mut self, combined: &Row) -> SqlResult<()> {
        let Some(clause) = self.clause(MergeMatch::NotMatchedByTarget, combined)? else { return Ok(()) };
        let MergeActionPlan::Insert { columns, values, overriding_system_value } = &clause.action else { return Ok(()) };
        let table = Arc::clone(&self.plan.rules.table);
        let mut row: Row = vec![Value::Null; self.width];
        let mut explicit = vec![false; self.width];
        for (pos, &slot) in columns.iter().enumerate() {
            let Some(Some(e)) = values.as_ref().map(|v| &v[pos]) else { continue };
            let col = &table.columns[slot];
            if col.generated.is_some() {
                return Err(SqlError::new(
                    code::GENERATED_ALWAYS,
                    format!("cannot insert a non-DEFAULT value into column \"{}\"", col.name),
                )
                .with_detail(format!("Column \"{}\" is a generated column.", col.name)));
            }
            if col.identity == Some(Identity::Always) && !overriding_system_value {
                return Err(identity_always_error(&col.name));
            }
            row[slot] = eval(e, combined, self.ctx)?;
            explicit[slot] = true;
        }
        for (i, c) in table.columns.iter().enumerate() {
            if !explicit[i]
                && !c.dropped
                && c.generated.is_none()
                && let Some(d) = &self.plan.rules.defaults[i]
            {
                row[i] = eval(d, &[], self.ctx)?;
            }
        }
        apply_generated(&self.plan.rules, &mut row, self.ctx)?;
        check_not_null(&table, &row)?;
        check_checks(&self.plan.rules, &row, self.ctx)?;
        let mut current = row.clone();
        current.extend(combined[self.width..].iter().cloned());
        self.emit("INSERT", &current, None, Some(&row))?;
        self.effects.push(Effect::Insert(row));
        Ok(())
    }
}

pub fn exec_merge(plan: &MergePlan, txn: &mut Txn, params: &[Value], env: &SessionEnv, state: &StatementState) -> SqlResult<DmlResult> {
    let table = Arc::clone(&plan.rules.table);
    let width = table.columns.len();
    let effects = {
        let ctx = EvalCtx::new(txn, params, env, state);
        let sources = run(&plan.source, &ctx)?;
        let targets: Vec<(RowId, Row)> = txn
            .scan(table.oid)
            .into_iter()
            .map(|(id, mut r)| {
                pad(&mut r, &table);
                (id, r)
            })
            .collect();
        let mut m = Merger { plan, ctx: &ctx, width, effects: Vec::new(), returning: Vec::new(), touched: HashSet::new() };
        let mut matched = vec![false; targets.len()];
        for s in &sources {
            let mut any = false;
            for (i, (id, t)) in targets.iter().enumerate() {
                let mut combined = t.clone();
                combined.extend(s.iter().cloned());
                if !eval_pred(&plan.on, &combined, &ctx)? {
                    continue;
                }
                any = true;
                matched[i] = true;
                m.target_action(MergeMatch::Matched, *id, t, &combined)?;
            }
            if !any {
                let mut combined = vec![Value::Null; width];
                combined.extend(s.iter().cloned());
                m.source_action(&combined)?;
            }
        }
        if plan.clauses.iter().any(|c| c.kind == MergeMatch::NotMatchedBySource) {
            for (i, (id, t)) in targets.iter().enumerate() {
                if !matched[i] {
                    let mut combined = t.clone();
                    combined.extend(std::iter::repeat_n(Value::Null, plan.source_width));
                    m.target_action(MergeMatch::NotMatchedBySource, *id, t, &combined)?;
                }
            }
        }
        // Unique validation over the table's final state.
        let mut final_rows: Vec<(RowId, Row)> = txn
            .scan_latest(table.oid)
            .into_iter()
            .filter(|(id, _)| !m.touched.contains(id))
            .map(|(id, mut r)| {
                pad(&mut r, &table);
                (id, r)
            })
            .collect();
        let mut changed: HashSet<RowId> = HashSet::new();
        for (n, e) in m.effects.iter().enumerate() {
            match e {
                Effect::Insert(r) => {
                    let fake = u64::MAX - n as u64;
                    changed.insert(fake);
                    final_rows.push((fake, r.clone()));
                }
                Effect::Update(id, _, new) => {
                    changed.insert(*id);
                    final_rows.push((*id, new.clone()));
                }
                Effect::Delete(..) => {}
            }
        }
        check_uniques(&plan.rules, &final_rows, &changed, &ctx)?;
        (m.effects, m.returning)
    };
    let (effects, returning) = effects;
    let count = effects.len() as u64;
    let (mut new_versions, mut updated_old, mut deleted) = (Vec::new(), Vec::new(), Vec::new());
    for e in effects {
        match e {
            Effect::Insert(row) => {
                new_versions.push(row.clone());
                txn.insert(table.oid, row);
            }
            Effect::Update(id, old, new) => {
                new_versions.push(new.clone());
                updated_old.push(old);
                txn.update(table.oid, id, new);
            }
            Effect::Delete(id, old) => {
                deleted.push(old);
                txn.delete(table.oid, id);
            }
        }
    }
    let final_rows: Vec<(RowId, Row)> = txn
        .scan_latest(table.oid)
        .into_iter()
        .map(|(id, mut r)| {
            pad(&mut r, &table);
            (id, r)
        })
        .collect();
    check_foreign_keys(&table, &new_versions.iter().collect::<Vec<_>>(), txn, Some(&final_rows))?;
    referential_actions(txn, &table, &updated_old, false, 0)?;
    referential_actions(txn, &table, &deleted, true, 0)?;
    Ok(DmlResult { count, rows: returning })
}
