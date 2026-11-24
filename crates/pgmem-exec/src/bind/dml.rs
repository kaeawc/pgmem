//! INSERT / UPDATE / DELETE planning.

use super::expr::output_name;
use super::{Binder, Clause, Ctx, Level, Scope, ScopeCol, coerce_assignment};
use crate::plan::{Expr, OutCol, Rel};
use pgmem_catalog::{ConstraintKind, Identity, TableDef};
use pgmem_ir as ir;
use pgmem_types::{SqlError, SqlResult, Type, code};
use std::sync::Arc;

/// A unique index to enforce, with its key expressions bound against the physical row.
#[derive(Clone, Debug)]
pub struct UniqueCheck {
    pub name: String,
    pub columns: Vec<usize>,
    /// Expression keys (bound against the physical row), used instead of `columns` when set.
    pub exprs: Vec<Expr>,
    pub predicate: Option<Expr>,
    pub nulls_not_distinct: bool,
}

/// Row-level rules shared by INSERT and UPDATE.
#[derive(Clone, Debug)]
pub struct RowRules {
    pub table: Arc<TableDef>,
    /// Per physical slot: DEFAULT expression (bound in an empty scope).
    pub defaults: Vec<Option<Expr>>,
    /// Per physical slot: GENERATED ALWAYS AS expression (bound against the physical row).
    pub generated: Vec<Option<Expr>>,
    pub checks: Vec<(String, Expr)>,
    pub uniques: Vec<UniqueCheck>,
}

#[derive(Clone, Debug)]
pub enum InsertRows {
    /// `None` marks a DEFAULT keyword.
    Values(Vec<Vec<Option<Expr>>>),
    Query(Rel),
    DefaultValues,
}

#[derive(Clone, Debug)]
pub struct OnConflictPlan {
    /// The arbiter unique index (`None` with DO NOTHING means "any unique violation").
    pub arbiter: Option<UniqueCheck>,
    pub update: Option<DoUpdatePlan>,
}

/// ON CONFLICT DO UPDATE: assignments over (existing row ++ excluded row), `None` meaning
/// DEFAULT, and the optional WHERE filter.
#[derive(Clone, Debug)]
pub struct DoUpdatePlan {
    pub assignments: Vec<(usize, Option<Expr>)>,
    pub filter: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct InsertPlan {
    pub rules: RowRules,
    /// Physical slots targeted by the source columns, in order.
    pub columns: Vec<usize>,
    pub rows: InsertRows,
    pub overriding_system_value: bool,
    pub on_conflict: Option<OnConflictPlan>,
    pub returning: Vec<Expr>,
    pub returning_cols: Vec<OutCol>,
}

#[derive(Clone, Debug)]
pub struct UpdatePlan {
    pub rules: RowRules,
    /// Assignments over (target row ++ FROM row); `None` is `SET col = DEFAULT`.
    pub assignments: Vec<(usize, Option<Expr>)>,
    pub from: Option<(Rel, usize)>,
    pub filter: Option<Expr>,
    pub returning: Vec<Expr>,
    pub returning_cols: Vec<OutCol>,
}

#[derive(Clone, Debug)]
pub struct DeletePlan {
    pub table: Arc<TableDef>,
    pub using: Option<(Rel, usize)>,
    pub filter: Option<Expr>,
    pub returning: Vec<Expr>,
    pub returning_cols: Vec<OutCol>,
}

/// The scope of a table's physical row (dropped columns occupy unaddressable slots).
pub fn physical_scope(table: &TableDef, qualifier: &str) -> Scope {
    let cols = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut sc = ScopeCol::new(Some(qualifier.to_owned()), c.name.clone(), c.ty.clone());
            sc.dropped = c.dropped;
            sc.table_oid = table.oid;
            sc.attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
            sc.typmod = c.typmod;
            sc
        })
        .collect();
    Scope::from_cols(cols)
}

