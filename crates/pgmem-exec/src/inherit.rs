//! Table inheritance (`tablecmds.c` `MergeAttributes`, `ATExecAddInherit`, the recursing
//! ALTER TABLE actions): children created with `INHERITS` take their parents' columns, CHECK
//! and NOT NULL constraints and defaults; scans, UPDATE, DELETE, TRUNCATE and most ALTER TABLE
//! actions on a parent include its children unless `ONLY` is given. INSERT never routes, and
//! indexes, unique keys and foreign keys are not inherited.

use crate::bind::{Binder, Ctx};
use crate::eval::StatementState;
use crate::plan::Row;
use crate::session::{Notice, QueryResult, SessionEnv};
use crate::triggers::{Event, routed};
use pgmem_catalog::{Catalog, ColumnDef, Constraint, ConstraintKind, Inherited, Oid, QualName, Relation, TableDef, TriggerTiming};
use pgmem_ir::{self as ir, AlterTableAction as A, Statement};
use pgmem_storage::{RowId, Txn};
use pgmem_types::{SqlError, SqlResult, Value, code};
use std::sync::Arc;

/// What CREATE TABLE ... INHERITS records besides the table itself.
#[derive(Default)]
pub struct InheritPlan {
    pub parents: Vec<Oid>,
    /// (slot, inheritance) of the columns and of their NOT NULL constraints.
    pub columns: Vec<(usize, Inherited)>,
    pub not_nulls: Vec<(usize, Inherited)>,
    /// CHECK constraints by name.
    pub constraints: Vec<(String, Inherited)>,
    /// CHECK constraints declared NO INHERIT, by name.
    pub no_inherit: Vec<String>,
    pub notices: Vec<String>,
}

fn table_def_error(msg: impl Into<String>) -> SqlError {
    SqlError::new(code::INVALID_TABLE_DEFINITION, msg)
}

fn mismatch(msg: impl Into<String>) -> SqlError {
    SqlError::new(code::DATATYPE_MISMATCH, msg)
}

/// A NO INHERIT CHECK merging with an inherited one of the same name (`MergeWithExistingConstraint`).
fn conflicts_with_inherited(name: &str, table: &str) -> SqlError {
    SqlError::new(
        code::INVALID_OBJECT_DEFINITION,
        format!("constraint \"{name}\" conflicts with inherited constraint on relation \"{table}\""),
    )
}

/// An inherited CHECK merging with a NO INHERIT one of the same name.
fn conflicts_with_non_inherited(name: &str, what: &str, table: &str) -> SqlError {
    SqlError::new(
        code::INVALID_OBJECT_DEFINITION,
        format!("constraint \"{name}\" conflicts with non-inherited constraint on {what} \"{table}\""),
    )
}

/// `StoreRelCheck`: partitioned tables take no NO INHERIT CHECK constraints.
pub(crate) fn no_inherit_on_partitioned(table: &str) -> SqlError {
    SqlError::new(code::INVALID_TABLE_DEFINITION, format!("cannot add NO INHERIT constraint to partitioned table \"{table}\""))
}

fn type_text(catalog: &Catalog, c: &ColumnDef) -> String {
    crate::funcs::catalog::format_type(c.ty.oid(), c.typmod, catalog).unwrap_or_else(|| c.ty.to_string())
}

fn collation_text(c: &ColumnDef) -> String {
    format!("\"{}\"", c.collation.as_deref().unwrap_or("default"))
}

fn is_temp(t: &TableDef) -> bool {
    t.schema.starts_with("pg_temp")
}

/// The name of a column's NOT NULL constraint.
pub(crate) fn not_null_name(table: &TableDef, c: &ColumnDef) -> String {
    c.not_null_name.clone().unwrap_or_else(|| crate::catalog_views::not_null_constraint_name(&table.name, &c.name))
}

fn check_expr(c: &Constraint) -> Option<&ir::Expr> {
    match &c.kind {
        ConstraintKind::Check { expr } => Some(expr),
        _ => None,
    }
}

/// The CHECK constraints of `parent` its children inherit.
fn inheritable_checks<'a>(catalog: &'a Catalog, parent: &'a TableDef) -> impl Iterator<Item = &'a Constraint> {
    parent.constraints.iter().filter(|c| check_expr(c).is_some() && !catalog.is_no_inherit(c.oid))
}

// ---------------------------------------------------------------------------------------------
// CREATE TABLE ... INHERITS

/// The parents named in `INHERITS (...)`, with Postgres' checks.
fn resolve_parents(catalog: &Catalog, ct: &ir::CreateTable) -> SqlResult<Vec<Arc<TableDef>>> {
    let mut parents: Vec<Arc<TableDef>> = Vec::new();
    for name in &ct.options.inherits {
        let t = match catalog.lookup_relation(name)? {
            Some(Relation::Table(t)) if t.matview.is_none() => t,
            Some(r) => {
                return Err(SqlError::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("inherited relation \"{}\" is not a table or foreign table", r.name()),
                ));
            }
            None => return Err(SqlError::undefined_table(&name.to_string())),
        };
        let wrong = |what: &str| SqlError::new(code::WRONG_OBJECT_TYPE, format!("cannot inherit from {what} \"{}\"", t.name));
        if catalog.is_partitioned(t.oid) {
            return Err(wrong("partitioned table"));
        }
        if catalog.partition_of(t.oid).is_some() {
            return Err(wrong("partition"));
        }
        if is_temp(&t) && !ct.temporary {
            return Err(wrong("temporary relation"));
        }
        if parents.iter().any(|p| p.oid == t.oid) {
            return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{}\" would be inherited from more than once", t.name)));
        }
        parents.push(t);
    }
    if ct.options.partition_by.is_some() {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, "cannot create partitioned table as inheritance child"));
    }
    Ok(parents)
}

