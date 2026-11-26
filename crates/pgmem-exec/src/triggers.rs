//! Triggers: CREATE / DROP TRIGGER, ENABLE / DISABLE, and firing row- and statement-level
//! triggers around INSERT, UPDATE and DELETE (`trigger.c`). Trigger functions see the live
//! columns of the row as NEW / OLD.

use crate::eval::StatementState;
use crate::functions::Exec;
use crate::plan::Row;
use crate::plpgsql::TriggerCall;
use crate::session::{Notice, QueryResult};
use pgmem_catalog::{FuncReturn, FunctionDef, Oid, Relation, TableDef, TriggerDef, TriggerTiming};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Insert,
    Update,
    Delete,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Event::Insert => "INSERT",
            Event::Update => "UPDATE",
            Event::Delete => "DELETE",
        }
    }
}

fn timing(t: ir::TriggerTiming) -> TriggerTiming {
    match t {
        ir::TriggerTiming::Before => TriggerTiming::Before,
        ir::TriggerTiming::After => TriggerTiming::After,
        ir::TriggerTiming::InsteadOf => TriggerTiming::InsteadOf,
    }
}

/// CREATE [OR REPLACE] TRIGGER.
pub(crate) fn create_trigger(ct: &ir::CreateTrigger, txn: &mut Txn) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let rel = catalog.lookup_relation(&ct.table)?.ok_or_else(|| SqlError::undefined_table(&ct.table.to_string()))?;
    let base = ct.table.base();
    match (&rel, ct.timing) {
        (Relation::Table(t), ir::TriggerTiming::InsteadOf) if t.matview.is_none() => {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is a table"))
                .with_detail("Tables cannot have INSTEAD OF triggers."));
        }
        (Relation::View(_), ir::TriggerTiming::Before | ir::TriggerTiming::After) if ct.for_each_row => {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is a view"))
                .with_detail("Views cannot have row-level BEFORE or AFTER triggers."));
        }
        (Relation::Table(_) | Relation::View(_), _) => {}
        (other, _) => {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("relation \"{base}\" cannot have triggers"))
                .with_detail(format!("This operation is not supported for {}s.", other.kind_name())));
        }
    }
    if ct.timing == ir::TriggerTiming::InsteadOf && !ct.for_each_row {
        return Err(SqlError::feature_not_supported("INSTEAD OF triggers must be FOR EACH ROW"));
    }
    if ct.timing == ir::TriggerTiming::InsteadOf && ct.when.is_some() {
        return Err(SqlError::feature_not_supported("INSTEAD OF triggers cannot have WHEN conditions"));
    }
    let func = catalog
        .find_functions(&ct.function)
        .into_iter()
        .find(|f| f.input_types().is_empty())
        .ok_or_else(|| SqlError::undefined_function(format!("function {}() does not exist", ct.function)))?;
    if func.ret != FuncReturn::Scalar(Type::Unknown) {
        return Err(SqlError::new(code::INVALID_OBJECT_DEFINITION, format!("function {} must return type trigger", ct.function)));
    }
    let def = TriggerDef {
        oid: 0,
        name: ct.name.clone(),
        table: rel.oid(),
        timing: timing(ct.timing),
        insert: ct.insert,
        delete: ct.delete,
        truncate: ct.truncate,
        update: ct.update.clone(),
        for_each_row: ct.for_each_row,
        when: ct.when.clone(),
        function: func.oid,
        args: ct.args.clone(),
        enabled: 'O',
        parent: 0,
    };
    install(txn, def, base, ct.or_replace, false)?;
    Ok(QueryResult { tag: "CREATE TRIGGER".into(), ..Default::default() })
}