fn lookup_table(b: &Binder, name: &ir::ObjectName) -> SqlResult<Arc<TableDef>> {
    let base = b.catalog.normalize(name)?;
    match b.catalog.relation(base) {
        Some(pgmem_catalog::Relation::Table(t)) => Ok(t),
        Some(pgmem_catalog::Relation::View(_)) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("cannot modify view \"{base}\""))
            .with_detail("Views that do not select from a single table or view are not automatically updatable.")),
        Some(other) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("cannot modify {} \"{base}\"", other.kind_name()))),
        None => Err(SqlError::undefined_table(&name.to_string())),
    }
}

impl Binder<'_> {
    /// Binds the expression-valued rules of a table: defaults, generated columns, CHECKs and
    /// unique indexes.
    pub fn bind_row_rules(&self, table: &Arc<TableDef>) -> SqlResult<RowRules> {
        let empty = Level::new(Scope::default());
        let empty_ctx = Ctx::new(&empty).with_clause(Clause::Default);
        let row_level = Level::new(physical_scope(table, &table.name));
        let row_ctx = Ctx::new(&row_level).with_clause(Clause::Check);
        let mut defaults = Vec::with_capacity(table.columns.len());
        let mut generated = Vec::with_capacity(table.columns.len());
        for c in &table.columns {
            defaults.push(match &c.default {
                Some(d) if !c.dropped => {
                    let (e, t) = self.bind_expr(d, &empty_ctx)?;
                    Some(coerce_assignment(self, e, &t, &c.ty, c.typmod, &c.name)?)
                }
                _ => None,
            });
            generated.push(match &c.generated {
                Some(g) if !c.dropped => {
                    let (e, t) = self.bind_expr(g, &row_ctx)?;
                    Some(coerce_assignment(self, e, &t, &c.ty, c.typmod, &c.name)?)
                }
                _ => None,
            });
        }
        let mut checks = Vec::new();
        for con in &table.constraints {
            if let ConstraintKind::Check { expr } = &con.kind {
                checks.push((con.name.clone(), self.bind_predicate(expr, "CHECK", &row_ctx)?));
            }
        }
        let mut uniques = Vec::new();
        for idx in table.indexes.iter().filter(|i| i.unique) {
            let exprs = idx.exprs.iter().map(|e| Ok(self.bind_expr(e, &row_ctx)?.0)).collect::<SqlResult<Vec<_>>>()?;
            let predicate = idx.predicate.as_ref().map(|p| self.bind_predicate(p, "WHERE", &row_ctx)).transpose()?;
            uniques.push(UniqueCheck {
                name: idx.name.clone(),
                columns: idx.columns.clone(),
                exprs,
                predicate,
                nulls_not_distinct: idx.nulls_not_distinct,
            });
        }
        Ok(RowRules { table: Arc::clone(table), defaults, generated, checks, uniques })
    }

    fn bind_returning(&self, items: &[ir::SelectItem], ctx: &Ctx) -> SqlResult<(Vec<Expr>, Vec<OutCol>)> {
        let scope = &ctx.current().scope;
        let rctx = ctx.with_clause(Clause::Returning);
        let mut exprs = Vec::new();
        let mut cols = Vec::new();
        for item in items {
            match item {
                ir::SelectItem::Wildcard => {
                    for &i in &scope.star {
                        let c = &scope.cols[i];
                        exprs.push(Expr::Col(i));
                        cols.push(OutCol {
                            name: c.name.clone(),
                            ty: c.ty.clone(),
                            table_oid: c.table_oid,
                            attnum: c.attnum,
                            typmod: c.typmod,
                        });
                    }
                }
                ir::SelectItem::QualifiedWildcard(q) => {
                    let qual = q.base();
                    if !scope.has_qualifier(qual) {
                        return Err(SqlError::new(code::UNDEFINED_TABLE, format!("missing FROM-clause entry for table \"{qual}\"")));
                    }
                    for i in scope.qualified_star(qual) {
                        let c = &scope.cols[i];
                        exprs.push(Expr::Col(i));
                        cols.push(OutCol {
                            name: c.name.clone(),
                            ty: c.ty.clone(),
                            table_oid: c.table_oid,
                            attnum: c.attnum,
                            typmod: c.typmod,
                        });
                    }
                }
                ir::SelectItem::Expr { expr, alias } => {
                    let (e, t) = self.bind_expr(expr, &rctx)?;
                    let t = if t == Type::Unknown { Type::Text } else { t };
                    let mut col = OutCol::new(alias.clone().unwrap_or_else(|| output_name(expr)), t);
                    if let Expr::Col(i) = &e
                        && let Some(c) = scope.cols.get(*i)
                    {
                        col.table_oid = c.table_oid;
                        col.attnum = c.attnum;
                        col.typmod = c.typmod;
                    }
                    exprs.push(e);
                    cols.push(col);
                }
            }
        }
        Ok((exprs, cols))
    }
}

