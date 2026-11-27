//! Row-level security while binding (`rewrite/rowsecurity.c`): the USING quals of a table's
//! policies filter its scans and the rows UPDATE / DELETE see (`add_security_quals`), and the
//! WITH CHECK conditions new rows of INSERT / UPDATE must satisfy (`add_with_check_options`).
//! Policies apply as the current role, or inside a view as the view's owner (unless the view
//! is `security_invoker`).

use super::{Binder, Clause, Ctx, Level, Scope, ScopeCol};
use crate::plan::{Expr, Rel};
use crate::rls::{RlsCheck, RlsUser};
use pgmem_catalog::{Oid, PolicyDef, TableDef};
use pgmem_types::{SqlError, SqlResult, Value, code};
use std::sync::Arc;

/// What the policies of a DML target add: quals on the rows it reads, checks on the rows it
/// writes.
#[derive(Default)]
pub struct TargetPolicies {
    pub quals: Option<Expr>,
    pub checks: Vec<RlsCheck>,
}

fn false_const() -> Expr {
    Expr::Const(Value::Bool(false))
}

fn or_all(mut v: Vec<Expr>) -> Expr {
    if v.len() == 1 { v.remove(0) } else { Expr::Or(v) }
}

/// `left AND right` with `left` evaluated first (security quals run before the user's).
pub fn and_first(left: Option<Expr>, right: Option<Expr>) -> Option<Expr> {
    match (left, right) {
        (Some(l), Some(r)) => Some(Expr::And(vec![l, r])),
        (l, r) => l.or(r),
    }
}

