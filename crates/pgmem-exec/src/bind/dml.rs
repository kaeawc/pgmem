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

/// An exclusion constraint, bound against the physical row.
#[derive(Clone, Debug)]
pub struct ExclusionCheck {
    pub name: String,
    pub keys: Vec<Expr>,
    /// Each key's operator, applied as `new op existing`.
    pub ops: Vec<crate::plan::Func>,
    pub predicate: Option<Expr>,
    /// Key names for messages.
    pub columns: Vec<String>,
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
    pub exclusions: Vec<ExclusionCheck>,
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
    pub overriding_user_value: bool,
    pub on_conflict: Option<OnConflictPlan>,
    pub returning: Vec<Expr>,
    pub returning_cols: Vec<OutCol>,
    /// WITH CHECK OPTION of the view written through: (view name, predicate over the row).
    pub view_checks: Vec<(String, Expr)>,
    /// Row-level security checks of new rows.
    pub rls_checks: Vec<crate::rls::RlsCheck>,
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
    pub view_checks: Vec<(String, Expr)>,
    /// Row-level security checks of new rows.
    pub rls_checks: Vec<crate::rls::RlsCheck>,
}

#[derive(Clone, Debug)]
pub struct DeletePlan {
    pub table: Arc<TableDef>,
    pub using: Option<(Rel, usize)>,
    pub filter: Option<Expr>,
    pub returning: Vec<Expr>,
    pub returning_cols: Vec<OutCol>,
}

/// [`physical_scope`] followed by the system columns: the row UPDATE / DELETE filters and
/// RETURNING see.
pub fn target_scope(table: &TableDef, qualifier: &str) -> Scope {
    let mut cols = physical_scope(table, qualifier).cols;
    cols.extend(super::system_scope_cols(qualifier).into_iter().map(|mut c| {
        c.perm = Some((table.oid, c.attnum));
        c
    }));
    Scope::from_cols(cols)
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
            sc.perm = Some((table.oid, sc.attnum));
            sc
        })
        .collect();
    Scope::from_cols(cols)
}

/// A DML target that is an automatically updatable view: the base table and the view's
/// restrictions.
struct ViewTarget {
    view: String,
    oid: pgmem_catalog::Oid,
    u: crate::views::Updatable,
}

/// The view `name` names, when it does: resolved for writing (`verb`: insert / update /
/// delete), or Postgres' error for a view that is not automatically updatable.
fn view_target(b: &Binder, name: &ir::ObjectName, verb: &str) -> SqlResult<Option<ViewTarget>> {
    let Some(pgmem_catalog::Relation::View(v)) = b.catalog.find_relation(name)? else { return Ok(None) };
    b.lock(v.oid, crate::lock_levels::ROW_EXCLUSIVE);
    match crate::views::resolve(b.catalog, &v)? {
        Ok(u) => Ok(Some(ViewTarget { view: v.name.clone(), oid: v.oid, u })),
        Err(reason) => {
            let (what, gerund, cmd) = match verb {
                "insert" => ("insert into", "inserting into", "INSERT"),
                "update" => ("update", "updating", "UPDATE"),
                _ => ("delete from", "deleting from", "DELETE"),
            };
            Err(SqlError::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("cannot {what} view \"{}\"", v.name))
                .with_detail(reason)
                .with_hint(format!(
                    "To enable {gerund} the view, provide an INSTEAD OF {cmd} trigger or an unconditional ON {cmd} DO INSTEAD rule."
                )))
        }
    }
}

impl ViewTarget {
    /// Binds `f` (the statement on the base table) with the target's column uses noted on the
    /// view's columns.
    fn bind_through<T>(&self, b: &Binder, f: impl FnOnce() -> SqlResult<T>) -> SqlResult<T> {
        let base = lookup_table(b, &self.u.base)?;
        let map = base
            .columns
            .iter()
            .map(|c| {
                let shown = self.u.columns.iter().position(|(_, bc)| bc.as_deref() == Some(c.name.as_str()));
                shown.map_or(0, |j| i16::try_from(j + 1).unwrap_or(i16::MAX))
            })
            .collect();
        let view = match b.catalog.relation_by_oid(self.oid) {
            Some(pgmem_catalog::Relation::View(v)) => Some(v),
            _ => None,
        };
        match view {
            Some(v) => b.as_view_owner(&v, || b.with_target_view((base.oid, self.oid, map), f)),
            None => b.with_target_view((base.oid, self.oid, map), f),
        }
    }