/// Finds the unique index matching an ON CONFLICT column list (order-insensitive), as
/// Postgres' `infer_arbiter_indexes` does.
fn arbiter_for(rules: &RowRules, table: &TableDef, target: &ir::ConflictTarget) -> SqlResult<Option<UniqueCheck>> {
    match target {
        ir::ConflictTarget::None => Ok(None),
        ir::ConflictTarget::Columns { columns, .. } => {
            let mut wanted = Vec::new();
            for c in columns {
                wanted.push(table.column_index(c).ok_or_else(|| SqlError::undefined_column(&format!("\"{c}\"")))?);
            }
            wanted.sort_unstable();
            wanted.dedup();
            rules
                .uniques
                .iter()
                .find(|u| {
                    u.exprs.is_empty() && {
                        let mut cols = u.columns.clone();
                        cols.sort_unstable();
                        cols == wanted
                    }
                })
                .cloned()
                .map(Some)
                .ok_or_else(|| {
                    SqlError::new(
                        code::INVALID_COLUMN_REFERENCE,
                        "there is no unique or exclusion constraint matching the ON CONFLICT specification",
                    )
                })
        }
        ir::ConflictTarget::Constraint(name) => rules.uniques.iter().find(|u| &u.name == name).cloned().map(Some).ok_or_else(|| {
            SqlError::new(code::UNDEFINED_OBJECT, format!("constraint \"{name}\" for table \"{}\" does not exist", table.name))
        }),
    }
}

