//! The relations a statement reads and writes, for privilege checks (Postgres' range-table
//! permission info). Each relation comes with what it needs, in the order Postgres checks
//! them: a query level's own relations (the DML target first, then its FROM / USING / MERGE
//! source), then those of the queries nested in it (subqueries, the INSERT source, WITH
//! queries). A SELECT WITH query nothing references is not run, so it is not checked; a
//! data-modifying one always is.

use pgmem_ir as ir;

/// What a statement needs on a relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// A privilege letter (`r` read, `a` insert, `w` update, `d` delete, `D` truncate):
    /// held on the table, or on every column the statement uses that way (on some column
    /// when it uses none).
    Privilege(char),
    /// A DML target: SELECT on each of its own columns the statement reads, if any.
    TargetRead,
}

/// The relations `stmt` uses, as written (resolve them with the session's search path).
pub(crate) fn statement_uses(stmt: &ir::Statement) -> Vec<(ir::ObjectName, Need)> {
    let mut w = Walk::default();
    w.statement(stmt, &[]);
    w.out
}

enum Nested {
    Query(ir::Query),
    Statement(ir::Statement),
}

#[derive(Default)]
struct Walk {
    out: Vec<(ir::ObjectName, Need)>,
    /// Unqualified names read as relations so far (CTE references).
    seen: Vec<String>,
}

/// The subqueries of `e` (not descending into them: they are walked as nested levels).
/// `Expr::walk` covers operands, CASE, function arguments and FILTER; the ordering and
/// window clauses of a call are walked here.
fn sublinks(e: &ir::Expr, nested: &mut Vec<Nested>) {
    let mut calls: Vec<ir::FunctionCall> = Vec::new();
    e.walk(&mut |x| match x {
        ir::Expr::Subquery(q) | ir::Expr::ArraySubquery(q) => nested.push(Nested::Query((**q).clone())),
        ir::Expr::Exists { query, .. } | ir::Expr::InSubquery { query, .. } => nested.push(Nested::Query((**query).clone())),
        ir::Expr::AnyAll { right: ir::AnyAllRight::Subquery(q), .. } => nested.push(Nested::Query((**q).clone())),
        ir::Expr::Function(call) if !call.order_by.is_empty() || !call.within_group.is_empty() || call.over.is_some() => {
            calls.push((**call).clone());
        }
        _ => {}
    });
    for call in &calls {
        for o in call.order_by.iter().chain(&call.within_group) {
            sublinks(&o.expr, nested);
        }
        if let Some(w) = &call.over {
            window_sublinks(w, nested);
        }
    }
}

fn window_sublinks(w: &ir::WindowSpec, nested: &mut Vec<Nested>) {
    w.partition_by.iter().chain(w.order_by.iter().map(|o| &o.expr)).for_each(|e| sublinks(e, nested));
    if let Some(f) = &w.frame {
        for b in [&f.start, &f.end] {
            if let ir::FrameBound::Preceding(e) | ir::FrameBound::Following(e) = b {
                sublinks(e, nested);
            }
        }
    }
}

fn item_sublinks(items: &[ir::SelectItem], nested: &mut Vec<Nested>) {
    for i in items {
        if let ir::SelectItem::Expr { expr, .. } | ir::SelectItem::FieldStar(expr) = i {
            sublinks(expr, nested);
        }
    }
}

fn assignment_sublinks(assignments: &[ir::Assignment], nested: &mut Vec<Nested>) {
    for a in assignments {
        sublinks(&a.value, nested);
    }
}

/// The CTE names visible in a query level with `with`: (for its body, for each WITH query).
fn cte_scopes(with: Option<&ir::With>, outer: &[String]) -> (Vec<String>, Vec<Vec<String>>) {
    let Some(w) = with else { return (outer.to_vec(), Vec::new()) };
    let mut all = outer.to_vec();
    all.extend(w.ctes.iter().map(|c| c.name.clone()));
    let each = (0..w.ctes.len())
        .map(|i| {
            if w.recursive {
                all.clone()
            } else {
                // A non-recursive WITH query sees only the ones before it.
                let mut s = outer.to_vec();
                s.extend(w.ctes[..i].iter().map(|c| c.name.clone()));
                s
            }
        })
        .collect();
    (all, each)
}

impl Walk {
    fn push(&mut self, name: &ir::ObjectName, need: Need) {
        self.out.push((name.clone(), need));
    }

    /// A relation read in FROM: a WITH query when its unqualified name is one in scope.
    fn read(&mut self, name: &ir::ObjectName, scope: &[String]) {
        if let [only] = name.0.as_slice()
            && scope.iter().any(|c| c == only)
        {
            self.seen.push(only.clone());
            return;
        }
        self.push(name, Need::Privilege('r'));
    }

