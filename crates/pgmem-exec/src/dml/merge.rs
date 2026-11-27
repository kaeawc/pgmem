//! MERGE execution (Postgres' `ExecMerge`): each source row is joined with the target rows
//! it matches; the first WHEN clause of the matching kind whose condition holds decides the
//! action. Effects are computed against the statement's snapshot, validated, then applied.
//!
//! A partitioned target's rows are its leaf partitions': rows are handled in the target's
//! layout, inserted rows are routed to their leaf, and an UPDATE whose new row belongs to
//! another partition moves it there.
//!
//! Triggers fire as in Postgres: the target's statement triggers for every action type the
//! WHEN clauses list (BEFORE: INSERT, UPDATE, DELETE; AFTER: DELETE, UPDATE, INSERT), and the
//! row triggers of the leaf each row is stored in (a move is a DELETE from one leaf and an
//! INSERT into the other). BEFORE row triggers may change the new row or skip it.

use super::{
    Checks, DmlResult, apply_generated, check_checks, check_foreign_keys, check_not_null, check_uniques, identity_always_error, pad,
    referential_actions, returning_row, system_values,
};
use crate::bind::merge::{MergeActionPlan, MergeClausePlan, MergePlan};
use crate::bind::{Binder, SYSTEM_COLUMNS};
use crate::eval::{EvalCtx, StatementState, eval, eval_pred};
use crate::functions::Exec;
use crate::partition::{constraint_error, remap, route};
use crate::plan::Row;
use crate::run::run;
use crate::session::SessionEnv;
use crate::triggers::{self, Event};
use pgmem_catalog::{FunctionDef, Identity, Oid, TableDef, TriggerDef, TriggerTiming};
use pgmem_ir::MergeMatch;
use pgmem_storage::{RowId, Txn};
use pgmem_types::{SqlError, SqlResult, Value, code};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

type TriggerList = Vec<(Arc<TriggerDef>, Arc<FunctionDef>)>;

/// The triggers a MERGE fires: the target's statement triggers for each action type listed,
/// and the row triggers of each leaf, loaded on first use.
struct MergeTriggers {
    /// The action types the WHEN clauses list, in BEFORE STATEMENT order.
    events: Vec<Event>,
    /// The columns of every UPDATE action: what `UPDATE OF` matches.
    updated: Vec<String>,
    rows: RefCell<HashMap<(Oid, Event, bool), Rc<TriggerList>>>,
}

impl MergeTriggers {
    fn new(plan: &MergePlan) -> MergeTriggers {
        let table = &plan.rules.table;
        let mut updated: Vec<String> = Vec::new();
        let mut listed = [false; 3];
        for c in &plan.clauses {
            match &c.action {
                MergeActionPlan::Insert { .. } => listed[0] = true,
                MergeActionPlan::Update(assignments) => {
                    listed[1] = true;
                    for (slot, _) in assignments {
                        let name = &table.columns[*slot].name;
                        if !updated.contains(name) {
                            updated.push(name.clone());
                        }
                    }
                }
                MergeActionPlan::Delete => listed[2] = true,
                MergeActionPlan::DoNothing => {}
            }
        }
        let events = [Event::Insert, Event::Update, Event::Delete].into_iter().zip(listed).filter(|(_, l)| *l).map(|(e, _)| e).collect();
        MergeTriggers { events, updated, rows: RefCell::default() }
    }

    fn load(&self, txn: &Txn, table: &TableDef, event: Event, when: TriggerTiming, row: bool, env: &SessionEnv) -> TriggerList {
        triggers::load(txn, table, event, when, row, (event == Event::Update).then_some(self.updated.as_slice()), env)
    }

    /// `leaf`'s BEFORE (or AFTER) row triggers for `event`.
    fn row(&self, txn: &Txn, target: &Arc<TableDef>, leaf: Oid, event: Event, before: bool, env: &SessionEnv) -> Rc<TriggerList> {
        let key = (leaf, event, before);
        if let Some(list) = self.rows.borrow().get(&key) {
            return Rc::clone(list);
        }
        let when = if before { TriggerTiming::Before } else { TriggerTiming::After };
        let list = Rc::new(self.load(txn, &leaf_def(txn, target, leaf), event, when, true, env));
        self.rows.borrow_mut().insert(key, Rc::clone(&list));
        list
    }