/// A column gathered from the parents.
struct InhCol {
    col: ColumnDef,
    count: i16,
    not_nulls: i16,
    default_conflict: bool,
    local: bool,
    local_not_null: bool,
}

fn add_parent_column(
    catalog: &Catalog,
    parent: &TableDef,
    pc: &ColumnDef,
    inh: &mut Vec<InhCol>,
    notices: &mut Vec<String>,
) -> SqlResult<()> {
    let Some(e) = inh.iter_mut().find(|e| e.col.name == pc.name) else {
        let mut col = pc.clone();
        if col.identity.take().is_some() {
            col.default = None;
        }
        col.missing = Value::Null;
        col.stattarget = None;
        col.not_null_name = pc.not_null.then(|| not_null_name(parent, pc));
        inh.push(InhCol { col, count: 1, not_nulls: i16::from(pc.not_null), default_conflict: false, local: false, local_not_null: false });
        return Ok(());
    };
    notices.push(format!("merging multiple inherited definitions of column \"{}\"", pc.name));
    if e.col.ty != pc.ty || e.col.typmod != pc.typmod {
        return Err(mismatch(format!("inherited column \"{}\" has a type conflict", pc.name)).with_detail(format!(
            "{} versus {}",
            type_text(catalog, &e.col),
            type_text(catalog, pc)
        )));
    }
    if e.col.collation != pc.collation {
        return Err(SqlError::new(code::COLLATION_MISMATCH, format!("inherited column \"{}\" has a collation conflict", pc.name))
            .with_detail(format!("{} versus {}", collation_text(&e.col), collation_text(pc))));
    }
    e.count += 1;
    if pc.not_null {
        e.not_nulls += 1;
        if !e.col.not_null {
            e.col.not_null = true;
            e.col.not_null_name = Some(not_null_name(parent, pc));
        }
    }
    let pdefault = if pc.identity.is_some() { None } else { pc.default.clone() };
    match (&e.col.default, pdefault) {
        (None, d) => e.col.default = d,
        (Some(a), Some(b)) if *a != b => e.default_conflict = true,
        _ => {}
    }
    Ok(())
}

/// Merges a locally declared column into the inherited one of the same name.
fn merge_local_column(catalog: &Catalog, e: &mut InhCol, local: ColumnDef) -> SqlResult<()> {
    if e.col.ty != local.ty || e.col.typmod != local.typmod {
        return Err(mismatch(format!("column \"{}\" has a type conflict", local.name)).with_detail(format!(
            "{} versus {}",
            type_text(catalog, &e.col),
            type_text(catalog, &local)
        )));
    }
    if local.collation.is_some() && e.col.collation != local.collation {
        return Err(SqlError::new(code::COLLATION_MISMATCH, format!("column \"{}\" has a collation conflict", local.name))
            .with_detail(format!("{} versus {}", collation_text(&e.col), collation_text(&local))));
    }
    e.local = true;
    if local.not_null {
        e.local_not_null = true;
        if !e.col.not_null {
            e.col.not_null = true;
            e.col.not_null_name = local.not_null_name.clone();
        }
    }
    if local.default.is_some() {
        e.col.default = local.default;
        e.default_conflict = false;
    }
    if local.generated.is_some() {
        e.col.generated = local.generated;
    }
    if local.identity.is_some() {
        e.col.identity = local.identity;
    }
    Ok(())
}

/// The parents' CHECK constraints, merged by name.
fn inherit_checks(catalog: &Catalog, parents: &[Arc<TableDef>], table: &mut TableDef, plan: &mut InheritPlan) -> SqlResult<()> {
    for p in parents {
        for c in inheritable_checks(catalog, p) {
            if let Some(existing) = table.constraints.iter().find(|x| x.name == c.name) {
                if check_expr(existing) != check_expr(c) {
                    return Err(SqlError::new(
                        code::DUPLICATE_OBJECT,
                        format!("check constraint name \"{}\" appears multiple times but with different expressions", c.name),
                    ));
                }
                if let Some((_, inh)) = plan.constraints.iter_mut().find(|(n, _)| *n == c.name) {
                    inh.count += 1;
                }
                continue;
            }
            let mut copy = c.clone();
            copy.oid = 0;
            table.constraints.push(copy);
            plan.constraints.push((c.name.clone(), Inherited::FROM_PARENT));
        }
    }
    Ok(())
}