/// Adds `def` to relation `rel_name` (replacing a same-name trigger with OR REPLACE) and, for a
/// row trigger on a partitioned table, clones it to every partition (`CreateTriggerFiringOn`).
/// Clones (`in_partition`) and the partitioned table's trigger share name, function, events and
/// WHEN condition; `UPDATE OF` columns and WHEN refer to columns by name, so they hold whatever
/// the partition's column order.
fn install(txn: &mut Txn, mut def: TriggerDef, rel_name: &str, or_replace: bool, in_partition: bool) -> SqlResult<Oid> {
    let existing = txn.catalog().triggers().find(|t| t.table == def.table && t.name == def.name).cloned();
    def.oid = match existing {
        Some(_) if !or_replace => {
            return Err(SqlError::new(
                code::DUPLICATE_OBJECT,
                format!("trigger \"{}\" for relation \"{rel_name}\" already exists", def.name),
            ));
        }
        Some(t) if t.parent != 0 && !in_partition => {
            return Err(SqlError::new(
                code::DUPLICATE_OBJECT,
                format!("trigger \"{}\" for relation \"{rel_name}\" is an internal or a child trigger", def.name),
            ));
        }
        Some(t) => t.oid,
        None => txn.catalog_mut().alloc_oid(),
    };
    let oid = def.oid;
    let (table, row) = (def.table, def.for_each_row);
    txn.catalog_mut().put_trigger(def.clone());
    if row && txn.catalog().is_partitioned(table) {
        let parts: Vec<Arc<TableDef>> = txn.catalog().partitions(table).into_iter().map(|(t, _)| t).collect();
        for part in parts {
            install(txn, TriggerDef { table: part.oid, parent: oid, ..def.clone() }, &part.name, or_replace, true)?;
        }
    }
    Ok(oid)
}

/// Clones the row triggers of partitioned table `parent` to its new partition `part` (and on
/// to `part`'s own partitions), with their enabled state (`CloneRowTriggersToPartition`).
pub(crate) fn clone_to_partition(txn: &mut Txn, parent: Oid, part: &TableDef) -> SqlResult<()> {
    for t in txn.catalog().table_triggers(parent) {
        if t.for_each_row {
            install(txn, TriggerDef { oid: 0, table: part.oid, parent: t.oid, ..(*t).clone() }, &part.name, false, true)?;
        }
    }
    Ok(())
}

/// Drops the triggers cloned from `trigger`, at every partition level.
fn drop_clones_of(txn: &mut Txn, trigger: Oid) {
    let clones: Vec<Oid> = txn.catalog().triggers().filter(|t| t.parent == trigger).map(|t| t.oid).collect();
    for c in clones {
        txn.catalog_mut().drop_trigger(c);
        drop_clones_of(txn, c);
    }
}

/// DETACH PARTITION: the partition's cloned triggers (and their clones below it) go
/// (`DropClonedTriggersFromPartition`).
pub(crate) fn drop_partition_clones(txn: &mut Txn, part: Oid) {
    let clones: Vec<Oid> = txn.catalog().triggers().filter(|t| t.table == part && t.parent != 0).map(|t| t.oid).collect();
    for c in clones {
        txn.catalog_mut().drop_trigger(c);
        drop_clones_of(txn, c);
    }
}

/// The table of the trigger `t` was cloned from.
fn parent_table_name(txn: &Txn, t: &TriggerDef) -> String {
    let cat = txn.catalog();
    cat.triggers().find(|p| p.oid == t.parent).and_then(|p| cat.table_by_oid(p.table)).map(|p| p.name.clone()).unwrap_or_default()
}

/// DROP TRIGGER [IF EXISTS] name ON table.
pub(crate) fn drop_trigger(name: &str, table: &ir::ObjectName, if_exists: bool, txn: &mut Txn) -> SqlResult<QueryResult> {
    let base = table.base();
    let rel = match txn.catalog().lookup_relation(table) {
        Ok(Some(r)) => r,
        Ok(None) | Err(_) if if_exists => {
            return Ok(QueryResult {
                tag: "DROP TRIGGER".into(),
                notices: vec![Notice {
                    severity: "NOTICE",
                    code: "00000",
                    message: format!("relation \"{base}\" does not exist, skipping"),
                }],
                ..Default::default()
            });
        }
        Ok(None) => return Err(SqlError::undefined_table(&table.to_string())),
        Err(e) => return Err(e),
    };
    let found = txn.catalog().triggers().find(|t| t.table == rel.oid() && t.name == name).cloned();
    match found {
        Some(t) if t.parent != 0 => {
            let parent = parent_table_name(txn, &t);
            Err(SqlError::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop trigger {name} on table {base} because trigger {name} on table {parent} requires it"),
            )
            .with_hint(format!("You can drop trigger {name} on table {parent} instead.")))
        }
        Some(t) => {
            txn.catalog_mut().drop_trigger(t.oid);
            drop_clones_of(txn, t.oid);
            Ok(QueryResult { tag: "DROP TRIGGER".into(), ..Default::default() })
        }
        None if if_exists => Ok(QueryResult {
            tag: "DROP TRIGGER".into(),
            notices: vec![Notice {
                severity: "NOTICE",
                code: "00000",
                message: format!("trigger \"{name}\" for relation \"{base}\" does not exist, skipping"),
            }],
            ..Default::default()
        }),
        None => Err(SqlError::new(code::UNDEFINED_OBJECT, format!("trigger \"{name}\" for table \"{base}\" does not exist"))),
    }
}