    fn statement(&mut self, stmt: &ir::Statement, outer: &[String]) {
        let with = match stmt {
            ir::Statement::Query(q) => return self.query(q, outer),
            ir::Statement::Insert(i) => i.with.as_ref(),
            ir::Statement::Update(u) => u.with.as_ref(),
            ir::Statement::Delete(d) => d.with.as_ref(),
            ir::Statement::Merge(m) => m.with.as_ref(),
            ir::Statement::Truncate(t) => {
                for n in &t.tables {
                    self.push(n, Need::Privilege('D'));
                }
                return;
            }
            _ => return,
        };
        let (scope, each) = cte_scopes(with, outer);
        let mut nested = Vec::new();
        match stmt {
            ir::Statement::Insert(i) => self.insert(i, &mut nested),
            ir::Statement::Update(u) => {
                self.push(&u.table, Need::Privilege('w'));
                self.push(&u.table, Need::TargetRead);
                for t in &u.from {
                    self.table_ref(t, &scope, &mut nested);
                }
                assignment_sublinks(&u.assignments, &mut nested);
                u.selection.iter().for_each(|e| sublinks(e, &mut nested));
                item_sublinks(&u.returning, &mut nested);
            }
            ir::Statement::Delete(d) => {
                self.push(&d.table, Need::Privilege('d'));
                self.push(&d.table, Need::TargetRead);
                for t in &d.using {
                    self.table_ref(t, &scope, &mut nested);
                }
                d.selection.iter().for_each(|e| sublinks(e, &mut nested));
                item_sublinks(&d.returning, &mut nested);
            }
            ir::Statement::Merge(m) => self.merge(m, &scope, &mut nested),
            _ => {}
        }
        self.finish(nested, &scope);
        self.ctes(with, &each);
    }

    fn insert(&mut self, i: &ir::Insert, nested: &mut Vec<Nested>) {
        self.push(&i.table, Need::Privilege('a'));
        if let Some(ir::OnConflict { action: ir::ConflictAction::Update { .. }, .. }) = &i.on_conflict {
            self.push(&i.table, Need::Privilege('w'));
        }
        if !i.returning.is_empty() || i.on_conflict.is_some() {
            self.push(&i.table, Need::TargetRead);
        }
        match &i.source {
            ir::InsertSource::Query(q) => nested.push(Nested::Query((**q).clone())),
            ir::InsertSource::Values(rows) => rows.iter().flatten().for_each(|e| sublinks(e, nested)),
            ir::InsertSource::DefaultValues => {}
        }
        // The conflict target is an index inference: a subquery there is an error.
        if let Some(ir::OnConflict { action: ir::ConflictAction::Update { assignments, selection }, .. }) = &i.on_conflict {
            assignment_sublinks(assignments, nested);
            selection.iter().for_each(|e| sublinks(e, nested));
        }
        item_sublinks(&i.returning, nested);
    }

    /// MERGE: the privilege of each action on the target, SELECT on its columns read (on
    /// some column when every action is DO NOTHING), then the source.
    fn merge(&mut self, m: &ir::Merge, scope: &[String], nested: &mut Vec<Nested>) {
        let mut privileges: Vec<char> = Vec::new();
        for c in &m.clauses {
            let p = match &c.action {
                ir::MergeAction::Update(assignments) => {
                    assignment_sublinks(assignments, nested);
                    'w'
                }
                ir::MergeAction::Delete => 'd',
                ir::MergeAction::Insert { values, .. } => {
                    values.iter().flatten().for_each(|e| sublinks(e, nested));
                    'a'
                }
                ir::MergeAction::DoNothing => continue,
            };
            if !privileges.contains(&p) {
                privileges.push(p);
            }
        }
        if privileges.is_empty() {
            self.push(&m.table, Need::Privilege('r'));
        }
        for p in privileges {
            self.push(&m.table, Need::Privilege(p));
        }
        self.push(&m.table, Need::TargetRead);
        self.table_ref(&m.source, scope, nested);
        sublinks(&m.on, nested);
        m.clauses.iter().filter_map(|c| c.condition.as_ref()).for_each(|e| sublinks(e, nested));
        item_sublinks(&m.returning, nested);
    }

    fn query(&mut self, q: &ir::Query, outer: &[String]) {
        let (scope, each) = cte_scopes(q.with.as_ref(), outer);
        let mut nested = Vec::new();
        self.set(&q.body, &scope, &mut nested);
        for e in q.order_by.iter().map(|o| &o.expr).chain(q.limit.iter()).chain(q.offset.iter()) {
            sublinks(e, &mut nested);
        }
        self.finish(nested, &scope);
        self.ctes(q.with.as_ref(), &each);
    }