/// `CREATE TABLE ... INHERITS`: puts the parents' columns first (merged with same-named local
/// columns, which Postgres reports in NOTICEs), then the remaining local columns, and copies the
/// parents' inheritable CHECK constraints.
pub(crate) fn merge_columns(b: &Binder, ct: &ir::CreateTable, table: &mut TableDef) -> SqlResult<InheritPlan> {
    let catalog = b.catalog;
    let parents = resolve_parents(catalog, ct)?;
    let mut plan = InheritPlan { parents: parents.iter().map(|p| p.oid).collect(), ..Default::default() };
    let mut inh: Vec<InhCol> = Vec::new();
    for p in &parents {
        for (_, pc) in p.live_columns() {
            add_parent_column(catalog, p, pc, &mut inh, &mut plan.notices)?;
        }
    }
    let inherited = inh.len();
    let mut appended = Vec::new();
    for (i, local) in std::mem::take(&mut table.columns).into_iter().enumerate() {
        let Some(pos) = inh[..inherited].iter().position(|e| e.col.name == local.name) else {
            appended.push(local);
            continue;
        };
        plan.notices.push(if pos == i {
            format!("merging column \"{}\" with inherited definition", local.name)
        } else {
            format!(
                "moving and merging column \"{}\" with inherited definition\nUser-specified column moved to the position of the inherited column.",
                local.name
            )
        });
        merge_local_column(catalog, &mut inh[pos], local)?;
    }
    for (slot, e) in inh.into_iter().enumerate() {
        if e.default_conflict {
            return Err(SqlError::new(
                code::INVALID_COLUMN_DEFINITION,
                format!("column \"{}\" inherits conflicting default values", e.col.name),
            )
            .with_hint("To resolve the conflict, specify a default explicitly."));
        }
        plan.columns.push((slot, Inherited { count: e.count, local: e.local }));
        if e.not_nulls > 0 {
            plan.not_nulls.push((slot, Inherited { count: e.not_nulls, local: e.local_not_null }));
        }
        table.columns.push(e.col);
    }
    table.columns.extend(appended);
    inherit_checks(catalog, &parents, table, &mut plan)?;
    Ok(plan)
}

/// A local CHECK constraint named like an inherited one: merged when the expressions match
/// (returns true), an error otherwise (also when the local one is NO INHERIT).
pub(crate) fn merge_local_check(
    table: &TableDef,
    plan: &mut InheritPlan,
    name: Option<&str>,
    kind: &ir::TableConstraintKind,
    no_inherit: bool,
) -> SqlResult<bool> {
    let (Some(name), ir::TableConstraintKind::Check(expr)) = (name, kind) else { return Ok(false) };
    let Some((_, inh)) = plan.constraints.iter_mut().find(|(n, _)| n == name) else { return Ok(false) };
    if table.constraints.iter().find(|c| c.name == name).and_then(check_expr) != Some(expr) {
        return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("constraint \"{name}\" for relation \"{}\" already exists", table.name)));
    }
    if no_inherit {
        return Err(conflicts_with_inherited(name, &table.name));
    }
    inh.local = true;
    plan.notices.push(format!("merging constraint \"{name}\" with inherited definition"));
    Ok(true)
}

/// Records a created table's inheritance state; returns the plan's NOTICEs.
pub(crate) fn apply_plan(cat: &mut Catalog, oid: Oid, plan: InheritPlan) -> Vec<Notice> {
    if !plan.parents.is_empty() {
        cat.set_inherit_parents(oid, plan.parents);
    }
    for (slot, v) in plan.columns {
        cat.set_column_inheritance(oid, slot, v);
    }
    for (slot, v) in plan.not_nulls {
        cat.set_not_null_inheritance(oid, slot, v);
    }
    let Some(table) = cat.table_by_oid(oid).cloned() else { return Vec::new() };
    let con_oid = |name: &str| table.constraints.iter().find(|c| c.name == name).map(|c| c.oid);
    for (name, v) in plan.constraints {
        if let Some(o) = con_oid(&name) {
            cat.set_constraint_inheritance(o, v);
        }
    }
    for name in plan.no_inherit {
        if let Some(o) = con_oid(&name) {
            cat.set_no_inherit(o, true);
        }
    }
    plan.notices.into_iter().map(notice).collect()
}

fn notice(message: String) -> Notice {
    Notice { severity: "NOTICE", code: "00000", message }
}

// ---------------------------------------------------------------------------------------------
// Scans and DML

/// The rows of an inheritance parent and of every table below it, in the parent's layout:
/// (table, row id, row). None when the table has no children.
pub(crate) fn scan(txn: &Txn, table: Oid) -> Option<Vec<(Oid, RowId, Row)>> {
    let catalog = txn.catalog();
    let children = catalog.inherit_descendants(table);
    if children.is_empty() {
        return None;
    }
    let root = catalog.table_by_oid(table)?;
    let mut out: Vec<(Oid, RowId, Row)> = txn.scan(table).into_iter().map(|(id, r)| (table, id, r)).collect();
    for child in children {
        txn.note_read(child.oid);
        for (id, mut row) in txn.scan(child.oid) {
            crate::dml::pad(&mut row, &child);
            out.push((child.oid, id, crate::partition::remap(&row, &child, root)));
        }
    }
    Some(out)
}

/// The inheritance parent an UPDATE / DELETE without ONLY targets, if it has children.
pub(crate) fn target(catalog: &Catalog, name: &ir::ObjectName) -> Option<Arc<TableDef>> {
    match catalog.find_relation(name).ok()?? {
        Relation::Table(t) if !catalog.inherit_children(t.oid).is_empty() => Some(t),
        _ => None,
    }
}

/// RETURNING items for a child: `*` stands for the parent's columns.
fn returning_for(items: &[ir::SelectItem], root: &TableDef, alias: &str) -> Vec<ir::SelectItem> {
    let columns = || {
        root.live_columns()
            .map(|(_, c)| ir::SelectItem::Expr { expr: ir::Expr::Column(vec![alias.to_owned(), c.name.clone()]), alias: None })
            .collect::<Vec<_>>()
    };
    items
        .iter()
        .flat_map(|i| match i {
            ir::SelectItem::Wildcard => columns(),
            ir::SelectItem::QualifiedWildcard(n) if n.0.len() == 1 && n.base() == alias => columns(),
            other => vec![other.clone()],
        })
        .collect()
}

/// The parent and its descendants, the tables an UPDATE / DELETE runs on.
fn targets(txn: &Txn, root: &Arc<TableDef>) -> Vec<Arc<TableDef>> {
    let mut out = vec![Arc::clone(root)];
    out.extend(txn.catalog().inherit_descendants(root.oid));
    out
}