/// `ALTER TRIGGER name ON table RENAME TO new_name`.
pub(crate) fn rename_trigger(name: &str, table: &ir::ObjectName, new_name: &str, txn: &mut Txn) -> SqlResult<QueryResult> {
    let rel = txn.catalog().lookup_relation(table)?.ok_or_else(|| SqlError::undefined_table(&table.to_string()))?;
    let base = rel.name().to_owned();
    let t = txn
        .catalog()
        .triggers()
        .find(|t| t.table == rel.oid() && t.name == name)
        .cloned()
        .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("trigger \"{name}\" for table \"{base}\" does not exist")))?;
    if t.parent != 0 {
        let parent = parent_table_name(txn, &t);
        return Err(SqlError::feature_not_supported(format!("cannot rename trigger \"{name}\" on table \"{base}\""))
            .with_hint(format!("Rename the trigger on the partitioned table \"{parent}\" instead.")));
    }
    rename_one(txn, &t, &base, new_name)?;
    Ok(QueryResult { tag: "ALTER TRIGGER".into(), ..Default::default() })
}

/// Renames trigger `t` of relation `rel_name`, then its clones on the partitions
/// (`renametrig_partition`).
fn rename_one(txn: &mut Txn, t: &TriggerDef, rel_name: &str, new_name: &str) -> SqlResult<()> {
    if t.name != new_name && txn.catalog().triggers().any(|o| o.table == t.table && o.name == new_name) {
        return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("trigger \"{new_name}\" for relation \"{rel_name}\" already exists")));
    }
    txn.catalog_mut().put_trigger(TriggerDef { name: new_name.to_owned(), ..t.clone() });
    let clones: Vec<Arc<TriggerDef>> = txn.catalog().triggers().filter(|c| c.parent == t.oid).cloned().collect();
    for c in clones {
        let part = txn.catalog().table_by_oid(c.table).map(|p| p.name.clone()).unwrap_or_default();
        rename_one(txn, &c, &part, new_name)?;
    }
    Ok(())
}

/// Sets the enabled state of the clones of `trigger`, at every partition level.
fn set_clones_enabled(txn: &mut Txn, trigger: Oid, enabled: char) {
    let clones: Vec<Arc<TriggerDef>> = txn.catalog().triggers().filter(|c| c.parent == trigger).cloned().collect();
    for c in clones {
        txn.catalog_mut().put_trigger(TriggerDef { enabled, ..(*c).clone() });
        set_clones_enabled(txn, c.oid, enabled);
    }
}

/// `ALTER TABLE ... {ENABLE [REPLICA | ALWAYS] | DISABLE} TRIGGER {name | ALL | USER}`.
pub(crate) fn set_enabled(table: &TableDef, name: Option<&str>, enabled: char, txn: &mut Txn) -> SqlResult<()> {
    let triggers = txn.catalog().table_triggers(table.oid);
    let mut any = false;
    for t in triggers {
        if name.is_none_or(|n| n == t.name) {
            any = true;
            txn.catalog_mut().put_trigger(TriggerDef { enabled, ..(*t).clone() });
            // On a partitioned table the row triggers' clones follow (`EnableDisableTrigger`).
            if t.for_each_row {
                set_clones_enabled(txn, t.oid, enabled);
            }
        }
    }
    if let Some(n) = name
        && !any
    {
        return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("trigger \"{n}\" for table \"{}\" does not exist", table.name)));
    }
    Ok(())
}