pub fn bind_insert(b: &Binder, ins: &ir::Insert, outer: &Ctx) -> SqlResult<InsertPlan> {
    let table = lookup_table(b, &ins.table)?;
    let qualifier = ins.alias.clone().unwrap_or_else(|| table.name.clone());
    let rules = b.bind_row_rules(&table)?;
    let columns: Vec<usize> = if ins.columns.is_empty() {
        table.live_columns().map(|(i, _)| i).collect()
    } else {
        let mut out = Vec::new();
        for c in &ins.columns {
            let idx =
                table.column_index(c).ok_or_else(|| SqlError::undefined_column(&format!("\"{c}\" of relation \"{}\"", table.name)))?;
            if out.contains(&idx) {
                return Err(SqlError::syntax(format!("column \"{c}\" specified more than once")));
            }
            out.push(idx);
        }
        out
    };
    let empty = Level::new(Scope::default());
    let mut vctx = outer.from_item(&[&empty]).with_clause(Clause::Values);
    if let Some(with) = &ins.with {
        let _ = with;
        return Err(super::feature("WITH on INSERT"));
    }
    vctx.ctes = outer.ctes.clone();
    let rows = match &ins.source {
        ir::InsertSource::DefaultValues => InsertRows::DefaultValues,
        ir::InsertSource::Values(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                if row.len() > columns.len() {
                    return Err(SqlError::syntax("INSERT has more expressions than target columns"));
                }
                if row.len() < columns.len() {
                    return Err(SqlError::syntax("INSERT has more target columns than expressions"));
                }
                let mut bound = Vec::with_capacity(row.len());
                for (e, &slot) in row.iter().zip(&columns) {
                    let col = &table.columns[slot];
                    if matches!(e, ir::Expr::Default) {
                        bound.push(None);
                        continue;
                    }
                    let (x, t) = b.bind_expr(e, &vctx)?;
                    bound.push(Some(coerce_assignment(b, x, &t, &col.ty, col.typmod, &col.name)?));
                }
                out.push(bound);
            }
            InsertRows::Values(out)
        }
        ir::InsertSource::Query(q) => {
            let bound = b.bind_query(q, &vctx)?;
            if bound.columns.len() > columns.len() {
                return Err(SqlError::syntax("INSERT has more expressions than target columns"));
            }
            if bound.columns.len() < columns.len() {
                return Err(SqlError::syntax("INSERT has more target columns than expressions"));
            }
            let mut exprs = Vec::new();
            for (i, (out, &slot)) in bound.columns.iter().zip(&columns).enumerate() {
                let col = &table.columns[slot];
                exprs.push(coerce_assignment(b, Expr::Col(i), &out.ty, &col.ty, col.typmod, &col.name)?);
            }
            InsertRows::Query(Rel::Project { input: Box::new(bound.rel), exprs })
        }
    };
    let row_scope = physical_scope(&table, &qualifier);
    let row_level = Level::new(row_scope.clone());
    let row_ctx = Ctx::new(&row_level);
    let on_conflict = match &ins.on_conflict {
        None => None,
        Some(oc) => {
            let arbiter = arbiter_for(&rules, &table, &oc.target)?;
            let update = match &oc.action {
                ir::ConflictAction::Nothing => None,
                ir::ConflictAction::Update { assignments, selection } => {
                    if arbiter.is_none() {
                        return Err(SqlError::syntax("ON CONFLICT DO UPDATE requires inference specification or constraint name")
                            .with_hint("For example, ON CONFLICT (column_name)."));
                    }
                    let mut cols = row_scope.cols.clone();
                    for c in &row_scope.cols {
                        let mut e = c.clone();
                        e.qualifier = Some("excluded".into());
                        cols.push(e);
                    }
                    let combined = Level::new(Scope { star: row_scope.star.clone(), cols });
                    let uctx = Ctx::new(&combined);
                    let assigns = bind_assignments(b, &table, assignments, &uctx)?;
                    let filter = selection.as_ref().map(|s| b.bind_predicate(s, "WHERE", &uctx)).transpose()?;
                    Some(DoUpdatePlan { assignments: assigns, filter })
                }
            };
            Some(OnConflictPlan { arbiter, update })
        }
    };
    let (returning, returning_cols) = b.bind_returning(&ins.returning, &row_ctx)?;
    Ok(InsertPlan { rules, columns, rows, overriding_system_value: ins.overriding_system_value, on_conflict, returning, returning_cols })
}

fn bind_assignments(b: &Binder, table: &TableDef, assignments: &[ir::Assignment], ctx: &Ctx) -> SqlResult<Vec<(usize, Option<Expr>)>> {
    let mut out: Vec<(usize, Option<Expr>)> = Vec::new();
    for a in assignments {
        if a.columns.len() > 1 {
            // SET (a, b) = (x, y)
            let ir::Expr::Row(values) = &a.value else { return Err(super::feature("SET (columns) = (subquery)")) };
            if values.len() != a.columns.len() {
                return Err(SqlError::syntax("number of columns does not match number of values"));
            }
            for (c, v) in a.columns.iter().zip(values) {
                out.push(bind_one_assignment(b, table, c, v, ctx)?);
            }
        } else {
            out.push(bind_one_assignment(b, table, &a.columns[0], &a.value, ctx)?);
        }
    }
    let mut seen = std::collections::HashSet::new();
    for (slot, _) in &out {
        if !seen.insert(*slot) {
            return Err(SqlError::syntax(format!("multiple assignments to same column \"{}\"", table.columns[*slot].name)));
        }
    }
    Ok(out)
}