    fn set(&mut self, s: &ir::SetExpr, scope: &[String], nested: &mut Vec<Nested>) {
        match s {
            ir::SetExpr::Select(sel) => {
                for t in &sel.from {
                    self.table_ref(t, scope, nested);
                }
                item_sublinks(&sel.projection, nested);
                let distinct = match &sel.distinct {
                    ir::Distinct::On(exprs) => exprs.as_slice(),
                    _ => &[],
                };
                for e in sel.selection.iter().chain(&sel.group_by).chain(sel.having.iter()).chain(distinct) {
                    sublinks(e, nested);
                }
                sel.windows.iter().for_each(|w| window_sublinks(&w.spec, nested));
            }
            ir::SetExpr::Values(rows) => rows.iter().flatten().for_each(|e| sublinks(e, nested)),
            ir::SetExpr::SetOp { left, right, .. } => {
                self.set(left, scope, nested);
                self.set(right, scope, nested);
            }
            ir::SetExpr::Query(q) => nested.push(Nested::Query((**q).clone())),
            ir::SetExpr::Dml(stmt) => nested.push(Nested::Statement((**stmt).clone())),
        }
    }

    fn table_ref(&mut self, t: &ir::TableRef, scope: &[String], nested: &mut Vec<Nested>) {
        match t {
            ir::TableRef::Table { name, .. } => self.read(name, scope),
            ir::TableRef::Subquery { query, .. } => nested.push(Nested::Query((**query).clone())),
            ir::TableRef::Function { call, .. } => call.args.iter().for_each(|e| sublinks(e, nested)),
            ir::TableRef::Join { left, right, constraint, .. } => {
                self.table_ref(left, scope, nested);
                self.table_ref(right, scope, nested);
                if let ir::JoinConstraint::On(e) = constraint {
                    sublinks(e, nested);
                }
            }
        }
    }

    fn finish(&mut self, nested: Vec<Nested>, scope: &[String]) {
        for n in nested {
            match n {
                Nested::Query(q) => self.query(&q, scope),
                Nested::Statement(s) => self.statement(&s, scope),
            }
        }
    }

    /// The WITH queries of a level, after its body (whose references decide which SELECT
    /// ones run): walked last to first, so a reference from a later one counts, and listed
    /// in order.
    fn ctes(&mut self, with: Option<&ir::With>, scopes: &[Vec<String>]) {
        let Some(w) = with else { return };
        let mut parts: Vec<Vec<(ir::ObjectName, Need)>> = Vec::with_capacity(w.ctes.len());
        for (cte, scope) in w.ctes.iter().zip(scopes).rev() {
            let dml = matches!(cte.query.body, ir::SetExpr::Dml(_));
            if !dml && !self.seen.contains(&cte.name) {
                continue;
            }
            let saved = std::mem::take(&mut self.out);
            self.query(&cte.query, scope);
            parts.push(std::mem::replace(&mut self.out, saved));
        }
        for p in parts.into_iter().rev() {
            self.out.extend(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uses(sql: &str) -> Vec<(String, Need)> {
        let stmt = pgmem_parse::parse_one(sql).expect("parse");
        statement_uses(&stmt).into_iter().map(|(n, need)| (n.to_string(), need)).collect()
    }

    #[test]
    fn order_and_cte_references() {
        use Need::{Privilege as P, TargetRead as T};
        assert_eq!(
            uses("UPDATE t SET v = (SELECT w FROM s) FROM u WHERE EXISTS (SELECT 1 FROM x)"),
            [("t".into(), P('w')), ("t".into(), T), ("u".into(), P('r')), ("s".into(), P('r')), ("x".into(), P('r'))]
        );
        assert_eq!(uses("WITH c AS (SELECT * FROM s) SELECT 1"), []);
        assert_eq!(
            uses("WITH c AS (SELECT * FROM s), d AS (SELECT * FROM c) SELECT * FROM d, t"),
            [("t".into(), P('r')), ("s".into(), P('r'))]
        );
        assert_eq!(uses("WITH c AS (DELETE FROM s) SELECT * FROM t"), [("t".into(), P('r')), ("s".into(), P('d')), ("s".into(), T)]);
        // A non-recursive WITH query cannot see a later one: that name is a table.
        assert_eq!(uses("WITH a AS (SELECT * FROM b), b AS (SELECT 1) SELECT * FROM a"), [("b".into(), P('r'))]);
        assert_eq!(
            uses("MERGE INTO t USING s ON true WHEN MATCHED THEN DO NOTHING"),
            [("t".into(), P('r')), ("t".into(), T), ("s".into(), P('r'))]
        );
        assert_eq!(
            uses("MERGE INTO t USING s ON true WHEN MATCHED THEN UPDATE SET v = 1 WHEN NOT MATCHED THEN INSERT VALUES (1)"),
            [("t".into(), P('w')), ("t".into(), P('a')), ("t".into(), T), ("s".into(), P('r'))]
        );
    }
}