/// The enabled triggers of a table for an event, in firing order, with their functions.
pub(crate) fn load(
    txn: &Txn,
    table: &TableDef,
    event: Event,
    when: TriggerTiming,
    row: bool,
    updated: Option<&[String]>,
    env: &crate::session::SessionEnv,
) -> Vec<(Arc<TriggerDef>, Arc<FunctionDef>)> {
    let cat = txn.catalog();
    // `session_replication_role = replica` fires 'R' and 'A' triggers; origin and local fire
    // 'O' and 'A'; 'D' never fires (`TriggerEnabled`).
    let replica = env.setting("session_replication_role").is_some_and(|r| r.eq_ignore_ascii_case("replica"));
    let fires = |mode: char| mode == 'A' || mode == if replica { 'R' } else { 'O' };
    cat.table_triggers(table.oid)
        .into_iter()
        .filter(|t| fires(t.enabled) && t.timing == when && t.for_each_row == row)
        .filter(|t| match event {
            Event::Insert => t.insert,
            Event::Delete => t.delete,
            // `UPDATE OF cols` fires only when one of them is a SET target.
            Event::Update => {
                t.update.as_ref().is_some_and(|cols| cols.is_empty() || updated.is_none_or(|u| cols.iter().any(|c| u.contains(c))))
            }
        })
        .filter_map(|t| cat.function_by_oid(t.function).cloned().map(|f| (t, f)))
        .collect()
}

fn live(table: &TableDef, row: &Row) -> Row {
    table.live_columns().map(|(i, _)| row.get(i).cloned().unwrap_or(Value::Null)).collect()
}

fn to_physical(table: &TableDef, live_row: Row, template: &Row) -> Row {
    let mut out = template.clone();
    for ((i, _), v) in table.live_columns().zip(live_row) {
        if let Some(slot) = out.get_mut(i) {
            *slot = v;
        }
    }
    out
}

fn timing_word(t: TriggerTiming) -> &'static str {
    match t {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
        TriggerTiming::InsteadOf => "INSTEAD OF",
    }
}

/// Fires row-level triggers on one row. For BEFORE (and INSTEAD OF) triggers the result is
/// the row to use (None: the operation is skipped for this row); AFTER triggers' results
/// are ignored. Rows are physical (dropped column slots included).
pub(crate) fn fire_row(
    triggers: &[(Arc<TriggerDef>, Arc<FunctionDef>)],
    table: &TableDef,
    event: Event,
    new: Option<Row>,
    old: Option<&Row>,
    exec: &mut Exec,
) -> SqlResult<Option<Row>> {
    let columns: Vec<(String, Type)> = table.live_columns().map(|(_, c)| (c.name.clone(), c.ty.clone())).collect();
    let template = new.clone().or_else(|| old.cloned()).unwrap_or_default();
    let mut current = new.as_ref().map(|r| live(table, r));
    let old_live = old.map(|r| live(table, r));
    for (t, f) in triggers {
        let call = TriggerCall {
            name: &t.name,
            table: &table.name,
            schema: &table.schema,
            when: timing_word(t.timing),
            op: event.name(),
            level: "ROW",
            columns: columns.clone(),
            new: current.clone(),
            old: old_live.clone(),
            args: &t.args,
        };
        if let Some(cond) = &t.when
            && !crate::plpgsql::eval_when(cond, &call, exec)?
        {
            continue;
        }
        let result = crate::plpgsql::run_trigger(f, &call, exec).map_err(SqlError::without_position)?;
        if t.timing == TriggerTiming::After {
            continue;
        }
        match result {
            // RETURN NULL skips the row (and the remaining triggers).
            None => return Ok(None),
            Some(row) => {
                if event == Event::Delete {
                    continue;
                }
                current = Some(row);
            }
        }
    }
    Ok(match event {
        Event::Delete => old.cloned(),
        _ => current.map(|r| to_physical(table, r, &template)),
    })
}

/// Runs `f`, a statement on leaf partition `leaf` that a statement on a partitioned table
/// became, with the leaf's statement-level triggers suppressed: only the partitioned table's
/// fire (`root_statement`).
pub(crate) fn routed<R>(state: &StatementState, leaf: Oid, f: impl FnOnce() -> R) -> R {
    state.routed_leaf.set(Some(leaf));
    let r = f();
    state.routed_leaf.set(None);
    r
}

/// Fires the statement-level triggers of the partitioned table a statement targets.
pub(crate) fn root_statement(
    txn: &mut Txn,
    root: &TableDef,
    event: Event,
    when: TriggerTiming,
    updated: Option<&[String]>,
    env: &crate::session::SessionEnv,
    state: &StatementState,
) -> SqlResult<()> {
    let list = load(txn, root, event, when, false, updated, env);
    if list.is_empty() {
        return Ok(());
    }
    let mut exec = Exec::Write { txn, env, state };
    fire_statement(&list, root, event, &mut exec)
}