    /// The target's statement triggers: BEFORE in INSERT, UPDATE, DELETE order, AFTER in reverse.
    fn statement(&self, when: TriggerTiming, table: &TableDef, txn: &mut Txn, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
        let mut events = self.events.clone();
        if when == TriggerTiming::After {
            events.reverse();
        }
        for event in events {
            let list = self.load(txn, table, event, when, false, env);
            if !list.is_empty() {
                triggers::fire_statement(&list, table, event, &mut Exec::Write { txn, env, state })?;
            }
        }
        Ok(())
    }

    /// Fires the AFTER row triggers of each applied change (rows in their leaf's layout).
    fn after_rows(
        &self,
        changes: Vec<AfterRow>,
        target: &Arc<TableDef>,
        txn: &mut Txn,
        env: &SessionEnv,
        state: &StatementState,
    ) -> SqlResult<()> {
        for (leaf, event, new, old) in changes {
            let list = self.row(txn, target, leaf, event, false, env);
            if list.is_empty() {
                continue;
            }
            let def = leaf_def(txn, target, leaf);
            triggers::fire_row(&list, &def, event, new, old.as_ref(), &mut Exec::Write { txn, env, state })?;
        }
        Ok(())
    }
}

/// An applied change for the AFTER row triggers: (leaf, event, new row, old row).
type AfterRow = (Oid, Event, Option<Row>, Option<Row>);

/// A change, with rows in the target's layout and the leaf partition (the target itself when
/// it is not partitioned) that stores them.
enum Effect {
    Insert(Oid, Row),
    Update(Oid, RowId, Row, Row),
    Delete(Oid, RowId, Row),
    /// An UPDATE whose new row belongs to another leaf: deleted from `from`, inserted into `to`.
    Move {
        from: Oid,
        id: RowId,
        to: Oid,
        old: Row,
        new: Row,
    },
}

impl Effect {
    /// The leaf a new row version goes to, with its row id (`None` for an insert).
    fn new_version(&self) -> Option<(Oid, Option<RowId>, &Row)> {
        match self {
            Effect::Insert(oid, r) => Some((*oid, None, r)),
            Effect::Update(oid, id, _, new) => Some((*oid, Some(*id), new)),
            Effect::Move { to, new, .. } => Some((*to, None, new)),
            Effect::Delete(..) => None,
        }
    }
}

/// A stored target row: (leaf, row id, row in the target's layout).
type Target = (Oid, RowId, Row);

/// A row of `target`'s layout in `leaf`'s.
fn leaf_row(txn: &Txn, target: &TableDef, leaf: Oid, row: &Row) -> Row {
    match txn.catalog().table_by_oid(leaf) {
        Some(l) if l.oid != target.oid => remap(row, target, l),
        _ => row.clone(),
    }
}

fn leaf_def(txn: &Txn, target: &Arc<TableDef>, leaf: Oid) -> Arc<TableDef> {
    txn.catalog().table_by_oid(leaf).map_or_else(|| Arc::clone(target), Arc::clone)
}

struct Merger<'a, 'c> {
    plan: &'a MergePlan,
    ctx: &'a EvalCtx<'c>,
    triggers: &'a MergeTriggers,
    /// The target's physical width; its system columns follow in the combined row.
    width: usize,
    effects: Vec<Effect>,
    returning: Vec<Row>,
    /// Target rows already updated or deleted by this command.
    touched: HashSet<(Oid, RowId)>,
}