impl Binder<'_> {
    /// Applies row-level security as `user` to statements bound from now on.
    pub fn set_rls(&self, user: RlsUser) {
        *self.rls.borrow_mut() = Some(user);
    }

    /// Runs `f` with policies applied as `view`'s owner, unless the view is
    /// `security_invoker`.
    pub(crate) fn as_view_owner<T>(&self, view: &pgmem_catalog::ViewDef, f: impl FnOnce() -> T) -> T {
        let current = self.rls.borrow().clone();
        let Some(user) = current else { return f() };
        if crate::reloptions::bool_option(&view.reloptions, "security_invoker") {
            return f();
        }
        let owner = self.catalog.owner(view.oid).unwrap_or(&user.bootstrap).to_owned();
        let previous = self.rls.replace(Some(RlsUser { role: owner, ..user }));
        let r = f();
        *self.rls.borrow_mut() = previous;
        r
    }

    /// Whether `table`'s policies apply to the statement (42501 when `row_security` is off
    /// and they would).
    fn rls_enabled(&self, table: &TableDef) -> SqlResult<Option<String>> {
        let user = self.rls.borrow().clone();
        let Some(user) = user else { return Ok(None) };
        Ok(crate::rls::enabled_for(self.catalog, &user, table, false)?.then_some(user.role))
    }

    /// Runs `f` with relation names in `pins` bound to the pinned relations.
    pub(crate) fn with_pins<T>(&self, pins: Vec<(pgmem_ir::ObjectName, Oid)>, f: impl FnOnce() -> T) -> T {
        let previous = self.rel_pins.replace(pins);
        let r = f();
        *self.rel_pins.borrow_mut() = previous;
        r
    }

    /// The relation a pinned name binds to.
    pub(crate) fn pinned(&self, name: &pgmem_ir::ObjectName) -> Option<pgmem_catalog::Relation> {
        let oid = self.rel_pins.borrow().iter().find(|(n, _)| n == name).map(|(_, o)| *o)?;
        self.catalog.relation_by_oid(oid)
    }

    /// Binds policy expressions of `table` over `scope` (the table's own name qualifies its
    /// columns), detecting policies that read their own table. Each binds with its policy's
    /// creation search_path and relations, as Postgres' stored parse trees do.
    fn bind_policy_exprs(&self, table: &TableDef, scope: &Scope, exprs: &[(&PolicyDef, &pgmem_ir::Expr)]) -> SqlResult<Vec<Expr>> {
        if self.rls_expanding.borrow().contains(&table.oid) {
            return Err(SqlError::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("infinite recursion detected in policy for relation \"{}\"", table.name),
            ));
        }
        self.rls_expanding.borrow_mut().push(table.oid);
        let level = Level::new(scope.clone());
        let ctx = Ctx::new(&level).with_clause(Clause::Policy);
        let bind = |(p, e): &(&PolicyDef, &pgmem_ir::Expr)| {
            pgmem_types::context::with_search_path(&p.search_path, || {
                self.with_pins(p.relations.clone(), || self.bind_predicate(e, "POLICY", &ctx))
            })
        };
        let r = self.quietly(|| exprs.iter().map(bind).collect::<SqlResult<Vec<_>>>());
        self.rls_expanding.borrow_mut().pop();
        r
    }

    /// `add_security_quals`: restrictive USING quals ANDed, then the permissive ones ORed
    /// (FALSE without a permissive USING).
    fn security_quals(&self, table: &TableDef, scope: &Scope, cmd: char, role: &str) -> SqlResult<Vec<Expr>> {
        let (permissive, restrictive) = crate::rls::applicable(self.catalog, table.oid, cmd, role);
        let perm: Vec<(&PolicyDef, &pgmem_ir::Expr)> = permissive.iter().filter_map(|p| p.using.as_ref().map(|e| (&**p, e))).collect();
        if perm.is_empty() {
            return Ok(vec![false_const()]);
        }
        let restr: Vec<(&PolicyDef, &pgmem_ir::Expr)> = restrictive.iter().filter_map(|p| p.using.as_ref().map(|e| (&**p, e))).collect();
        let mut out = self.bind_policy_exprs(table, scope, &restr)?;
        out.push(or_all(self.bind_policy_exprs(table, scope, &perm)?));
        Ok(out)
    }

    /// `add_with_check_options`: one check ORing the permissive policies' WITH CHECK (or
    /// USING), then one per restrictive policy; a FALSE check without permissive ones.
    /// `using_only`: the USING expressions (SELECT policies checked on new rows).
    /// The checks bind over `row`'s physical row (a partition of `table` for a statement run
    /// on the partitions of partitioned `table`).
    fn with_checks(&self, table: &TableDef, row: &TableDef, cmd: char, role: &str, using_only: bool) -> SqlResult<Vec<RlsCheck>> {
        let (permissive, restrictive) = crate::rls::applicable(self.catalog, table.oid, cmd, role);
        let scope = super::dml::physical_scope(row, &table.name);
        let check = |policy: Option<String>, pred: Expr| RlsCheck { policy, table: table.name.clone(), pred };
        let perm: Vec<(&PolicyDef, &pgmem_ir::Expr)> =
            permissive.iter().filter_map(|p| wco_qual(p, using_only).map(|e| (&**p, e))).collect();
        if perm.is_empty() {
            return Ok(vec![check(None, false_const())]);
        }
        let mut out = vec![check(None, or_all(self.bind_policy_exprs(table, &scope, &perm)?))];
        for p in &restrictive {
            if let Some(e) = wco_qual(p, using_only) {
                let mut bound = self.bind_policy_exprs(table, &scope, &[(&**p, e)])?;
                out.push(check(Some(p.name.clone()), bound.remove(0)));
            }
        }
        Ok(out)
    }

    /// Runs `f` (binding a SELECT's FROM) with the relations locked by its FOR UPDATE / SHARE
    /// clauses (`of`; empty: all; None: no locking clause).
    pub(crate) fn with_lock_mode<T>(&self, of: Option<Vec<String>>, f: impl FnOnce() -> T) -> T {
        let previous = self.rls_lock.replace(of);
        let r = f();
        *self.rls_lock.borrow_mut() = previous;
        r
    }

    /// Whether the FROM item named `qualifier` is locked by the locking clause being bound.
    pub(crate) fn locked(&self, qualifier: &str) -> bool {
        self.rls_lock.borrow().as_ref().is_some_and(|of| of.is_empty() || of.iter().any(|n| n == qualifier))
    }

    /// Runs `f` (binding the query of FROM subquery or view `qualifier`) with every relation
    /// of that query's FROM locked when the locking clause reaches it.
    pub(crate) fn locking_into<T>(&self, qualifier: &str, f: impl FnOnce() -> T) -> T {
        self.rls_lock_next.set(self.locked(qualifier));
        let r = f();
        self.rls_lock_next.set(false);
        r
    }

    /// Takes the lock mode [`Binder::locking_into`] passes to the query about to be bound.
    pub(crate) fn take_lock_next(&self) -> Option<Vec<String>> {
        self.rls_lock_next.replace(false).then(Vec::new)
    }

    /// A scan of `table` (columns `cols`, before any alias) filtered by its SELECT policies,
    /// and by its UPDATE policies when a FOR UPDATE / SHARE clause locks it (`locked`).
    pub(crate) fn rls_scan(&self, table: &TableDef, rel: Rel, cols: &[ScopeCol], locked: bool) -> SqlResult<Rel> {
        let Some(role) = self.rls_enabled(table)? else { return Ok(rel) };
        let cols = cols
            .iter()
            .cloned()
            .map(|mut c| {
                c.qualifier = Some(table.name.clone());
                c
            })
            .collect();
        let scope = Scope::from_cols(cols);
        let mut quals = self.security_quals(table, &scope, 'r', &role)?;
        if locked {
            quals.extend(self.security_quals(table, &scope, 'w', &role)?);
        }
        Ok(Rel::Filter { input: Box::new(rel), pred: and_all(quals) })
    }

    /// The policies of DML target `table` for command `cmd` (`a`, `w`, `d`); `reads`: the
    /// statement reads the target's columns, so SELECT policies apply too.
    /// While the statement runs on the partitions of a partitioned table, a partition takes
    /// that table's policies.
    pub(crate) fn rls_target(&self, target: &TableDef, cmd: char, reads: bool) -> SqlResult<TargetPolicies> {
        let root = self.rls_root.borrow().clone();
        let table = root.as_deref().filter(|r| r.oid != target.oid).unwrap_or(target);
        let Some(role) = self.rls_enabled(table)? else { return Ok(TargetPolicies::default()) };
        let mut out = TargetPolicies::default();
        if cmd != 'a' {
            let scope = super::dml::target_scope(target, &table.name);
            let mut quals = Vec::new();
            if reads {
                quals.extend(self.security_quals(table, &scope, 'r', &role)?);
            }
            quals.extend(self.security_quals(table, &scope, cmd, &role)?);
            out.quals = Some(and_all(quals));
        }
        if cmd != 'd' {
            out.checks = self.with_checks(table, target, cmd, &role, false)?;
            if reads {
                out.checks.extend(self.with_checks(table, target, 'r', &role, true)?);
            }
        }
        Ok(out)
    }

    /// Runs `f` (a statement on partitioned `root` running on one of its partitions) with
    /// `root`'s policies applying to the partition as DML target.
    pub(crate) fn with_policy_root<T>(&self, root: &TableDef, f: impl FnOnce() -> T) -> T {
        let previous = self.rls_root.replace(Some(Arc::new(root.clone())));
        let r = f();
        *self.rls_root.borrow_mut() = previous;
        r
    }

    /// Fails for statement kinds pgmem does not apply policies to yet, on tables they would
    /// apply to.
    pub(crate) fn rls_unsupported(&self, table: &TableDef, what: &str) -> SqlResult<()> {
        if self.rls_enabled(table)?.is_some() {
            return Err(SqlError::feature_not_supported(format!(
                "{what} on table \"{}\" with row-level security is not supported by pgmem",
                table.name
            )));
        }
        Ok(())
    }

    /// Runs `f` and reports whether it read columns of DML target `table` (Postgres'
    /// `ACL_SELECT` in the target's required permissions).
    pub(crate) fn tracking_target_reads<T>(&self, table: Oid, f: impl FnOnce() -> SqlResult<T>) -> SqlResult<(T, bool)> {
        let (shown, _) = self.target_perm(table, 1);
        let previous = self.column_use.replace(Some(Vec::new()));
        let r = f();
        let used = self.column_use.replace(previous).unwrap_or_default();
        let reads = used.iter().any(|(rel, _, p)| *p == 'r' && (*rel == table || *rel == shown));
        if let Some(prev) = self.column_use.borrow_mut().as_mut() {
            prev.extend(used);
        }
        Ok((r?, reads))
    }
}

/// `QUAL_FOR_WCO`: a policy's WITH CHECK, else its USING (`using_only`: always USING).
fn wco_qual(p: &PolicyDef, using_only: bool) -> Option<&pgmem_ir::Expr> {
    if using_only { p.using.as_ref() } else { p.with_check.as_ref().or(p.using.as_ref()) }
}

fn and_all(mut v: Vec<Expr>) -> Expr {
    if v.len() == 1 { v.remove(0) } else { Expr::And(v) }
}