    /// The base column a view column writes (Postgres' error for a computed one).
    fn base_column(&self, column: &str, verb: &str) -> SqlResult<String> {
        match self.u.columns.iter().find(|(n, _)| n == column) {
            Some((_, Some(base))) => Ok(base.clone()),
            Some((_, None)) => Err(SqlError::feature_not_supported(format!("cannot {verb} column \"{column}\" of view \"{}\"", self.view))
                .with_detail("View columns that are not columns of their base relation are not updatable.")),
            None => Err(SqlError::undefined_column(&format!("\"{column}\" of relation \"{}\"", self.view))),
        }
    }

    /// The view's WHERE clauses (`checks`: its CHECK OPTION predicates instead), bound over
    /// the base table's physical row.
    fn bind_predicates(&self, b: &Binder, table: &TableDef, checks: bool) -> SqlResult<Vec<(String, Expr)>> {
        // The view's own conditions run with its owner's privileges.
        b.quietly(|| self.bind_predicates_inner(b, table, checks))
    }

    fn bind_predicates_inner(&self, b: &Binder, table: &TableDef, checks: bool) -> SqlResult<Vec<(String, Expr)>> {
        let items: Vec<(String, ir::Expr, String)> = if checks {
            self.u.checks.clone()
        } else {
            self.u.filters.iter().map(|(e, q)| (self.view.clone(), e.clone(), q.clone())).collect()
        };
        items
            .into_iter()
            .map(|(view, e, q)| {
                let level = Level::new(physical_scope(table, &q));
                Ok((view, b.bind_predicate(&e, "WHERE", &Ctx::new(&level).with_clause(Clause::Where))?))
            })
            .collect()
    }
}

/// ANDs the view's restrictions into a DML filter.
fn and_filters(filter: Option<Expr>, extra: Vec<(String, Expr)>) -> Option<Expr> {
    let mut all: Vec<Expr> = filter.into_iter().collect();
    all.extend(extra.into_iter().map(|(_, e)| e));
    match all.len() {
        0 => None,
        1 => all.pop(),
        _ => Some(Expr::And(all)),
    }
}

pub(crate) fn lookup_table(b: &Binder, name: &ir::ObjectName) -> SqlResult<Arc<TableDef>> {
    let base = name.base();
    match b.catalog.find_relation(name)? {
        Some(pgmem_catalog::Relation::Table(t)) if t.matview.is_some() => {
            Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("cannot change materialized view \"{base}\"")))
        }
        Some(pgmem_catalog::Relation::Table(t)) => Ok(t),
        Some(pgmem_catalog::Relation::View(_)) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("cannot modify view \"{base}\""))
            .with_detail("Views that do not select from a single table or view are not automatically updatable.")),
        Some(other) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("cannot modify {} \"{base}\"", other.kind_name()))),
        None => Err(SqlError::undefined_table(&name.to_string()).about(pgmem_types::ErrorRef::Relation(name.0.join(".")))),
    }
}