fn bind_one_assignment(b: &Binder, table: &TableDef, column: &str, value: &ir::Expr, ctx: &Ctx) -> SqlResult<(usize, Option<Expr>)> {
    let slot =
        table.column_index(column).ok_or_else(|| SqlError::undefined_column(&format!("\"{column}\" of relation \"{}\"", table.name)))?;
    let col = &table.columns[slot];
    if matches!(value, ir::Expr::Default) {
        return Ok((slot, None));
    }
    if col.generated.is_some() {
        return Err(SqlError::new(code::GENERATED_ALWAYS, format!("column \"{column}\" can only be updated to DEFAULT"))
            .with_detail(format!("Column \"{column}\" is a generated column.")));
    }
    if col.identity == Some(Identity::Always) {
        return Err(SqlError::new(code::GENERATED_ALWAYS, format!("column \"{column}\" can only be updated to DEFAULT"))
            .with_detail(format!("Column \"{column}\" is an identity column defined as GENERATED ALWAYS.")));
    }
    let (e, t) = b.bind_expr(value, &ctx.with_clause(Clause::Other))?;
    Ok((slot, Some(coerce_assignment(b, e, &t, &col.ty, col.typmod, &col.name)?)))
}

pub fn bind_update(b: &Binder, upd: &ir::Update, outer: &Ctx) -> SqlResult<UpdatePlan> {
    let table = lookup_table(b, &upd.table)?;
    let qualifier = upd.alias.clone().unwrap_or_else(|| table.name.clone());
    let rules = b.bind_row_rules(&table)?;
    let target_scope = physical_scope(&table, &qualifier);
    let width = target_scope.width();
    let (from, scope) = if upd.from.is_empty() {
        (None, target_scope)
    } else {
        let empty = Level::new(Scope::default());
        let fctx = outer.from_item(&[&empty]);
        let (rel, fscope) = b.bind_from(&upd.from, &fctx)?;
        let fw = fscope.width();
        let mut cols = target_scope.cols.clone();
        cols.extend(fscope.cols.iter().cloned());
        let mut star = target_scope.star.clone();
        star.extend(fscope.star.iter().map(|i| i + width));
        (Some((rel, fw)), Scope { cols, star })
    };
    let level = Level::new(scope);
    let ctx = outer.from_item(&[&level]);
    let assignments = bind_assignments(b, &table, &upd.assignments, &ctx)?;
    let filter = upd.selection.as_ref().map(|s| b.bind_predicate(s, "WHERE", &ctx.with_clause(Clause::Where))).transpose()?;
    let (returning, returning_cols) = b.bind_returning(&upd.returning, &ctx)?;
    Ok(UpdatePlan { rules, assignments, from, filter, returning, returning_cols })
}

pub fn bind_delete(b: &Binder, del: &ir::Delete, outer: &Ctx) -> SqlResult<DeletePlan> {
    let table = lookup_table(b, &del.table)?;
    let qualifier = del.alias.clone().unwrap_or_else(|| table.name.clone());
    let target_scope = physical_scope(&table, &qualifier);
    let width = target_scope.width();
    let (using, scope) = if del.using.is_empty() {
        (None, target_scope)
    } else {
        let empty = Level::new(Scope::default());
        let fctx = outer.from_item(&[&empty]);
        let (rel, uscope) = b.bind_from(&del.using, &fctx)?;
        let uw = uscope.width();
        let mut cols = target_scope.cols.clone();
        cols.extend(uscope.cols.iter().cloned());
        let mut star = target_scope.star.clone();
        star.extend(uscope.star.iter().map(|i| i + width));
        (Some((rel, uw)), Scope { cols, star })
    };
    let level = Level::new(scope);
    let ctx = outer.from_item(&[&level]);
    let filter = del.selection.as_ref().map(|s| b.bind_predicate(s, "WHERE", &ctx.with_clause(Clause::Where))).transpose()?;
    let (returning, returning_cols) = b.bind_returning(&del.returning, &ctx)?;
    Ok(DeletePlan { table, using, filter, returning, returning_cols })
}