/// UPDATE of an inheritance parent: runs on the parent and on each table below it
/// (`ONLY`), firing statement-level triggers of the parent only.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_update(
    u: &ir::Update,
    root: &Arc<TableDef>,
    binder: &Binder,
    ctx: &Ctx,
    txn: &mut Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let columns = if u.returning.is_empty() { Vec::new() } else { crate::bind::bind_update(binder, u, ctx)?.returning_cols };
    let alias = u.alias.clone().unwrap_or_else(|| root.name.clone());
    let set_columns: Vec<String> = u.assignments.iter().flat_map(|a| a.columns.iter().filter_map(|c| c.first().cloned())).collect();
    crate::triggers::root_statement(txn, root, Event::Update, TriggerTiming::Before, Some(&set_columns), env, state)?;
    let mut results = Vec::new();
    for t in targets(txn, root) {
        let mut tu = u.clone();
        tu.table = crate::partition::leaf_name(&t);
        tu.alias = Some(alias.clone());
        tu.only = true;
        tu.returning = returning_for(&u.returning, root, &alias);
        let stmt = Statement::Update(Box::new(tu));
        // The rows of every table below the parent take the parent's policies.
        results.push(binder.with_policy_root(root, || {
            routed(state, t.oid, || crate::statement::exec_statement(&stmt, binder, ctx, txn, params, env, state))
        })?);
    }
    crate::triggers::root_statement(txn, root, Event::Update, TriggerTiming::After, Some(&set_columns), env, state)?;
    Ok(crate::partition::combine(results, "UPDATE", columns))
}

/// DELETE from an inheritance parent: runs on the parent and on each table below it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_delete(
    d: &ir::Delete,
    root: &Arc<TableDef>,
    binder: &Binder,
    ctx: &Ctx,
    txn: &mut Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let columns = if d.returning.is_empty() { Vec::new() } else { crate::bind::bind_delete(binder, d, ctx)?.returning_cols };
    let alias = d.alias.clone().unwrap_or_else(|| root.name.clone());
    crate::triggers::root_statement(txn, root, Event::Delete, TriggerTiming::Before, None, env, state)?;
    let mut results = Vec::new();
    for t in targets(txn, root) {
        let mut td = d.clone();
        td.table = crate::partition::leaf_name(&t);
        td.alias = Some(alias.clone());
        td.only = true;
        td.returning = returning_for(&d.returning, root, &alias);
        let stmt = Statement::Delete(Box::new(td));
        // The rows of every table below the parent take the parent's policies.
        results.push(binder.with_policy_root(root, || {
            routed(state, t.oid, || crate::statement::exec_statement(&stmt, binder, ctx, txn, params, env, state))
        })?);
    }
    crate::triggers::root_statement(txn, root, Event::Delete, TriggerTiming::After, None, env, state)?;
    Ok(crate::partition::combine(results, "DELETE", columns))
}

// ---------------------------------------------------------------------------------------------
// DROP TABLE