/// Fires statement-level triggers.
pub(crate) fn fire_statement(
    triggers: &[(Arc<TriggerDef>, Arc<FunctionDef>)],
    table: &TableDef,
    event: Event,
    exec: &mut Exec,
) -> SqlResult<()> {
    let columns: Vec<(String, Type)> = table.live_columns().map(|(_, c)| (c.name.clone(), c.ty.clone())).collect();
    for (t, f) in triggers {
        let call = TriggerCall {
            name: &t.name,
            table: &table.name,
            schema: &table.schema,
            when: timing_word(t.timing),
            op: event.name(),
            level: "STATEMENT",
            columns: columns.clone(),
            new: None,
            old: None,
            args: &t.args,
        };
        crate::plpgsql::run_trigger(f, &call, exec).map_err(SqlError::without_position)?;
    }
    Ok(())
}

/// The INSTEAD OF row triggers of view `view` for `event`.
fn instead_of_triggers(
    txn: &Txn,
    view: &pgmem_catalog::ViewDef,
    event: Event,
    env: &crate::session::SessionEnv,
) -> Vec<(Arc<TriggerDef>, Arc<FunctionDef>)> {
    let pseudo = view_table(txn, view);
    load(txn, &pseudo, event, TriggerTiming::InsteadOf, true, None, env)
}

/// A view as a table definition (its columns typed by its query), for trigger calls.
fn view_table(txn: &Txn, view: &pgmem_catalog::ViewDef) -> TableDef {
    let catalog = txn.catalog().clone();
    let binder = crate::bind::Binder::new(&catalog, &[]);
    let top = crate::bind::Level::new(crate::bind::Scope::default());
    let types: Vec<Type> = pgmem_types::context::with_search_path(&view.search_path, || {
        binder.bind_query(&view.query, &crate::bind::Ctx::new(&top)).map(|b| b.columns.into_iter().map(|c| c.ty).collect())
    })
    .unwrap_or_default();
    let columns = view
        .columns
        .iter()
        .zip(types.into_iter().chain(std::iter::repeat(Type::Text)))
        .map(|(n, t)| pgmem_catalog::ColumnDef::new(n.clone(), t))
        .collect();
    TableDef {
        oid: view.oid,
        schema: view.schema.clone(),
        name: view.name.clone(),
        columns,
        constraints: Vec::new(),
        indexes: Vec::new(),
        matview: None,
        unlogged: false,
        replident: 'd',
        reloptions: Vec::new(),
    }
}

fn select_from(view: &ir::ObjectName, alias: Option<&String>, extra: Vec<ir::Expr>, selection: Option<ir::Expr>) -> ir::Query {
    let mut projection = vec![ir::SelectItem::Wildcard];
    projection.extend(extra.into_iter().map(|expr| ir::SelectItem::Expr { expr, alias: None }));
    let alias = alias.map(|a| ir::TableAlias { name: a.clone(), columns: Vec::new(), column_types: Vec::new() });
    ir::Query::from_body(ir::SetExpr::Select(Box::new(ir::Select {
        projection,
        from: vec![ir::TableRef::Table { name: view.clone(), alias }],
        selection,
        ..Default::default()
    })))
}