impl<'a> Merger<'a, '_> {
    /// Where the combined row's source columns start.
    fn source_start(&self) -> usize {
        self.width + SYSTEM_COLUMNS.len()
    }

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

    /// The system columns of the target row in `combined` (target ++ system ++ source).
    fn system<'r>(&self, combined: &'r Row) -> &'r [Value] {
        &combined[self.width..self.source_start()]
    }

    fn emit(&mut self, action: &'static str, current: &Row, old: Option<(&Row, &[Value])>, new: Option<(&Row, &[Value])>) -> SqlResult<()> {
        if self.plan.returning.is_empty() {
            return Ok(());
        }
        self.ctx.state.merge_action.set(action);
        let r = returning_row(current, old, new, self.width);
        self.returning.push(self.plan.returning.iter().map(|e| eval(e, &r, self.ctx)).collect::<SqlResult<Vec<_>>>()?);
        Ok(())
    }

    /// `row ++ system columns of (leaf, id) ++ source`: the row RETURNING sees.
    fn current(&self, row: &Row, leaf: Oid, id: Option<RowId>, combined: &Row) -> Row {
        let mut out = row.clone();
        out.extend(system_values(leaf, id, 0));
        out.extend(combined[self.source_start()..].iter().cloned());
        out
    }

    /// The leaf partition a new row of the target (its layout) is stored in: the target's
    /// own partition constraint first, then routing below a partitioned target.
    fn place(&self, row: &Row) -> SqlResult<Oid> {
        let table = &self.plan.rules.table;
        let catalog = self.ctx.txn.catalog();
        if catalog.partition_of(table.oid).is_some() && !crate::partition::satisfies_constraint(catalog, table, row, self.ctx)? {
            return Err(constraint_error(table, row));
        }
        if catalog.is_partitioned(table.oid) {
            return Ok(route(catalog, &Binder::new(catalog, &[]), table, row, self.ctx)?.0.oid);
        }
        Ok(table.oid)
    }

    /// The leaf a new row of a partitioned target routes to (the target itself otherwise).
    fn route(&self, row: &Row) -> SqlResult<Oid> {
        let table = &self.plan.rules.table;
        let catalog = self.ctx.txn.catalog();
        if catalog.is_partitioned(table.oid) {
            return Ok(route(catalog, &Binder::new(catalog, &[]), table, row, self.ctx)?.0.oid);
        }
        Ok(table.oid)
    }

    /// [`Self::place`], which must be `leaf` (a BEFORE INSERT trigger of `leaf` may have
    /// changed the partition key).
    fn place_in(&self, leaf: Oid, row: &Row) -> SqlResult<()> {
        if self.place(row)? == leaf {
            return Ok(());
        }
        let (txn, table) = (self.ctx.txn, &self.plan.rules.table);
        let def = leaf_def(txn, table, leaf);
        Err(constraint_error(&def, &leaf_row(txn, table, leaf, row)))
    }

    /// Fires `leaf`'s BEFORE row triggers for `event` on rows of the target's layout: the row
    /// to use (the old one for DELETE), or `None` when a trigger skipped it.
    fn before_row(&self, leaf: Oid, event: Event, new: Option<Row>, old: Option<&Row>) -> SqlResult<Option<Row>> {
        let (txn, table) = (self.ctx.txn, &self.plan.rules.table);
        let list = self.triggers.row(txn, table, leaf, event, true, self.ctx.env);
        if list.is_empty() {
            return Ok(new.or_else(|| old.cloned()));
        }
        let def = leaf_def(txn, table, leaf);
        let to_leaf = |r: &Row| leaf_row(txn, table, leaf, r);
        let old_leaf = old.map(to_leaf);
        let out = triggers::fire_row(&list, &def, event, new.as_ref().map(to_leaf), old_leaf.as_ref(), &mut Exec::Read(self.ctx))?;
        Ok(out.map(|r| if def.oid == table.oid { r } else { remap(&r, &def, table) }))
    }

    /// The new row of an UPDATE of `leaf`'s row `old` after the BEFORE triggers, with the leaf
    /// it goes to; `None` when a trigger skipped it. A row moving to another leaf is deleted
    /// from its own (BEFORE DELETE triggers) and inserted into the other (BEFORE INSERT).
    fn updated_row(&self, leaf: Oid, old: &Row, new_row: Row) -> SqlResult<Option<(Oid, Row)>> {
        let Some(mut new_row) = self.before_row(leaf, Event::Update, Some(new_row), Some(old))? else { return Ok(None) };
        apply_generated(&self.plan.rules, &mut new_row, self.ctx)?;
        let to = self.place(&new_row)?;
        if to == leaf {
            return Ok(Some((to, new_row)));
        }
        if self.before_row(leaf, Event::Delete, None, Some(old))?.is_none() {
            return Ok(None);
        }
        let Some(mut new_row) = self.before_row(to, Event::Insert, Some(new_row), None)? else { return Ok(None) };
        apply_generated(&self.plan.rules, &mut new_row, self.ctx)?;
        self.place_in(to, &new_row)?;
        Ok(Some((to, new_row)))
    }

    /// WHEN MATCHED / NOT MATCHED BY SOURCE for target row `t` (`combined` = target ++ system
    /// columns ++ source).
    fn target_action(&mut self, kind: MergeMatch, t: &Target, combined: &Row) -> SqlResult<()> {
        let Some(clause) = self.clause(kind, combined)? else { return Ok(()) };
        let (leaf, id, target) = (t.0, t.1, &t.2);
        match &clause.action {
            MergeActionPlan::DoNothing | MergeActionPlan::Insert { .. } => return Ok(()),
            MergeActionPlan::Update(_) | MergeActionPlan::Delete if self.touched.contains(&(leaf, id)) => {
                return Err(SqlError::new(code::CARDINALITY_VIOLATION, "MERGE command cannot affect row a second time")
                    .with_hint("Ensure that not more than one source row matches any one target row."));
            }
            MergeActionPlan::Delete => {
                if self.before_row(leaf, Event::Delete, None, Some(target))?.is_none() {
                    return Ok(());
                }
                self.touched.insert((leaf, id));
                self.emit("DELETE", combined, Some((target, self.system(combined))), None)?;
                self.effects.push(Effect::Delete(leaf, id, target.clone()));
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
                let Some((to, new_row)) = self.updated_row(leaf, target, new_row)? else { return Ok(()) };
                self.touched.insert((leaf, id));
                check_not_null(&self.plan.rules.table, &new_row)?;
                check_checks(&self.plan.rules, &new_row, self.ctx)?;
                let current = self.current(&new_row, to, Some(id), combined);
                let new_system = self.system(&current).to_vec();
                self.emit("UPDATE", &current, Some((target, self.system(combined))), Some((&new_row, &new_system)))?;
                self.effects.push(if to == leaf {
                    Effect::Update(leaf, id, target.clone(), new_row)
                } else {
                    Effect::Move { from: leaf, id, to, old: target.clone(), new: new_row }
                });
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
        let leaf = self.route(&row)?;
        let Some(mut row) = self.before_row(leaf, Event::Insert, Some(row), None)? else { return Ok(()) };
        apply_generated(&self.plan.rules, &mut row, self.ctx)?;
        self.place_in(leaf, &row)?;
        check_not_null(&table, &row)?;
        check_checks(&self.plan.rules, &row, self.ctx)?;
        let current = self.current(&row, leaf, None, combined);
        let system = self.system(&current).to_vec();
        self.emit("INSERT", &current, None, Some((&row, &system)))?;
        self.effects.push(Effect::Insert(leaf, row));
        Ok(())
    }

    /// Unique validation over each receiving leaf's final state (constraints named after it).
    fn check_uniques(&self) -> SqlResult<()> {
        let txn = self.ctx.txn;
        let table = &self.plan.rules.table;
        let leaves: BTreeSet<Oid> = self.effects.iter().filter_map(|e| e.new_version().map(|v| v.0)).collect();
        for oid in leaves {
            let leaf = leaf_def(txn, table, oid);
            let rules = if oid == table.oid { self.plan.rules.clone() } else { Binder::new(txn.catalog(), &[]).bind_row_rules(&leaf)? };
            let mut final_rows: Vec<(RowId, Row)> = txn
                .scan_latest(oid)
                .into_iter()
                .filter(|(id, _)| !self.touched.contains(&(oid, *id)))
                .map(|(id, mut r)| {
                    pad(&mut r, &leaf);
                    (id, r)
                })
                .collect();
            let mut changed: HashSet<RowId> = HashSet::new();
            for (n, (_, id, row)) in self.effects.iter().filter_map(Effect::new_version).enumerate().filter(|(_, v)| v.0 == oid) {
                let id = id.unwrap_or(u64::MAX - n as u64);
                changed.insert(id);
                final_rows.push((id, leaf_row(txn, table, oid, row)));
            }
            check_uniques(&rules, &final_rows, &changed, self.ctx)?;
        }
        Ok(())
    }
}

/// The target's stored rows, in its layout (a partitioned target's from its leaves).
fn target_rows(txn: &Txn, table: &TableDef) -> Vec<Target> {
    crate::partition::scan(txn, table.oid).unwrap_or_else(|| {
        txn.scan(table.oid)
            .into_iter()
            .map(|(id, mut r)| {
                pad(&mut r, table);
                (table.oid, id, r)
            })
            .collect()
    })
}

/// Joins the source with the target and decides each row's action.
fn plan_effects(m: &mut Merger) -> SqlResult<()> {
    let (plan, ctx) = (m.plan, m.ctx);
    let sources = run(&plan.source, ctx)?;
    let targets = target_rows(ctx.txn, &plan.rules.table);
    let mut matched = vec![false; targets.len()];
    let with_system = |t: &Target| {
        let mut combined = t.2.clone();
        combined.extend(system_values(t.0, Some(t.1), 0));
        combined
    };
    for s in &sources {
        let mut any = false;
        for (i, t) in targets.iter().enumerate() {
            let mut combined = with_system(t);
            combined.extend(s.iter().cloned());
            if !eval_pred(&plan.on, &combined, ctx)? {
                continue;
            }
            any = true;
            matched[i] = true;
            m.target_action(MergeMatch::Matched, t, &combined)?;
        }
        if !any {
            let mut combined = vec![Value::Null; m.source_start()];
            combined.extend(s.iter().cloned());
            m.source_action(&combined)?;
        }
    }
    if plan.clauses.iter().any(|c| c.kind == MergeMatch::NotMatchedBySource) {
        for (i, t) in targets.iter().enumerate() {
            if !matched[i] {
                let mut combined = with_system(t);
                combined.extend(std::iter::repeat_n(Value::Null, plan.source_width));
                m.target_action(MergeMatch::NotMatchedBySource, t, &combined)?;
            }
        }
    }
    m.check_uniques()
}

/// One leaf's applied changes (rows in its layout), for the foreign key checks and actions.
#[derive(Default)]
struct LeafChanges {
    new_versions: Vec<Row>,
    updated_old: Vec<Row>,
    updated_new: Vec<Row>,
    deleted: Vec<Row>,
}

pub fn exec_merge(plan: &MergePlan, txn: &mut Txn, params: &[Value], env: &SessionEnv, state: &StatementState) -> SqlResult<DmlResult> {
    let table = Arc::clone(&plan.rules.table);
    let trig = MergeTriggers::new(plan);
    trig.statement(TriggerTiming::Before, &table, txn, env, state)?;
    let (effects, returning) = {
        let ctx = EvalCtx::new(txn, params, env, state);
        let mut m = Merger {
            plan,
            ctx: &ctx,
            triggers: &trig,
            width: table.columns.len(),
            effects: Vec::new(),
            returning: Vec::new(),
            touched: HashSet::new(),
        };
        plan_effects(&mut m)?;
        (m.effects, m.returning)
    };
    let count = effects.len() as u64;
    let mut leaves: BTreeMap<Oid, LeafChanges> = BTreeMap::new();
    // Rows moved between partitions: an update of the target's row (its layout).
    let (mut moved_old, mut moved_new) = (Vec::new(), Vec::new());
    let mut after: Vec<AfterRow> = Vec::new();
    for e in effects {
        match e {
            Effect::Insert(oid, row) => {
                let row = leaf_row(txn, &table, oid, &row);
                leaves.entry(oid).or_default().new_versions.push(row.clone());
                after.push((oid, Event::Insert, Some(row.clone()), None));
                txn.insert(oid, row);
            }
            Effect::Update(oid, id, old, new) => {
                let (old, new) = (leaf_row(txn, &table, oid, &old), leaf_row(txn, &table, oid, &new));
                let lc = leaves.entry(oid).or_default();
                lc.new_versions.push(new.clone());
                lc.updated_new.push(new.clone());
                lc.updated_old.push(old.clone());
                after.push((oid, Event::Update, Some(new.clone()), Some(old)));
                txn.update(oid, id, new);
            }
            Effect::Delete(oid, id, old) => {
                let old = leaf_row(txn, &table, oid, &old);
                leaves.entry(oid).or_default().deleted.push(old.clone());
                after.push((oid, Event::Delete, None, Some(old)));
                txn.delete(oid, id);
            }
            Effect::Move { from, id, to, old, new } => {
                let row = leaf_row(txn, &table, to, &new);
                leaves.entry(to).or_default().new_versions.push(row.clone());
                after.push((from, Event::Delete, None, Some(leaf_row(txn, &table, from, &old))));
                after.push((to, Event::Insert, Some(row.clone()), None));
                txn.delete(from, id);
                txn.insert(to, row);
                moved_old.push(old);
                moved_new.push(new);
            }
        }
    }
    let ck = Checks { env, state };
    for (oid, lc) in &leaves {
        let leaf = leaf_def(txn, &table, *oid);
        let final_rows: Vec<(RowId, Row)> = txn
            .scan_latest(*oid)
            .into_iter()
            .map(|(id, mut r)| {
                pad(&mut r, &leaf);
                (id, r)
            })
            .collect();
        check_foreign_keys(&leaf, &lc.new_versions.iter().collect::<Vec<_>>(), txn, Some(&final_rows), ck)?;
        referential_actions(txn, &leaf, &lc.updated_old, &lc.updated_new, false, 0, ck)?;
        referential_actions(txn, &leaf, &lc.deleted, &[], true, 0, ck)?;
    }
    referential_actions(txn, &table, &moved_old, &moved_new, false, 0, ck)?;
    trig.after_rows(after, &table, txn, env, state)?;
    trig.statement(TriggerTiming::After, &table, txn, env, state)?;
    Ok(DmlResult { count, rows: returning })
}