impl Binder<'_> {
    /// Binds the expression-valued rules of a table: defaults, generated columns, CHECKs and
    /// unique indexes.
    pub fn bind_row_rules(&self, table: &Arc<TableDef>) -> SqlResult<RowRules> {
        // Defaults, generated columns and constraints read no privileges of the role.
        self.quietly(|| self.bind_row_rules_inner(table))
    }

    fn bind_row_rules_inner(&self, table: &Arc<TableDef>) -> SqlResult<RowRules> {
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
                None if !c.dropped && c.identity.is_none() && c.generated.is_none() && c.ty.is_domain() => {
                    self.domain_default(&c.ty, c.typmod, &c.name, &empty_ctx)?
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
        let mut exclusions = Vec::new();
        for con in &table.constraints {
            let ConstraintKind::Exclude { elements, predicate, .. } = &con.kind else { continue };
            let mut keys = Vec::new();
            let mut ops = Vec::new();
            let mut columns = Vec::new();
            for (e, op) in elements {
                let (k, t) = self.bind_expr(e, &row_ctx)?;
                ops.push(crate::funcs::resolve_binary(op, &t, &t)?.func);
                keys.push(k);
                columns.push(match e {
                    ir::Expr::Column(p) => p.last().cloned().unwrap_or_default(),
                    other => super::like_key_name(other),
                });
            }
            let predicate = predicate.as_ref().map(|p| self.bind_predicate(p, "WHERE", &row_ctx)).transpose()?;
            exclusions.push(ExclusionCheck { name: con.name.clone(), keys, ops, predicate, columns });
        }
        Ok(RowRules { table: Arc::clone(table), defaults, generated, checks, uniques, exclusions })
    }

    /// The default of a column without its own DEFAULT whose type is a domain: the nearest
    /// domain DEFAULT along the domain's base chain, else NULL coerced to the domain (so its
    /// NOT NULL and CHECK constraints apply), else none.
    fn domain_default(&self, ty: &Type, typmod: i32, column: &str, ctx: &Ctx) -> SqlResult<Option<Expr>> {
        let mut cur = ty.clone();
        let mut constrained = false;
        while let Type::Domain(oid) = cur {
            let Some(def) = self.catalog.type_by_oid(oid).filter(|t| t.oid == oid) else { break };
            let pgmem_catalog::TypeDefKind::Domain { base, not_null, default, checks, .. } = &def.kind else { break };
            if let Some(d) = default {
                let (e, t) = self.bind_expr(d, ctx)?;
                return Ok(Some(coerce_assignment(self, e, &t, ty, typmod, column)?));
            }
            constrained |= *not_null || !checks.is_empty();
            cur = base.clone();
        }
        if !constrained {
            return Ok(None);
        }
        Ok(Some(coerce_assignment(self, Expr::null(), &Type::Unknown, ty, typmod, column)?))
    }

    /// Binds RETURNING over the statement's row followed by the old and the new version of
    /// the target row, each with its system columns (Postgres 18's `old.col` / `new.col`, renamed by
    /// `RETURNING WITH (OLD AS o, NEW AS n)`), as the executor evaluates it.
    pub(crate) fn bind_returning(&self, items: &[ir::SelectItem], ctx: &Ctx, table: &TableDef) -> SqlResult<(Vec<Expr>, Vec<OutCol>)> {
        let (aliases, items) = match items.first() {
            Some(ir::SelectItem::Expr { expr: ir::Expr::Function(f), .. }) if f.name.base() == RETURNING_ALIASES => {
                let name = |i: usize| match f.args.get(i) {
                    Some(ir::Expr::Literal(ir::Literal::String(s))) => s.clone(),
                    _ => String::new(),
                };
                ((name(0), name(1)), &items[1..])
            }
            _ => (("old".to_owned(), "new".to_owned()), items),
        };
        if items.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let current = &ctx.current().scope;
        let mut cols = current.cols.clone();
        for alias in [&aliases.0, &aliases.1] {
            cols.extend(self.target_cols(physical_scope(table, alias)).cols.into_iter().chain(super::system_scope_cols(alias)).map(
                |mut c| {
                    c.hidden_unqualified = true;
                    c
                },
            ));
        }
        let level = Level::new(Scope { cols, star: current.star.clone() });
        let rctx = ctx.nested_same_level(&level).with_clause(Clause::Returning);
        let scope = &level.scope;
        let mut exprs = Vec::new();
        let mut cols = Vec::new();
        for item in items {
            match item {
                ir::SelectItem::Wildcard => {
                    for &i in &scope.star {
                        let c = &scope.cols[i];
                        self.note_read(c);
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
                        self.note_read(c);
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
                ir::SelectItem::FieldStar(expr) => {
                    let (e, t) = self.bind_expr(expr, &rctx)?;
                    for (fe, name, ty) in super::expr::expand_fields(e, &t)? {
                        exprs.push(fe);
                        cols.push(OutCol::new(name, ty));
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
fn arbiter_for(b: &Binder, rules: &RowRules, table: &Arc<TableDef>, target: &ir::ConflictTarget) -> SqlResult<Option<UniqueCheck>> {
    match target {
        ir::ConflictTarget::None => Ok(None),
        ir::ConflictTarget::Columns { columns, exprs, selection } => {
            // A partial unique index is only inferred when the ON CONFLICT WHERE clause
            // matches its predicate (Postgres accepts any clause that implies it).
            let row_level = Level::new(b.target_cols(physical_scope(table, &table.name)));
            let row_ctx = Ctx::new(&row_level).with_clause(Clause::Check);
            let selection = selection.as_ref().map(|s| b.bind_predicate(s, "WHERE", &row_ctx)).transpose()?;
            let mut wanted = Vec::new();
            for c in columns {
                wanted.push(table.column_index(c).ok_or_else(|| SqlError::undefined_column(&format!("\"{c}\"")))?);
            }
            // The arbiter's columns are read.
            for &i in &wanted {
                b.note_target_use(table.oid, i16::try_from(i + 1).unwrap_or(i16::MAX), 'r');
            }
            wanted.sort_unstable();
            wanted.dedup();
            // With expression elements, the index's key expressions must be the same set.
            let mut keys: Vec<Expr> = wanted.iter().map(|i| Expr::Col(*i)).collect();
            for e in exprs {
                let bound = b.bind_expr(e, &row_ctx)?.0;
                if !keys.contains(&bound) {
                    keys.push(bound);
                }
            }
            let same_keys = |u: &UniqueCheck| {
                if u.exprs.is_empty() {
                    let mut cols = u.columns.clone();
                    cols.sort_unstable();
                    exprs.is_empty() && cols == wanted
                } else {
                    u.exprs.len() == keys.len() && u.exprs.iter().all(|e| keys.contains(e))
                }
            };
            rules
                .uniques
                .iter()
                .find(|u| (u.predicate.is_none() || u.predicate == selection) && same_keys(u))
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

/// The first RETURNING item `RETURNING WITH (OLD AS o, NEW AS n)` is parsed into.
const RETURNING_ALIASES: &str = "__pgmem_returning_aliases";

pub fn bind_insert(b: &Binder, ins: &ir::Insert, outer: &Ctx) -> SqlResult<InsertPlan> {
    if let Some(vt) = view_target(b, &ins.table, "insert")? {
        // INSERT through a view: the base table, the view's columns mapped onto it.
        let mut base = ins.clone();
        base.table = vt.u.base.clone();
        base.alias = Some(ins.alias.clone().unwrap_or_else(|| vt.view.clone()));
        let names: Vec<String> =
            if ins.columns.is_empty() { vt.u.columns.iter().map(|(n, _)| n.clone()).collect() } else { ins.columns.clone() };
        let needed = match &ins.source {
            ir::InsertSource::Values(rows) => rows.first().map_or(names.len(), Vec::len),
            _ => names.len(),
        };
        base.columns = names.iter().take(needed).map(|c| vt.base_column(c, "insert into")).collect::<SqlResult<Vec<_>>>()?;
        let mut plan = vt.bind_through(b, || bind_insert(b, &base, outer))?;
        plan.view_checks.extend(vt.bind_predicates(b, &plan.rules.table, true)?);
        return Ok(plan);
    }
    let table = lookup_table(b, &ins.table)?;
    b.lock(table.oid, crate::lock_levels::ROW_EXCLUSIVE);
    let qualifier = ins.alias.clone().unwrap_or_else(|| table.name.clone());
    let rules = b.bind_row_rules(&table)?;
    let columns: Vec<usize> = if ins.columns.is_empty() {
        // Without a column list, the source fills the leading columns (the rest get defaults).
        let n = match &ins.source {
            ir::InsertSource::Values(rows) => rows.first().map_or(usize::MAX, Vec::len),
            _ => usize::MAX,
        };
        table.live_columns().map(|(i, _)| i).take(n).collect()
    } else {
        let mut out = Vec::new();
        for c in &ins.columns {
            let idx = table.column_index(c).ok_or_else(|| {
                SqlError::undefined_column(&format!("\"{c}\" of relation \"{}\"", table.name))
                    .about(pgmem_types::ErrorRef::Column(c.clone()))
            })?;
            if out.contains(&idx) {
                return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{c}\" specified more than once")));
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
    let note_inserted = |columns: &[usize]| {
        for &slot in columns {
            b.note_target_use(table.oid, i16::try_from(slot + 1).unwrap_or(i16::MAX), 'a');
        }
    };
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
            note_inserted(&columns);
            InsertRows::Values(out)
        }
        ir::InsertSource::Query(q) => {
            let bound = b.bind_query(q, &vctx)?;
            let columns =
                if ins.columns.is_empty() { columns.iter().copied().take(bound.columns.len()).collect() } else { columns.clone() };
            if bound.columns.len() > columns.len() {
                return Err(SqlError::syntax("INSERT has more expressions than target columns"));
            }
            if bound.columns.len() < columns.len() {
                return Err(SqlError::syntax("INSERT has more target columns than expressions"));
            }
            note_inserted(&columns);
            let mut exprs = Vec::new();
            for (i, (out, &slot)) in bound.columns.iter().zip(&columns).enumerate() {
                let col = &table.columns[slot];
                exprs.push(coerce_assignment(b, Expr::Col(i), &out.ty, &col.ty, col.typmod, &col.name)?);
            }
            InsertRows::Query(Rel::Project { input: Box::new(bound.rel), exprs })
        }
    };
    let row_scope = b.target_cols(physical_scope(&table, &qualifier));
    let ((on_conflict, (returning, returning_cols)), reads) = b.tracking_target_reads(table.oid, || {
        let on_conflict = bind_on_conflict(b, ins, &rules, &table, &row_scope)?;
        let returning_level = Level::new(b.target_cols(target_scope(&table, &qualifier)));
        Ok((on_conflict, b.bind_returning(&ins.returning, &Ctx::new(&returning_level), &table)?))
    })?;
    if on_conflict.as_ref().is_some_and(|oc| oc.update.is_some()) {
        b.rls_unsupported(&table, "INSERT ... ON CONFLICT DO UPDATE")?;
    }
    let rls = b.rls_target(&table, 'a', reads)?;
    Ok(InsertPlan {
        rules,
        columns,
        rows,
        overriding_system_value: ins.overriding_system_value,
        overriding_user_value: ins.overriding_user_value,
        on_conflict,
        returning,
        returning_cols,
        view_checks: Vec::new(),
        rls_checks: rls.checks,
    })
}

/// The ON CONFLICT clause of an INSERT into `table`.
fn bind_on_conflict(
    b: &Binder,
    ins: &ir::Insert,
    rules: &RowRules,
    table: &Arc<TableDef>,
    row_scope: &Scope,
) -> SqlResult<Option<OnConflictPlan>> {
    Ok(match &ins.on_conflict {
        None => None,
        Some(oc) => {
            let arbiter = arbiter_for(b, rules, table, &oc.target)?;
            if let Some(a) = &arbiter
                && table.constraints.iter().any(|c| c.name == a.name && c.deferrable)
            {
                return Err(SqlError::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "ON CONFLICT does not support deferrable unique constraints/exclusion constraints as arbiters",
                ));
            }
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
                        // The proposed row is no read of the table.
                        e.perm = None;
                        cols.push(e);
                    }
                    let combined = Level::new(Scope { star: row_scope.star.clone(), cols });
                    let uctx = Ctx::new(&combined);
                    let assigns = bind_assignments(b, table, assignments, &uctx)?;
                    let filter = selection.as_ref().map(|s| b.bind_predicate(s, "WHERE", &uctx)).transpose()?;
                    Some(DoUpdatePlan { assignments: assigns, filter })
                }
            };
            Some(OnConflictPlan { arbiter, update })
        }
    })
}

pub(crate) fn bind_assignments(
    b: &Binder,
    table: &TableDef,
    assignments: &[ir::Assignment],
    ctx: &Ctx,
) -> SqlResult<Vec<(usize, Option<Expr>)>> {
    let mut out: Vec<(usize, Option<Expr>)> = Vec::new();
    for a in assignments {
        if a.columns.len() > 1
            && let ir::Expr::Subquery(q) = &a.value
        {
            // SET (a, b) = (SELECT x, y ...): one record, its fields assigned in order.
            let (sub, types) = b.bind_row_subquery(q, &ctx.with_clause(Clause::Other))?;
            if types.len() != a.columns.len() {
                return Err(SqlError::syntax("number of columns does not match number of values"));
            }
            for (i, (target, ty)) in a.columns.iter().zip(&types).enumerate() {
                let column = target.last().map_or("", String::as_str);
                let slot = table
                    .column_index(column)
                    .ok_or_else(|| SqlError::undefined_column(&format!("\"{column}\" of relation \"{}\"", table.name)))?;
                let col = &table.columns[slot];
                let field = Expr::Field { expr: Box::new(sub.clone()), idx: i };
                out.push((slot, Some(coerce_assignment(b, field, ty, &col.ty, col.typmod, &col.name)?)));
            }
        } else if a.columns.len() > 1 {
            // SET (a, b) = (x, y)
            let ir::Expr::Row(values) = &a.value else { return Err(super::feature("SET (columns) = (this expression)")) };
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
        b.note_target_use(table.oid, i16::try_from(*slot + 1).unwrap_or(i16::MAX), 'w');
    }
    Ok(out)
}

/// One SET target: `col`, or `col.field` for a composite column. A qualified target whose
/// first part is not a column is taken as `table.col` (pgmem accepts that; Postgres reads it
/// as a field of a column named like the table and fails).
fn bind_one_assignment(b: &Binder, table: &TableDef, target: &[String], value: &ir::Expr, ctx: &Ctx) -> SqlResult<(usize, Option<Expr>)> {
    // `SET a.b = v` assigns field b of column a; Postgres reads a table qualifier the same way.
    let (column, fields): (&str, &[String]) = match target {
        [first, rest @ ..] => (first, rest),
        [] => ("", &[]),
    };
    let slot = table.column_index(column).ok_or_else(|| {
        SqlError::undefined_column(&format!("\"{column}\" of relation \"{}\"", table.name))
            .about(pgmem_types::ErrorRef::Column(column.to_owned()))
    })?;
    let col = &table.columns[slot];
    if let [field] = fields {
        return bind_field_assignment(b, table, slot, field, value, ctx);
    }
    if fields.len() > 1 {
        return Err(super::feature("assignment to nested composite fields"));
    }
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

/// `SET col.field = value`: the column's composite value with one field replaced.
fn bind_field_assignment(
    b: &Binder,
    table: &TableDef,
    slot: usize,
    field: &str,
    value: &ir::Expr,
    ctx: &Ctx,
) -> SqlResult<(usize, Option<Expr>)> {
    let col = &table.columns[slot];
    let Type::Composite(oid) = col.ty.base() else {
        return Err(SqlError::datatype_mismatch(format!(
            "cannot assign to field \"{field}\" of column \"{}\" because its type {} is not a composite type",
            col.name,
            col.ty.name()
        )));
    };
    let fields = crate::usertypes::composite_fields(oid);
    let idx = fields.iter().position(|f| f.name == field).ok_or_else(|| {
        SqlError::undefined_column(&format!("\"{field}\" of relation \"{}\"", table.name)).with_detail(format!(
            "cannot assign to field \"{field}\" of column \"{}\" because there is no such column in data type {}",
            col.name,
            col.ty.name()
        ))
    })?;
    let f = &fields[idx];
    let (e, t) = b.bind_expr(value, &ctx.with_clause(Clause::Other))?;
    let v = coerce_assignment(b, e, &t, &f.ty, f.typmod, &format!("{}.{field}", col.name))?;
    let store = Expr::FieldStore { expr: Box::new(Expr::Col(slot)), idx, value: Box::new(v), type_oid: oid, width: fields.len() };
    Ok((slot, Some(store)))
}

pub fn bind_update(b: &Binder, upd: &ir::Update, outer: &Ctx) -> SqlResult<UpdatePlan> {
    if let Some(vt) = view_target(b, &upd.table, "update")? {
        let mut base = upd.clone();
        base.table = vt.u.base.clone();
        base.alias = Some(upd.alias.clone().unwrap_or_else(|| vt.view.clone()));
        for a in &mut base.assignments {
            for target in &mut a.columns {
                if let Some(first) = target.first_mut() {
                    *first = vt.base_column(first, "update")?;
                }
            }
        }
        let mut plan = vt.bind_through(b, || bind_update(b, &base, outer))?;
        let filters = vt.bind_predicates(b, &plan.rules.table, false)?;
        plan.filter = and_filters(plan.filter.take(), filters);
        plan.view_checks.extend(vt.bind_predicates(b, &plan.rules.table, true)?);
        return Ok(plan);
    }
    let table = lookup_table(b, &upd.table)?;
    b.lock_table_and_indexes(&table, crate::lock_levels::ROW_EXCLUSIVE);
    if !upd.only {
        b.lock_inherit_children(&table, crate::lock_levels::ROW_EXCLUSIVE);
    }
    let qualifier = upd.alias.clone().unwrap_or_else(|| table.name.clone());
    let rules = b.bind_row_rules(&table)?;
    let target_scope = b.target_cols(target_scope(&table, &qualifier));
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
    let ((assignments, filter, (returning, returning_cols)), reads) = b.tracking_target_reads(table.oid, || {
        let assignments = bind_assignments(b, &table, &upd.assignments, &ctx)?;
        let filter = upd.selection.as_ref().map(|s| b.bind_predicate(s, "WHERE", &ctx.with_clause(Clause::Where))).transpose()?;
        Ok((assignments, filter, b.bind_returning(&upd.returning, &ctx, &table)?))
    })?;
    let rls = b.rls_target(&table, 'w', reads)?;
    let filter = super::rls::and_first(rls.quals, filter);
    Ok(UpdatePlan { rules, assignments, from, filter, returning, returning_cols, view_checks: Vec::new(), rls_checks: rls.checks })
}

pub fn bind_delete(b: &Binder, del: &ir::Delete, outer: &Ctx) -> SqlResult<DeletePlan> {
    if let Some(vt) = view_target(b, &del.table, "delete")? {
        let mut base = del.clone();
        base.table = vt.u.base.clone();
        base.alias = Some(del.alias.clone().unwrap_or_else(|| vt.view.clone()));
        let mut plan = vt.bind_through(b, || bind_delete(b, &base, outer))?;
        let filters = vt.bind_predicates(b, &plan.table, false)?;
        plan.filter = and_filters(plan.filter.take(), filters);
        return Ok(plan);
    }
    let table = lookup_table(b, &del.table)?;
    b.lock_table_and_indexes(&table, crate::lock_levels::ROW_EXCLUSIVE);
    if !del.only {
        b.lock_inherit_children(&table, crate::lock_levels::ROW_EXCLUSIVE);
    }
    let qualifier = del.alias.clone().unwrap_or_else(|| table.name.clone());
    let target_scope = b.target_cols(target_scope(&table, &qualifier));
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
    let ((filter, (returning, returning_cols)), reads) = b.tracking_target_reads(table.oid, || {
        let filter = del.selection.as_ref().map(|s| b.bind_predicate(s, "WHERE", &ctx.with_clause(Clause::Where))).transpose()?;
        Ok((filter, b.bind_returning(&del.returning, &ctx, &table)?))
    })?;
    let filter = super::rls::and_first(b.rls_target(&table, 'd', reads)?.quals, filter);
    Ok(DeletePlan { table, using, filter, returning, returning_cols })
}