/// INSERT / UPDATE / DELETE on a view with an INSTEAD OF trigger: the trigger runs for each
/// row instead (Some(result)); None when the statement has no such target.
pub(crate) fn instead_of(
    stmt: &ir::Statement,
    txn: &mut Txn,
    params: &[Value],
    env: &crate::session::SessionEnv,
    state: &crate::eval::StatementState,
) -> SqlResult<Option<QueryResult>> {
    let (name, event) = match stmt {
        ir::Statement::Insert(i) => (&i.table, Event::Insert),
        ir::Statement::Update(u) => (&u.table, Event::Update),
        ir::Statement::Delete(d) => (&d.table, Event::Delete),
        _ => return Ok(None),
    };
    let Ok(Some(Relation::View(view))) = txn.catalog().find_relation(name) else { return Ok(None) };
    let trigs = instead_of_triggers(txn, &view, event, env);
    if trigs.is_empty() {
        return Ok(None);
    }
    let table = view_table(txn, &view);
    let width = table.columns.len();
    let catalog = txn.catalog().clone();
    let binder = crate::bind::Binder::new(&catalog, &[]);
    let top = crate::bind::Level::new(crate::bind::Scope::default());
    let ctx = crate::bind::Ctx::new(&top);
    // The (old, new) row pairs the trigger runs for.
    let pairs: Vec<(Option<Row>, Option<Row>)> = {
        let ectx = crate::eval::EvalCtx::new(txn, params, env, state);
        match stmt {
            ir::Statement::Insert(i) => {
                let slots: Vec<usize> = if i.columns.is_empty() {
                    (0..width).collect()
                } else {
                    i.columns
                        .iter()
                        .map(|c| {
                            table
                                .column_index(c)
                                .ok_or_else(|| SqlError::undefined_column(&format!("\"{c}\" of relation \"{}\"", view.name)))
                        })
                        .collect::<SqlResult<_>>()?
                };
                let sources: Vec<Row> = match &i.source {
                    ir::InsertSource::DefaultValues => vec![Vec::new()],
                    ir::InsertSource::Values(rows) => {
                        let mut out = Vec::new();
                        for r in rows {
                            let mut vals = Vec::new();
                            for (e, slot) in r.iter().zip(&slots) {
                                if matches!(e, ir::Expr::Default) {
                                    vals.push(Value::Null);
                                    continue;
                                }
                                let (b, t) = binder.bind_expr(e, &ctx)?;
                                let b = crate::bind::coerce(
                                    &binder,
                                    b,
                                    &t,
                                    &table.columns[*slot].ty,
                                    crate::cast::CoercionContext::Assignment,
                                )?;
                                vals.push(crate::eval::eval(&b, &[], &ectx)?);
                            }
                            out.push(vals);
                        }
                        out
                    }
                    ir::InsertSource::Query(q) => {
                        let bound = binder.bind_query(q, &ctx)?;
                        crate::run::run(&bound.rel, &ectx)?
                    }
                };
                sources
                    .into_iter()
                    .map(|src| {
                        let mut row = vec![Value::Null; width];
                        for (v, slot) in src.into_iter().zip(&slots) {
                            row[*slot] = v;
                        }
                        (None, Some(row))
                    })
                    .collect()
            }
            ir::Statement::Update(u) => {
                let mut targets = Vec::new();
                let mut exprs = Vec::new();
                for a in &u.assignments {
                    for c in &a.columns {
                        let slot = table
                            .column_index(&c[0])
                            .ok_or_else(|| SqlError::undefined_column(&format!("\"{}\" of relation \"{}\"", c[0], view.name)))?;
                        targets.push(slot);
                    }
                    // `SET (a, b) = (x, y)` sets each column; DEFAULT is the view column's
                    // default, which a view without one has as NULL.
                    let values = match &a.value {
                        ir::Expr::Row(items) if a.columns.len() > 1 && items.len() == a.columns.len() => items.clone(),
                        v => vec![v.clone()],
                    };
                    exprs.extend(values.into_iter().map(|e| match e {
                        ir::Expr::Default => ir::Expr::Literal(ir::Literal::Null),
                        e => e,
                    }));
                }
                let q = select_from(&u.table, u.alias.as_ref(), exprs, u.selection.clone());
                let bound = binder.bind_query(&q, &ctx)?;
                crate::run::run(&bound.rel, &ectx)?
                    .into_iter()
                    .map(|r| {
                        let old: Row = r[..width].to_vec();
                        let mut new = old.clone();
                        for (slot, v) in targets.iter().zip(r[width..].iter()) {
                            new[*slot] = v.clone();
                        }
                        (Some(old), Some(new))
                    })
                    .collect()
            }
            ir::Statement::Delete(d) => {
                let q = select_from(&d.table, d.alias.as_ref(), Vec::new(), d.selection.clone());
                let bound = binder.bind_query(&q, &ctx)?;
                crate::run::run(&bound.rel, &ectx)?.into_iter().map(|r| (Some(r), None)).collect()
            }
            _ => Vec::new(),
        }
    };
    let mut count = 0u64;
    for (old, new) in pairs {
        let mut exec = Exec::Write { txn, env, state };
        let result = fire_row(&trigs, &table, event, new, old.as_ref(), &mut exec)?;
        if result.is_some() {
            count += 1;
        }
    }
    let tag = match event {
        Event::Insert => format!("INSERT 0 {count}"),
        Event::Update => format!("UPDATE {count}"),
        Event::Delete => format!("DELETE {count}"),
    };
    Ok(Some(QueryResult { tag, ..Default::default() }))
}