/// The tables that must go with `table` (its inheritance descendants), each with the parent it
/// depends on, for DROP's dependency report.
pub(crate) fn dependents(catalog: &Catalog, table: &TableDef) -> Vec<(Arc<TableDef>, String)> {
    let mut seen = vec![table.oid];
    let mut out: Vec<(Arc<TableDef>, String)> = Vec::new();
    let mut i = 0;
    let mut queue = vec![table.oid];
    while i < queue.len() {
        let parent = queue[i];
        let pname = catalog.table_by_oid(parent).map(|t| t.name.clone()).unwrap_or_default();
        for c in catalog.inherit_children(parent) {
            if seen.contains(&c) {
                continue;
            }
            seen.push(c);
            queue.push(c);
            if let Some(t) = catalog.table_by_oid(c) {
                out.push((Arc::clone(t), pname.clone()));
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// ALTER TABLE

/// One ALTER TABLE action on the named table and, unless `only`, on its inheritance children.
pub(crate) fn alter(
    txn: &mut Txn,
    current: &QualName,
    action: &A,
    only: bool,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QualName> {
    let table = txn.catalog().table_q(current).cloned().ok_or_else(|| SqlError::undefined_table(&current.name))?;
    match action {
        A::Inherit(parent) => {
            add_parent(txn, &table, parent)?;
            return Ok(current.clone());
        }
        A::NoInherit(parent) => {
            remove_parent(txn, &table, parent)?;
            return Ok(current.clone());
        }
        _ => {}
    }
    if let A::AddConstraint(c) = action
        && let ir::TableConstraintKind::Check(expr) = &c.kind
    {
        if c.timing.no_inherit && txn.catalog().is_partitioned(table.oid) {
            return Err(no_inherit_on_partitioned(&table.name));
        }
        if merge_added_check(txn, &table, c.name.as_deref(), expr, c.timing.no_inherit, state)? {
            return Ok(current.clone());
        }
    }
    check_inherited(txn.catalog(), &table, action)?;
    if only && !txn.catalog().inherit_children(table.oid).is_empty() {
        check_only(action)?;
    }
    alter_tree(txn, current, action, !only, env, state)
}

/// ADD CONSTRAINT of a CHECK named like a purely inherited one (`MergeWithExistingConstraint`):
/// the same expression merges into it (returns true; the children already have it), unless the
/// new one is NO INHERIT. A different expression is left to the duplicate-name error.
fn merge_added_check(
    txn: &mut Txn,
    table: &TableDef,
    name: Option<&str>,
    expr: &ir::Expr,
    no_inherit: bool,
    state: &StatementState,
) -> SqlResult<bool> {
    let catalog = txn.catalog();
    let Some(existing) = name.and_then(|n| table.constraints.iter().find(|c| c.name == n)) else { return Ok(false) };
    let mut inh = catalog.constraint_inheritance(existing.oid);
    if inh.local || inh.count == 0 || catalog.partition_of(table.oid).is_some() || check_expr(existing) != Some(expr) {
        return Ok(false);
    }
    if no_inherit {
        return Err(conflicts_with_inherited(&existing.name, &table.name));
    }
    inh.local = true;
    txn.catalog_mut().set_constraint_inheritance(existing.oid, inh);
    state.notices.borrow_mut().push(notice(format!("merging constraint \"{}\" with inherited definition", existing.name)));
    Ok(true)
}

/// Actions a child may not apply to what it inherits.
fn check_inherited(catalog: &Catalog, table: &TableDef, action: &A) -> SqlResult<()> {
    let column = |name: &str| table.column_index(name).map(|s| (s, catalog.column_inheritance(table.oid, s)));
    let inherited_col = |name: &str| column(name).is_some_and(|(_, i)| i.count > 0);
    let inherited_con = |name: &str, renaming: bool| {
        // Renaming an inherited foreign key or index-backed constraint is allowed.
        let renamable = |c: &Constraint| renaming && !matches!(c.kind, ConstraintKind::Check { .. });
        let con = table
            .constraints
            .iter()
            .any(|c| c.name == name && !renamable(c) && catalog.table_constraint_inheritance(table.oid, c).count > 0);
        // A NOT NULL constraint, by its name.
        let not_null = table
            .live_columns()
            .any(|(s, c)| c.not_null && not_null_name(table, c) == name && catalog.not_null_inheritance(table.oid, s).count > 0);
        con || not_null
    };
    let partition = catalog.partition_of(table.oid).is_some();
    match action {
        A::DropColumn { name, .. } if inherited_col(name) => Err(table_def_error(format!("cannot drop inherited column \"{name}\""))),
        A::RenameColumn { old, .. } if inherited_col(old) => Err(table_def_error(format!("cannot rename inherited column \"{old}\""))),
        A::AlterType { column, .. } if inherited_col(column) => Err(table_def_error(format!("cannot alter inherited column \"{column}\""))),
        A::DropConstraint { name, .. } if inherited_con(name, false) => {
            Err(table_def_error(format!("cannot drop inherited constraint \"{name}\" of relation \"{}\"", table.name)))
        }
        A::RenameConstraint { old, .. } if inherited_con(old, true) => {
            Err(table_def_error(format!("cannot rename inherited constraint \"{old}\"")))
        }
        A::DropNotNull { column } => match table.column_index(column) {
            Some(s) if partition && catalog.not_null_inheritance(table.oid, s).count > 0 => {
                Err(table_def_error(format!("column \"{column}\" is marked NOT NULL in parent table")))
            }
            Some(s) if catalog.not_null_inheritance(table.oid, s).count > 0 => Err(table_def_error(format!(
                "cannot drop inherited constraint \"{}\" of relation \"{}\"",
                not_null_name(table, &table.columns[s]),
                table.name
            ))),
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Actions on a parent that must reach its children (`ONLY` is an error).
fn check_only(action: &A) -> SqlResult<()> {
    match action {
        A::AddColumn { .. } => Err(table_def_error("column must be added to child tables too")),
        A::RenameColumn { old, .. } => Err(table_def_error(format!("inherited column \"{old}\" must be renamed in child tables too"))),
        A::AlterType { column, .. } => {
            Err(table_def_error(format!("type of inherited column \"{column}\" must be changed in child tables too")))
        }
        A::RenameConstraint { old, .. } => {
            Err(table_def_error(format!("inherited constraint \"{old}\" must be renamed in child tables too")))
        }
        A::AddConstraint(c) if matches!(c.kind, ir::TableConstraintKind::Check(_)) && !c.timing.no_inherit => {
            Err(table_def_error("constraint must be added to child tables too"))
        }
        _ => Ok(()),
    }
}

/// Applies the action to `current`, then propagates it to its direct children.
fn alter_tree(
    txn: &mut Txn,
    current: &QualName,
    action: &A,
    recurse: bool,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QualName> {
    alter_tree_with(txn, current, action, recurse, env, state, &|_| ())
}

/// `alter_tree`, running `fix` on the table between its own change and its children's.
fn alter_tree_with(
    txn: &mut Txn,
    current: &QualName,
    action: &A,
    recurse: bool,
    env: &SessionEnv,
    state: &StatementState,
    fix: &dyn Fn(&mut Txn),
) -> SqlResult<QualName> {
    let before = txn.catalog().table_q(current).cloned().ok_or_else(|| SqlError::undefined_table(&current.name))?;
    let new_name = crate::ddl::alter_table_action(txn, current, action, env, state)?;
    fix(txn);
    let Some(after) = txn.catalog().table_q(&new_name).cloned() else { return Ok(new_name) };
    let added = added_check(&before, &after);
    if let (A::AddConstraint(c), Some(con)) = (action, added)
        && c.timing.no_inherit
    {
        txn.catalog_mut().set_no_inherit(con.oid, true);
    }
    if let A::DropColumn { name, .. } = action
        && let Some(slot) = before.column_index(name)
    {
        txn.catalog_mut().set_column_inheritance(before.oid, slot, Inherited::default());
        txn.catalog_mut().set_not_null_inheritance(before.oid, slot, Inherited::default());
    }
    let children: Vec<Arc<TableDef>> =
        txn.catalog().inherit_children(before.oid).iter().filter_map(|o| txn.catalog().table_by_oid(*o).cloned()).collect();
    for child in children {
        let step = Step { before: &before, after: &after, child: &child, recurse };
        propagate(txn, &step, action, env, state)?;
    }
    Ok(new_name)
}

/// The CHECK constraint an ADD CONSTRAINT created.
fn added_check(before: &TableDef, after: &TableDef) -> Option<Constraint> {
    after.constraints.iter().find(|c| check_expr(c).is_some() && !before.has_constraint(&c.name)).cloned()
}

struct Step<'a> {
    before: &'a TableDef,
    after: &'a TableDef,
    child: &'a TableDef,
    recurse: bool,
}

fn propagate(txn: &mut Txn, s: &Step, action: &A, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let q = s.child.qualified();
    match action {
        A::AddColumn { column, .. } => add_column(txn, s, column, env, state),
        A::DropColumn { name, cascade, .. } => drop_column(txn, s, name, *cascade, env, state),
        A::AddConstraint(c) if matches!(c.kind, ir::TableConstraintKind::Check(_)) && !c.timing.no_inherit => {
            add_check(txn, s, c, env, state)
        }
        A::DropConstraint { name, cascade, .. } => drop_check(txn, s, name, *cascade, env, state),
        A::SetNotNull { column } if s.recurse => set_not_null(txn, s, column, env, state),
        A::DropNotNull { column } => drop_not_null(txn, s, column, env, state),
        A::RenameConstraint { old, .. } => {
            let inherited = s.child.constraints.iter().any(|c| c.name == *old && txn.catalog().constraint_inheritance(c.oid).count > 0);
            if inherited { alter_tree(txn, &q, action, true, env, state).map(|_| ()) } else { Ok(()) }
        }
        A::RenameColumn { .. } | A::AlterType { .. } | A::SetDefault { .. } | A::DropDefault { .. } if s.recurse => {
            alter_tree(txn, &q, action, true, env, state).map(|_| ())
        }
        _ => Ok(()),
    }
}

/// The column definition a child gets for a column added to its parent: the parent's resolved
/// type and default (a serial's sequence is the parent's), its NOT NULL and CHECKs.
fn child_column(column: &ir::ColumnDef, added: &ColumnDef) -> ir::ColumnDef {
    let mut c = column.clone();
    let serial = match c.ty.name.as_str() {
        "serial" | "serial4" => Some("int4"),
        "bigserial" | "serial8" => Some("int8"),
        "smallserial" | "serial2" => Some("int2"),
        _ => None,
    };
    c.constraints.retain(|k| {
        matches!(
            k.kind,
            ir::ColumnConstraintKind::NotNull
                | ir::ColumnConstraintKind::Null
                | ir::ColumnConstraintKind::Default(_)
                | ir::ColumnConstraintKind::Check(_)
                | ir::ColumnConstraintKind::Collate(_)
                | ir::ColumnConstraintKind::Generated(_)
        )
    });
    if let Some(base) = serial {
        c.ty.name = base.to_owned();
        let timing = ir::ConstraintTiming::default();
        c.constraints.push(ir::ColumnConstraint { name: None, kind: ir::ColumnConstraintKind::NotNull, timing, reloptions: Vec::new() });
        if let Some(d) = &added.default {
            c.constraints.push(ir::ColumnConstraint {
                name: None,
                kind: ir::ColumnConstraintKind::Default(d.clone()),
                timing,
                reloptions: Vec::new(),
            });
        }
    }
    c
}

fn add_column(txn: &mut Txn, s: &Step, column: &ir::ColumnDef, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let Some(added) = s.after.column(&column.name) else { return Ok(()) };
    let catalog = txn.catalog();
    if let Some(slot) = s.child.column_index(&column.name) {
        let existing = &s.child.columns[slot];
        if existing.ty != added.ty || existing.typmod != added.typmod {
            return Err(mismatch(format!("child table \"{}\" has different type for column \"{}\"", s.child.name, column.name)));
        }
        let mut inh = catalog.column_inheritance(s.child.oid, slot);
        inh.count += 1;
        txn.catalog_mut().set_column_inheritance(s.child.oid, slot, inh);
        state.notices.borrow_mut().push(notice(format!("merging definition of column \"{}\" for child \"{}\"", column.name, s.child.name)));
        return Ok(());
    }
    let action = A::AddColumn { if_not_exists: false, column: child_column(column, added) };
    let q = alter_tree(txn, &s.child.qualified(), &action, true, env, state)?;
    let Some(child) = txn.catalog().table_q(&q).cloned() else { return Ok(()) };
    if let Some(slot) = child.column_index(&column.name) {
        txn.catalog_mut().set_column_inheritance(child.oid, slot, Inherited::FROM_PARENT);
        if child.columns[slot].not_null {
            txn.catalog_mut().set_not_null_inheritance(child.oid, slot, Inherited::FROM_PARENT);
        }
    }
    Ok(())
}

/// Drops a child's copy of a dropped parent column when it inherited only that copy (Postgres'
/// `ATExecDropColumn` recursion); otherwise the child keeps it with one parent fewer.
fn drop_column(txn: &mut Txn, s: &Step, name: &str, cascade: bool, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let Some(slot) = s.child.column_index(name) else { return Ok(()) };
    let mut inh = txn.catalog().column_inheritance(s.child.oid, slot);
    if s.recurse && inh.count == 1 && !inh.local {
        txn.catalog_mut().set_column_inheritance(s.child.oid, slot, Inherited::default());
        let action = A::DropColumn { name: name.to_owned(), if_exists: false, cascade };
        return alter_tree(txn, &s.child.qualified(), &action, true, env, state).map(|_| ());
    }
    inh.count = (inh.count - 1).max(0);
    if !s.recurse {
        inh.local = true;
    }
    txn.catalog_mut().set_column_inheritance(s.child.oid, slot, inh);
    Ok(())
}

fn add_check(txn: &mut Txn, s: &Step, c: &ir::TableConstraint, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let Some(added) = added_check(s.before, s.after) else { return Ok(()) };
    if let Some(existing) = s.child.constraints.iter().find(|x| x.name == added.name) {
        if check_expr(existing) != check_expr(&added) {
            return Err(SqlError::new(
                code::DUPLICATE_OBJECT,
                format!("constraint \"{}\" for relation \"{}\" already exists", added.name, s.child.name),
            ));
        }
        if txn.catalog().is_no_inherit(existing.oid) {
            return Err(conflicts_with_non_inherited(&added.name, "relation", &s.child.name));
        }
        let mut inh = txn.catalog().constraint_inheritance(existing.oid);
        inh.count += 1;
        txn.catalog_mut().set_constraint_inheritance(existing.oid, inh);
        state.notices.borrow_mut().push(notice(format!("merging constraint \"{}\" with inherited definition", added.name)));
        return Ok(());
    }
    let mut c = c.clone();
    c.name = Some(added.name.clone());
    let q = alter_tree(txn, &s.child.qualified(), &A::AddConstraint(c), true, env, state)?;
    let con = txn.catalog().table_q(&q).and_then(|t| t.constraints.iter().find(|x| x.name == added.name).map(|x| x.oid));
    if let Some(o) = con {
        txn.catalog_mut().set_constraint_inheritance(o, Inherited::FROM_PARENT);
    }
    Ok(())
}

fn drop_check(txn: &mut Txn, s: &Step, name: &str, cascade: bool, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let Some(dropped) = s.before.constraints.iter().find(|c| c.name == name) else { return Ok(()) };
    if check_expr(dropped).is_none() || txn.catalog().is_no_inherit(dropped.oid) {
        return Ok(());
    }
    let Some(con) = s.child.constraints.iter().find(|c| c.name == name && check_expr(c).is_some()) else { return Ok(()) };
    let mut inh = txn.catalog().constraint_inheritance(con.oid);
    if inh.count == 0 {
        return Ok(());
    }
    if s.recurse && inh.count == 1 && !inh.local {
        txn.catalog_mut().set_constraint_inheritance(con.oid, Inherited::default());
        let action = A::DropConstraint { name: name.to_owned(), if_exists: false, cascade };
        return alter_tree(txn, &s.child.qualified(), &action, true, env, state).map(|_| ());
    }
    inh.count -= 1;
    if !s.recurse {
        inh.local = true;
    }
    txn.catalog_mut().set_constraint_inheritance(con.oid, inh);
    Ok(())
}

fn set_not_null(txn: &mut Txn, s: &Step, column: &str, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let Some(slot) = s.child.column_index(column) else { return Ok(()) };
    if s.child.columns[slot].not_null {
        let mut inh = txn.catalog().not_null_inheritance(s.child.oid, slot);
        inh.count += 1;
        txn.catalog_mut().set_not_null_inheritance(s.child.oid, slot, inh);
        return Ok(());
    }
    // The child's constraint takes the parent's name (and passes it on).
    let name = s.after.column(column).map(|c| not_null_name(s.after, c));
    let child = s.child.oid;
    let fix = |txn: &mut Txn| {
        if let Some(t) = txn.catalog().table_by_oid(child).cloned() {
            let mut def = (*t).clone();
            def.columns[slot].not_null_name.clone_from(&name);
            txn.catalog_mut().replace_table(&t.qualified(), def);
        }
        txn.catalog_mut().set_not_null_inheritance(child, slot, Inherited::FROM_PARENT);
    };
    alter_tree_with(txn, &s.child.qualified(), &A::SetNotNull { column: column.to_owned() }, true, env, state, &fix).map(|_| ())
}

fn drop_not_null(txn: &mut Txn, s: &Step, column: &str, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let Some(slot) = s.child.column_index(column) else { return Ok(()) };
    let mut inh = txn.catalog().not_null_inheritance(s.child.oid, slot);
    if inh.count == 0 || !s.before.column(column).is_some_and(|c| c.not_null) {
        return Ok(());
    }
    if s.recurse && inh.count == 1 && !inh.local {
        txn.catalog_mut().set_not_null_inheritance(s.child.oid, slot, Inherited::default());
        return alter_tree(txn, &s.child.qualified(), &A::DropNotNull { column: column.to_owned() }, true, env, state).map(|_| ());
    }
    inh.count -= 1;
    if !s.recurse {
        inh.local = true;
    }
    txn.catalog_mut().set_not_null_inheritance(s.child.oid, slot, inh);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// ALTER TABLE ... [NO] INHERIT

fn parent_table(catalog: &Catalog, name: &ir::ObjectName) -> SqlResult<Arc<TableDef>> {
    match catalog.lookup_relation(name)? {
        Some(Relation::Table(t)) if t.matview.is_none() => Ok(t),
        Some(r) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", r.name()))),
        None => Err(SqlError::undefined_table(&name.to_string())),
    }
}

/// The child must already have the parent's columns (same types, NOT NULL where the parent
/// has it) and inheritable CHECK constraints.
fn check_compatible(catalog: &Catalog, child: &TableDef, parent: &TableDef) -> SqlResult<()> {
    for (_, p) in parent.live_columns() {
        let Some(c) = child.column(&p.name) else {
            return Err(mismatch(format!("child table is missing column \"{}\"", p.name)));
        };
        if c.ty != p.ty || c.typmod != p.typmod {
            return Err(mismatch(format!("child table \"{}\" has different type for column \"{}\"", child.name, p.name)));
        }
        if c.collation != p.collation {
            return Err(SqlError::new(
                code::COLLATION_MISMATCH,
                format!("child table \"{}\" has different collation for column \"{}\"", child.name, p.name),
            ));
        }
        if p.not_null && !c.not_null {
            return Err(mismatch(format!("column \"{}\" in child table \"{}\" must be marked NOT NULL", p.name, child.name)));
        }
    }
    for pc in inheritable_checks(catalog, parent) {
        match child.constraints.iter().find(|c| c.name == pc.name) {
            None => return Err(mismatch(format!("child table is missing constraint \"{}\"", pc.name))),
            Some(c) if check_expr(c) != check_expr(pc) => {
                return Err(mismatch(format!(
                    "child table \"{}\" has different definition for check constraint \"{}\"",
                    child.name, pc.name
                )));
            }
            Some(c) if catalog.is_no_inherit(c.oid) => {
                return Err(conflicts_with_non_inherited(&pc.name, "child table", &child.name));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Adds `delta` parents' worth of inheritance to the child's columns and constraints that
/// match the parent's; at zero they become local (Postgres' `RemoveInheritance`).
fn adjust_counts(cat: &mut Catalog, child: &TableDef, parent: &TableDef, delta: i16) {
    let bump = |mut inh: Inherited| {
        inh.count = (inh.count + delta).max(0);
        if inh.count == 0 {
            inh.local = true;
        }
        inh
    };
    for (_, p) in parent.live_columns() {
        let Some(slot) = child.column_index(&p.name) else { continue };
        let inh = bump(cat.column_inheritance(child.oid, slot));
        cat.set_column_inheritance(child.oid, slot, inh);
        if p.not_null && child.columns[slot].not_null {
            let inh = bump(cat.not_null_inheritance(child.oid, slot));
            cat.set_not_null_inheritance(child.oid, slot, inh);
        }
    }
    let checks: Vec<String> = inheritable_checks(cat, parent).map(|c| c.name.clone()).collect();
    for c in child.constraints.iter().filter(|c| checks.contains(&c.name)) {
        let inh = bump(cat.constraint_inheritance(c.oid));
        cat.set_constraint_inheritance(c.oid, inh);
    }
}

/// `ALTER TABLE child INHERIT parent`.
fn add_parent(txn: &mut Txn, child: &TableDef, name: &ir::ObjectName) -> SqlResult<()> {
    let catalog = txn.catalog();
    let parent = parent_table(catalog, name)?;
    if catalog.partition_of(child.oid).is_some() || catalog.is_partitioned(child.oid) {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, "cannot change inheritance of a partition"));
    }
    if catalog.is_partitioned(parent.oid) {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, "cannot inherit from partitioned table"));
    }
    if catalog.inherits_from(parent.oid, child.oid) {
        return Err(SqlError::new(code::DUPLICATE_TABLE, "circular inheritance not allowed")
            .with_detail(format!("\"{}\" is already a child of \"{}\".", parent.name, child.name)));
    }
    if catalog.inherit_parents(child.oid).contains(&parent.oid) {
        return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{}\" would be inherited from more than once", parent.name)));
    }
    check_compatible(catalog, child, &parent)?;
    let mut parents = catalog.inherit_parents(child.oid).to_vec();
    parents.push(parent.oid);
    let cat = txn.catalog_mut();
    adjust_counts(cat, child, &parent, 1);
    cat.set_inherit_parents(child.oid, parents);
    Ok(())
}

/// `ALTER TABLE child NO INHERIT parent`.
fn remove_parent(txn: &mut Txn, child: &TableDef, name: &ir::ObjectName) -> SqlResult<()> {
    let catalog = txn.catalog();
    let parent = parent_table(catalog, name)?;
    if catalog.partition_of(child.oid).is_some() {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, "cannot change inheritance of a partition"));
    }
    let mut parents = catalog.inherit_parents(child.oid).to_vec();
    if !parents.contains(&parent.oid) {
        return Err(SqlError::new(
            code::UNDEFINED_TABLE,
            format!("relation \"{}\" is not a parent of relation \"{}\"", parent.name, child.name),
        ));
    }
    parents.retain(|p| *p != parent.oid);
    let cat = txn.catalog_mut();
    adjust_counts(cat, child, &parent, -1);
    cat.set_inherit_parents(child.oid, parents);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Catalogs

/// `(conislocal, coninhcount, connoinherit)` of a `pg_constraint` row.
pub(crate) fn constraint_flags(catalog: &Catalog, table: Oid, oid: Oid, contype: char, conkey: &[usize]) -> (bool, i16, bool) {
    let inh = if contype == 'n' {
        conkey.first().map_or_else(Inherited::default, |a| catalog.not_null_inheritance(table, a - 1))
    } else {
        match catalog.table_by_oid(table).and_then(|t| t.constraints.iter().find(|c| c.oid == oid)) {
            Some(c) => catalog.table_constraint_inheritance(table, c),
            None => catalog.constraint_inheritance(oid),
        }
    };
    (inh.local, inh.count, contype == 'c' && catalog.is_no_inherit(oid))
}
