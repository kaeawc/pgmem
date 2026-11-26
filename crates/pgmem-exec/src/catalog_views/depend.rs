//! `pg_depend` and `pg_rewrite`, and the object-address functions that name what they point
//! at (`pg_describe_object`, `pg_identify_object`, `pg_identify_object_as_address`).
//!
//! Dependencies are derived from the catalog the way Postgres 18 records them when the objects
//! are created: references to pinned (builtin) objects are not recorded. pgmem models no toast
//! tables, no internal RI triggers and no extension membership, so their rows are missing.

use super::model::{
    ATTRDEF_CLASS_OID, CLASS_CLASS_OID, CONSTRAINT_CLASS_OID, INFORMATION_SCHEMA_NAMESPACE_OID, Model, NAMESPACE_CLASS_OID, PROC_CLASS_OID,
    TRIGGER_CLASS_OID, attrdef_oid, rowtype_oid,
};
use super::{Spec, ch, name, oid, snapshot, txt};
use crate::eval::EvalCtx;
use crate::funcs::string::quote_ident;
use crate::plan::Row;
use pgmem_catalog::{
    ArgMode, Catalog, ColumnDef, ConstraintKind, FIRST_USER_OID, FuncArg, FuncReturn, Oid, PG_CATALOG_NAMESPACE_OID, PartitionKeyPart,
    Relation, TableDef, TypeDefKind, ViewDef,
};
use pgmem_ir as ir;
use pgmem_types::{SqlResult, Type, Value};

pub const TYPE_CLASS_OID: Oid = 1247;
pub const REWRITE_CLASS_OID: Oid = 2618;
pub const LANGUAGE_CLASS_OID: Oid = 2612;
pub const EXTENSION_CLASS_OID: Oid = 3079;
const PLPGSQL_LANGUAGE_OID: Oid = 13_562;
/// The built-in plpgsql extension's `pg_extension` OID.
pub const PLPGSQL_EXTENSION_OID: Oid = 13_558;
const PG_TOAST_NAMESPACE_OID: Oid = 99;

const RULE_OID_BASE: Oid = 0x3000_0000;
const ROWTYPE_OID_BASE: Oid = 0x6000_0000;
const ROWTYPE_ARRAY_OID_BASE: Oid = 0x6800_0000;

/// The OID of a view's (or materialized view's) `_RETURN` rule.
pub fn rule_oid(relation: Oid) -> Oid {
    RULE_OID_BASE.wrapping_add(relation)
}

/// The OID of the array type of a relation's row type.
pub fn rowtype_array_oid(relation: Oid) -> Oid {
    ROWTYPE_ARRAY_OID_BASE.wrapping_add(relation)
}

/// The relation whose row type (or row type's array) has OID `type_oid`: (relation, is array).
fn rowtype_relation(m: &Model, type_oid: Oid) -> Option<(Oid, bool)> {
    let (rel, array) = if type_oid >= ROWTYPE_ARRAY_OID_BASE {
        (type_oid - ROWTYPE_ARRAY_OID_BASE, true)
    } else if type_oid >= ROWTYPE_OID_BASE {
        (type_oid - ROWTYPE_OID_BASE, false)
    } else {
        return None;
    };
    let exists = m.catalog.table_by_oid(rel).is_some() || m.catalog.views().any(|v| v.oid == rel);
    exists.then_some((rel, array))
}

/// The `pg_type` OID of a column or argument type.
fn type_oid(catalog: &Catalog, ty: &Type) -> Oid {
    match ty {
        Type::Array(e) => match e.as_ref() {
            Type::Enum(o) | Type::Domain(o) | Type::Composite(o) => match catalog.type_by_oid(*o) {
                Some(t) => t.array_oid,
                None if (ROWTYPE_OID_BASE..ROWTYPE_ARRAY_OID_BASE).contains(o) => rowtype_array_oid(*o - ROWTYPE_OID_BASE),
                None => ty.oid(),
            },
            _ => ty.oid(),
        },
        other => other.oid(),
    }
}

/// Types Postgres records dependencies on (builtin types are pinned).
fn user_type(o: Oid) -> bool {
    o >= FIRST_USER_OID
}

// ---------------------------------------------------------------------------------------------
// What an expression or query refers to
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Refs {
    /// Relations read (in FROM, or named by a regclass constant).
    rels: Vec<Oid>,
    /// (relation, attnum) of the columns referenced.
    cols: Vec<(Oid, i32)>,
    funcs: Vec<Oid>,
    types: Vec<Oid>,
}

fn push<T: PartialEq>(v: &mut Vec<T>, x: T) {
    if !v.contains(&x) {
        v.push(x);
    }
}

/// A FROM item: its name and columns (attnum 0 for derived columns).
#[derive(Clone)]
struct Item {
    alias: String,
    rel: Option<Oid>,
    cols: Vec<(String, i32)>,
}

struct Analyzer<'a> {
    catalog: &'a Catalog,
    refs: Refs,
    ctes: Vec<(String, Vec<String>)>,
}

fn input_args(f: &pgmem_catalog::FunctionDef) -> Vec<&FuncArg> {
    f.args.iter().filter(|a| a.mode.is_input()).collect()
}

impl<'a> Analyzer<'a> {
    fn new(catalog: &'a Catalog) -> Analyzer<'a> {
        Analyzer { catalog, refs: Refs::default(), ctes: Vec::new() }
    }

    fn query(&mut self, q: &ir::Query, outer: &[Vec<Item>]) -> Vec<String> {
        let depth = self.ctes.len();
        if let Some(w) = &q.with {
            for c in &w.ctes {
                if w.recursive {
                    self.ctes.push((c.name.clone(), c.columns.clone()));
                }
                let names = self.query(&c.query, outer);
                let cols = if c.columns.is_empty() { names } else { c.columns.clone() };
                if w.recursive {
                    self.ctes.pop();
                }
                self.ctes.push((c.name.clone(), cols));
            }
        }
        let names = self.set(&q.body, outer, &q.order_by);
        for e in q.limit.iter().chain(q.offset.iter()) {
            self.expr(e, outer);
        }
        self.ctes.truncate(depth);
        names
    }

    fn set(&mut self, s: &ir::SetExpr, outer: &[Vec<Item>], order_by: &[ir::OrderByExpr]) -> Vec<String> {
        match s {
            ir::SetExpr::Select(sel) => self.select(sel, outer, order_by),
            ir::SetExpr::Values(rows) => {
                for e in rows.iter().flatten() {
                    self.expr(e, outer);
                }
                (1..=rows.first().map_or(0, Vec::len)).map(|i| format!("column{i}")).collect()
            }
            ir::SetExpr::SetOp { left, right, .. } => {
                let names = self.set(left, outer, &[]);
                self.set(right, outer, &[]);
                names
            }
            ir::SetExpr::Query(q) => self.query(q, outer),
            ir::SetExpr::Dml(_) => Vec::new(),
        }
    }

    fn select(&mut self, sel: &ir::Select, outer: &[Vec<Item>], order_by: &[ir::OrderByExpr]) -> Vec<String> {
        let mut items = Vec::new();
        for t in &sel.from {
            self.table_ref(t, outer, &mut items);
        }
        let mut levels = outer.to_vec();
        levels.push(items.clone());
        let mut names = Vec::new();
        for p in &sel.projection {
            match p {
                ir::SelectItem::Expr { expr, alias } => {
                    self.expr(expr, &levels);
                    names.push(alias.clone().unwrap_or_else(|| output_name(expr)));
                }
                ir::SelectItem::Wildcard => {
                    for it in &items {
                        names.extend(self.all_columns(it));
                    }
                }
                ir::SelectItem::QualifiedWildcard(n) => {
                    if let Some(it) = items.iter().find(|i| i.alias == n.base()) {
                        names.extend(self.all_columns(it));
                    }
                }
                ir::SelectItem::FieldStar(e) => self.expr(e, &levels),
            }
        }
        let mut exprs: Vec<&ir::Expr> = sel.selection.iter().chain(sel.group_by.iter()).chain(sel.having.iter()).collect();
        if let ir::Distinct::On(on) = &sel.distinct {
            exprs.extend(on);
        }
        for w in &sel.windows {
            exprs.extend(w.spec.partition_by.iter());
            exprs.extend(w.spec.order_by.iter().map(|o| &o.expr));
        }
        exprs.extend(order_by.iter().map(|o| &o.expr));
        for e in exprs {
            self.expr(e, &levels);
        }
        names
    }

    fn all_columns(&mut self, it: &Item) -> Vec<String> {
        if let Some(rel) = it.rel {
            for (_, attnum) in &it.cols {
                push(&mut self.refs.cols, (rel, *attnum));
            }
        }
        it.cols.iter().map(|(n, _)| n.clone()).collect()
    }

    fn table_ref(&mut self, t: &ir::TableRef, outer: &[Vec<Item>], items: &mut Vec<Item>) {
        let rename = |cols: &mut Vec<(String, i32)>, alias: Option<&ir::TableAlias>| {
            if let Some(a) = alias {
                for (c, n) in cols.iter_mut().zip(&a.columns) {
                    c.0.clone_from(n);
                }
            }
        };
        match t {
            ir::TableRef::Table { name, alias } => {
                let alias_name = alias.as_ref().map_or_else(|| name.base().to_owned(), |a| a.name.clone());
                if name.0.len() == 1
                    && let Some((_, cols)) = self.ctes.iter().rev().find(|(n, _)| n == name.base())
                {
                    let mut cols: Vec<(String, i32)> = cols.iter().map(|c| (c.clone(), 0)).collect();
                    rename(&mut cols, alias.as_ref());
                    items.push(Item { alias: alias_name, rel: None, cols });
                    return;
                }
                let Some(rel) = self.catalog.find_relation(name).ok().flatten() else { return };
                let mut cols: Vec<(String, i32)> = match &rel {
                    Relation::Table(t) => t.live_columns().map(|(i, c)| (c.name.clone(), attnum(i))).collect(),
                    Relation::View(v) => v.columns.iter().enumerate().map(|(i, c)| (c.clone(), attnum(i))).collect(),
                    Relation::Sequence(_) => {
                        ["last_value", "log_cnt", "is_called"].iter().enumerate().map(|(i, c)| ((*c).to_owned(), attnum(i))).collect()
                    }
                    Relation::Index { .. } => Vec::new(),
                };
                rename(&mut cols, alias.as_ref());
                push(&mut self.refs.rels, rel.oid());
                items.push(Item { alias: alias_name, rel: Some(rel.oid()), cols });
            }
            ir::TableRef::Subquery { query, alias, lateral } => {
                let mut levels = outer.to_vec();
                if *lateral {
                    levels.push(items.clone());
                }
                let names = self.query(query, &levels);
                let mut cols: Vec<(String, i32)> = names.into_iter().map(|n| (n, 0)).collect();
                rename(&mut cols, alias.as_ref());
                items.push(Item { alias: alias.as_ref().map_or_else(String::new, |a| a.name.clone()), rel: None, cols });
            }
            ir::TableRef::Function { call, alias, .. } => {
                let mut levels = outer.to_vec();
                levels.push(items.clone());
                self.expr(&ir::Expr::Function(Box::new(call.clone())), &levels);
                let alias_name = alias.as_ref().map_or_else(|| call.name.base().to_owned(), |a| a.name.clone());
                let cols = match alias {
                    Some(a) if !a.columns.is_empty() => a.columns.iter().map(|c| (c.clone(), 0)).collect(),
                    _ => vec![(alias_name.clone(), 0)],
                };
                items.push(Item { alias: alias_name, rel: None, cols });
            }
            ir::TableRef::Join { left, right, constraint, .. } => {
                let start = items.len();
                self.table_ref(left, outer, items);
                let mid = items.len();
                self.table_ref(right, outer, items);
                let using: Vec<String> = match constraint {
                    ir::JoinConstraint::On(e) => {
                        let mut levels = outer.to_vec();
                        levels.push(items.clone());
                        self.expr(e, &levels);
                        Vec::new()
                    }
                    ir::JoinConstraint::Using(cols) => cols.clone(),
                    ir::JoinConstraint::Natural => {
                        let right_names: Vec<&String> = items[mid..].iter().flat_map(|i| i.cols.iter().map(|(n, _)| n)).collect();
                        items[start..mid]
                            .iter()
                            .flat_map(|i| i.cols.iter().map(|(n, _)| n))
                            .filter(|n| right_names.contains(n))
                            .cloned()
                            .collect()
                    }
                    ir::JoinConstraint::None => Vec::new(),
                };
                for c in &using {
                    for side in [&items[start..mid], &items[mid..]] {
                        if let Some((rel, a)) = find_column(side, c) {
                            push(&mut self.refs.cols, (rel, a));
                        }
                    }
                }
            }
        }
    }

    fn expr(&mut self, e: &ir::Expr, levels: &[Vec<Item>]) {
        let mut columns: Vec<Vec<String>> = Vec::new();
        let mut queries: Vec<ir::Query> = Vec::new();
        let mut calls: Vec<ir::FunctionCall> = Vec::new();
        let mut casts: Vec<(ir::TypeName, Option<String>)> = Vec::new();
        e.walk(&mut |x| match x {
            ir::Expr::Column(parts) => columns.push(parts.clone()),
            ir::Expr::Subquery(q) | ir::Expr::ArraySubquery(q) => queries.push((**q).clone()),
            ir::Expr::Exists { query, .. } | ir::Expr::InSubquery { query, .. } => queries.push((**query).clone()),
            ir::Expr::AnyAll { right: ir::AnyAllRight::Subquery(q), .. } => queries.push((**q).clone()),
            ir::Expr::Function(call) => calls.push((**call).clone()),
            ir::Expr::Cast { expr, ty } => casts.push((ty.clone(), string_literal(expr).map(str::to_owned))),
            ir::Expr::TypedString { ty, value } => casts.push((ty.clone(), Some(value.clone()))),
            _ => {}
        });
        for parts in &columns {
            if let Some((rel, a)) = resolve_column(levels, parts) {
                push(&mut self.refs.cols, (rel, a));
            }
        }
        for q in &queries {
            self.query(q, levels);
        }
        for (ty, literal) in &casts {
            self.cast(ty, literal.as_deref());
        }
        for call in &calls {
            self.call(call, levels);
        }
    }

    fn cast(&mut self, ty: &ir::TypeName, literal: Option<&str>) {
        if ty.name == "regclass" && ty.array_dims == 0 {
            if let Some(rel) = literal.and_then(|l| self.relation_named(l)) {
                push(&mut self.refs.rels, rel);
            }
            return;
        }
        let mut qualified: Vec<String> = ty.schema.iter().cloned().collect();
        qualified.push(ty.name.clone());
        let name = ir::ObjectName(qualified);
        let base = match crate::usertypes::lookup_type(self.catalog, &name).ok().flatten() {
            Some(t) => t,
            None => match Type::from_sql_name(&ty.name) {
                Some(t) => t,
                None => return,
            },
        };
        let t = (0..ty.array_dims).fold(base, |t, _| Type::array_of(t));
        let o = type_oid(self.catalog, &t);
        if user_type(o) {
            push(&mut self.refs.types, o);
        }
    }

    /// The relation a regclass string constant names (`'public.s'`).
    fn relation_named(&self, s: &str) -> Option<Oid> {
        let parts = ir::ObjectName(s.split('.').map(|p| p.trim_matches('"').to_owned()).collect());
        self.catalog.find_relation(&parts).ok().flatten().map(|r| r.oid())
    }

    fn call(&mut self, call: &ir::FunctionCall, levels: &[Vec<Item>]) {
        let mut extra: Vec<&ir::Expr> = call.order_by.iter().chain(call.within_group.iter()).map(|o| &o.expr).collect();
        if let Some(w) = &call.over {
            extra.extend(w.partition_by.iter());
            extra.extend(w.order_by.iter().map(|o| &o.expr));
        }
        for e in extra {
            self.expr(e, levels);
        }
        if matches!(call.name.base(), "nextval" | "currval" | "setval")
            && call.name.0.len() == 1
            && let Some(rel) = call.args.first().and_then(string_literal).and_then(|l| self.relation_named(l))
        {
            push(&mut self.refs.rels, rel);
        }
        let n = call.args.len();
        let candidates = self.catalog.find_functions(&call.name);
        let Some(f) = candidates.iter().find(|f| {
            let inputs = input_args(f);
            let required = inputs.iter().filter(|a| a.default.is_none()).count();
            (required..=inputs.len()).contains(&n) || inputs.last().is_some_and(|a| a.mode == ArgMode::Variadic)
        }) else {
            return;
        };
        push(&mut self.refs.funcs, f.oid);
        // An untyped string argument becomes a constant of the parameter's type.
        for (arg, param) in call.args.iter().zip(input_args(f)) {
            if matches!(arg, ir::Expr::Literal(ir::Literal::String(_))) {
                let o = type_oid(self.catalog, &param.ty);
                if user_type(o) {
                    push(&mut self.refs.types, o);
                }
            }
        }
    }
}

fn attnum(i: usize) -> i32 {
    i32::try_from(i + 1).unwrap_or(0)
}

fn string_literal(e: &ir::Expr) -> Option<&str> {
    match e {
        ir::Expr::Literal(ir::Literal::String(s)) => Some(s),
        ir::Expr::Cast { expr, .. } => string_literal(expr),
        _ => None,
    }
}

/// The column a select-list expression is named after (`?column?` when none).
fn output_name(e: &ir::Expr) -> String {
    match e {
        ir::Expr::Column(parts) => parts.last().cloned().unwrap_or_default(),
        ir::Expr::Function(f) => f.name.base().to_owned(),
        ir::Expr::Cast { expr, ty } => match expr.as_ref() {
            ir::Expr::Column(_) | ir::Expr::Function(_) => output_name(expr),
            _ => ty.name.clone(),
        },
        _ => "?column?".to_owned(),
    }
}

fn find_column(items: &[Item], column: &str) -> Option<(Oid, i32)> {
    items.iter().find_map(|i| i.cols.iter().find(|(n, _)| n == column).and_then(|(_, a)| i.rel.map(|r| (r, *a))))
}

/// The relation column a (possibly qualified) column reference names, innermost scope first.
fn resolve_column(levels: &[Vec<Item>], parts: &[String]) -> Option<(Oid, i32)> {
    for items in levels.iter().rev() {
        match parts {
            [c] => {
                if let Some(item) = items.iter().find(|i| i.cols.iter().any(|(n, _)| n == c)) {
                    return find_column(std::slice::from_ref(item), c);
                }
            }
            [.., t, c] => {
                if let Some(item) = items.iter().find(|i| &i.alias == t) {
                    return find_column(std::slice::from_ref(item), c);
                }
            }
            [] => return None,
        }
    }
    None
}

/// What a view's query references.
fn query_refs(catalog: &Catalog, q: &ir::Query, search_path: &[String]) -> Refs {
    pgmem_types::context::with_search_path(search_path, || {
        let mut a = Analyzer::new(catalog);
        a.query(q, &[]);
        a.refs
    })
}

/// What an expression over table `t`'s columns (a default, CHECK, index key) references.
fn table_expr_refs(catalog: &Catalog, t: &TableDef, exprs: &[&ir::Expr]) -> Refs {
    let path = [t.schema.clone(), "public".to_owned()];
    pgmem_types::context::with_search_path(&path, || {
        let mut a = Analyzer::new(catalog);
        let cols = t.live_columns().map(|(i, c)| (c.name.clone(), attnum(i))).collect();
        let levels = vec![vec![Item { alias: t.name.clone(), rel: Some(t.oid), cols }]];
        for e in exprs {
            a.expr(e, &levels);
        }
        a.refs
    })
}

/// What an expression over no relation (a domain CHECK, an argument default) references.
fn free_expr_refs(catalog: &Catalog, schema: &str, exprs: &[&ir::Expr]) -> Refs {
    let path = [schema.to_owned(), "public".to_owned()];
    pgmem_types::context::with_search_path(&path, || {
        let mut a = Analyzer::new(catalog);
        for e in exprs {
            a.expr(e, &[]);
        }
        a.refs
    })
}

// ---------------------------------------------------------------------------------------------
// pg_depend
// ---------------------------------------------------------------------------------------------

/// An object address: (classid, objid, objsubid).
#[derive(Clone, Copy, PartialEq)]
struct Addr(Oid, Oid, i32);

struct Deps(Vec<(Addr, Addr, char)>);

impl Deps {
    fn add(&mut self, from: Addr, to: Addr, deptype: char) {
        self.0.push((from, to, deptype));
    }

    /// Adds each distinct dependency in `refs` (a whole relation only when none of its columns
    /// is referenced); columns of `self_rel` get `self_type`, everything else `deptype`.
    fn add_refs(&mut self, from: Addr, refs: &Refs, self_rel: Option<Oid>, self_type: char, deptype: char) {
        let mut targets: Vec<(Addr, char)> = Vec::new();
        for (rel, a) in &refs.cols {
            push(&mut targets, (Addr(CLASS_CLASS_OID, *rel, *a), if Some(*rel) == self_rel { self_type } else { deptype }));
        }
        for rel in &refs.rels {
            if Some(*rel) != self_rel && !refs.cols.iter().any(|(r, _)| r == rel) {
                push(&mut targets, (Addr(CLASS_CLASS_OID, *rel, 0), deptype));
            }
        }
        for f in &refs.funcs {
            push(&mut targets, (Addr(PROC_CLASS_OID, *f, 0), deptype));
        }
        for t in &refs.types {
            push(&mut targets, (Addr(TYPE_CLASS_OID, *t, 0), deptype));
        }
        for (to, d) in targets {
            self.add(from, to, d);
        }
    }

    fn namespace(&mut self, from: Addr, ns: Oid) {
        if ns != PG_CATALOG_NAMESPACE_OID && ns != PG_TOAST_NAMESPACE_OID {
            self.add(from, Addr(NAMESPACE_CLASS_OID, ns, 0), 'n');
        }
    }

    fn column_type(&mut self, rel: Oid, a: i32, o: Oid) {
        if user_type(o) {
            self.add(Addr(CLASS_CLASS_OID, rel, a), Addr(TYPE_CLASS_OID, o, 0), 'n');
        }
    }

    /// A relation's row type and its array type.
    fn rowtype(&mut self, rel: Oid) {
        self.add(Addr(TYPE_CLASS_OID, rowtype_oid(rel), 0), Addr(CLASS_CLASS_OID, rel, 0), 'i');
        self.add(Addr(TYPE_CLASS_OID, rowtype_array_oid(rel), 0), Addr(TYPE_CLASS_OID, rowtype_oid(rel), 0), 'i');
    }

    fn rule(&mut self, catalog: &Catalog, rel: Oid, q: &ir::Query, search_path: &[String]) {
        let r = Addr(REWRITE_CLASS_OID, rule_oid(rel), 0);
        self.add(r, Addr(CLASS_CLASS_OID, rel, 0), 'i');
        self.add_refs(r, &query_refs(catalog, q, search_path), None, 'n', 'n');
    }
}

/// Whether a column has a `pg_attrdef` row (as `pg_attrdef` lists them: no identity columns,
/// no `DEFAULT NULL`).
fn has_attrdef(catalog: &Catalog, t: &TableDef, c: &ColumnDef) -> bool {
    c.generated.is_some() || crate::deparse::deparse_default(catalog, t, c).ok().flatten().is_some()
}

fn relation_deps(m: &Model, d: &mut Deps) {
    let catalog = m.catalog;
    let constraints = m.constraints();
    for t in m.tables() {
        let me = Addr(CLASS_CLASS_OID, t.oid, 0);
        d.namespace(me, m.namespace(&t.schema));
        d.rowtype(t.oid);
        if let Some(mv) = &t.matview {
            d.rule(catalog, t.oid, &mv.query, &mv.search_path);
        }
        if let Some(of) = catalog.partition_of(t.oid) {
            d.add(me, Addr(CLASS_CLASS_OID, of.parent, 0), 'a');
        }
        if let Some(key) = catalog.partition_key(t.oid) {
            for part in &key.parts {
                let cols: Vec<i32> = match part {
                    PartitionKeyPart::Column(i) => vec![attnum(*i)],
                    PartitionKeyPart::Expr(e) => table_expr_refs(catalog, t, &[e]).cols.iter().map(|(_, a)| *a).collect(),
                };
                for a in cols {
                    d.add(Addr(CLASS_CLASS_OID, t.oid, a), me, 'i');
                }
            }
        }
        for (i, c) in t.live_columns() {
            d.column_type(t.oid, attnum(i), type_oid(catalog, &c.ty));
            let (expr, own) = match (&c.generated, &c.default) {
                (Some(g), _) => (g, 'i'),
                (None, Some(e)) if has_attrdef(catalog, t, c) => (e, 'a'),
                _ => continue,
            };
            let ad = Addr(ATTRDEF_CLASS_OID, attrdef_oid(t.oid, i + 1), 0);
            d.add(ad, Addr(CLASS_CLASS_OID, t.oid, attnum(i)), own);
            d.add_refs(ad, &table_expr_refs(catalog, t, &[expr]), Some(t.oid), 'n', 'n');
        }
        for idx in &t.indexes {
            let me = Addr(CLASS_CLASS_OID, idx.oid, 0);
            // An index enforcing a constraint depends on the constraint only.
            if let Some(con) =
                constraints.iter().find(|c| c.conindid == idx.oid && c.table.oid == t.oid && matches!(c.contype, 'p' | 'u' | 'x'))
            {
                d.add(me, Addr(CONSTRAINT_CLASS_OID, con.oid, 0), 'i');
                continue;
            }
            for c in &idx.columns {
                d.add(me, Addr(CLASS_CLASS_OID, t.oid, attnum(*c)), 'a');
            }
            if idx.columns.is_empty() {
                d.add(me, Addr(CLASS_CLASS_OID, t.oid, 0), 'a');
            }
            let exprs: Vec<&ir::Expr> = idx.exprs.iter().chain(idx.predicate.iter()).collect();
            let mut refs = table_expr_refs(catalog, t, &exprs);
            refs.cols.retain(|(r, a)| *r != t.oid || !idx.columns.iter().any(|c| attnum(*c) == *a));
            d.add_refs(me, &refs, Some(t.oid), 'a', 'n');
        }
    }
    for c in &constraints {
        let me = Addr(CONSTRAINT_CLASS_OID, c.oid, 0);
        for a in &c.conkey {
            d.add(me, Addr(CLASS_CLASS_OID, c.table.oid, i32::try_from(*a).unwrap_or(0)), 'a');
        }
        match c.kind {
            Some(ConstraintKind::Check { expr }) => {
                d.add_refs(me, &table_expr_refs(catalog, c.table, &[expr]), Some(c.table.oid), 'n', 'n')
            }
            Some(ConstraintKind::ForeignKey { .. }) => {
                for a in &c.confkey {
                    d.add(me, Addr(CLASS_CLASS_OID, c.confrelid, i32::try_from(*a).unwrap_or(0)), 'n');
                }
                if c.conindid != 0 {
                    d.add(me, Addr(CLASS_CLASS_OID, c.conindid, 0), 'n');
                }
            }
            _ => {}
        }
    }
    for v in m.views() {
        let me = Addr(CLASS_CLASS_OID, v.oid, 0);
        d.namespace(me, m.namespace(&v.schema));
        d.rowtype(v.oid);
        d.rule(catalog, v.oid, &v.query, &v.search_path);
        let atts = pgmem_types::context::with_search_path(&v.search_path, || m.view_atts(v));
        for a in atts {
            d.column_type(v.oid, attnum(a.attnum - 1), type_oid(catalog, &a.ty));
        }
    }
    for seq in m.sequences() {
        let me = Addr(CLASS_CLASS_OID, seq.oid, 0);
        d.namespace(me, m.namespace(&seq.schema));
        let Some((tname, cname)) = &seq.owned_by else { continue };
        let Some(t) = catalog.table(&seq.schema, tname) else { continue };
        let Some(i) = t.column_index(cname) else { continue };
        let identity = t.columns.get(i).is_some_and(|c| c.identity.is_some());
        d.add(me, Addr(CLASS_CLASS_OID, t.oid, attnum(i)), if identity { 'i' } else { 'a' });
    }
}

fn type_deps(m: &Model, d: &mut Deps) {
    let catalog = m.catalog;
    for ty in catalog.types() {
        let me = Addr(TYPE_CLASS_OID, ty.oid, 0);
        d.namespace(me, m.namespace(&ty.schema));
        d.add(Addr(TYPE_CLASS_OID, ty.array_oid, 0), me, 'i');
        match &ty.kind {
            TypeDefKind::Composite { fields, relid } => {
                d.add(Addr(CLASS_CLASS_OID, *relid, 0), me, 'i');
                for (i, f) in fields.iter().enumerate() {
                    d.column_type(*relid, attnum(i), type_oid(catalog, &f.ty));
                }
            }
            TypeDefKind::Domain { base, default, checks, .. } => {
                let b = type_oid(catalog, base);
                if user_type(b) {
                    d.add(me, Addr(TYPE_CLASS_OID, b, 0), 'n');
                    // A constant default is cooked to a constant of the base type.
                    if default.as_ref().is_some_and(|e| string_literal(e).is_some()) {
                        d.add(me, Addr(TYPE_CLASS_OID, b, 0), 'n');
                    }
                }
                for con in super::domain_constraints(catalog, ty) {
                    let ca = Addr(CONSTRAINT_CLASS_OID, con.oid, 0);
                    d.add(ca, me, 'a');
                    if let Some(check) = checks.iter().find(|c| c.oid == con.oid) {
                        d.add_refs(ca, &free_expr_refs(catalog, &ty.schema, &[&check.expr]), None, 'n', 'n');
                    }
                }
            }
            TypeDefKind::Enum { .. } => {}
        }
    }
}

fn routine_deps(m: &Model, d: &mut Deps) {
    let catalog = m.catalog;
    for f in catalog.functions() {
        let me = Addr(PROC_CLASS_OID, f.oid, 0);
        d.namespace(me, m.namespace(&f.schema));
        if f.language == "plpgsql" {
            d.add(me, Addr(LANGUAGE_CLASS_OID, PLPGSQL_LANGUAGE_OID, 0), 'n');
        }
        let mut types: Vec<Oid> = Vec::new();
        match &f.ret {
            FuncReturn::Scalar(t) | FuncReturn::SetOf(t) => push(&mut types, type_oid(catalog, t)),
            FuncReturn::Table(cols) => cols.iter().for_each(|(_, t)| push(&mut types, type_oid(catalog, t))),
            FuncReturn::None => {}
        }
        for a in &f.args {
            push(&mut types, type_oid(catalog, &a.ty));
        }
        for t in types.into_iter().filter(|t| user_type(*t)) {
            d.add(me, Addr(TYPE_CLASS_OID, t, 0), 'n');
        }
        // Argument defaults are recorded separately, cooked to the parameter types.
        let defaults: Vec<&ir::Expr> = f.args.iter().filter_map(|a| a.default.as_ref()).collect();
        let mut refs = free_expr_refs(catalog, &f.schema, &defaults);
        for a in &f.args {
            let o = type_oid(catalog, &a.ty);
            if a.default.as_ref().is_some_and(|e| string_literal(e).is_some()) && user_type(o) {
                push(&mut refs.types, o);
            }
        }
        d.add_refs(me, &refs, None, 'n', 'n');
        // ALTER FUNCTION ... DEPENDS ON EXTENSION: 'x'.
        for e in &f.extensions {
            let ext = if e == "plpgsql" { Some(PLPGSQL_EXTENSION_OID) } else { catalog.extension(e).map(|x| x.oid) };
            if let Some(ext) = ext {
                d.add(me, Addr(EXTENSION_CLASS_OID, ext, 0), 'x');
            }
        }
    }
    for tr in catalog.triggers() {
        let me = Addr(TRIGGER_CLASS_OID, tr.oid, 0);
        d.add(me, Addr(PROC_CLASS_OID, tr.function, 0), 'n');
        d.add(me, Addr(CLASS_CLASS_OID, tr.table, 0), 'a');
        if let (Some(cols), Some(t)) = (&tr.update, catalog.table_by_oid(tr.table)) {
            for c in cols {
                if let Some(i) = t.column_index(c) {
                    d.add(me, Addr(CLASS_CLASS_OID, t.oid, attnum(i)), 'n');
                }
            }
        }
    }
    for e in catalog.extensions() {
        d.namespace(Addr(EXTENSION_CLASS_OID, e.oid, 0), m.namespace(&e.schema));
    }
}

fn dependencies(m: &Model) -> Vec<(Addr, Addr, char)> {
    let mut d = Deps(Vec::new());
    relation_deps(m, &mut d);
    type_deps(m, &mut d);
    routine_deps(m, &mut d);
    d.0
}

pub(super) fn pg_depend(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(super::pg_catalog::PG_DEPEND);
    Ok(dependencies(&m)
        .into_iter()
        .map(|(from, to, deptype)| {
            s.row(vec![
                ("classid", oid(from.0)),
                ("objid", oid(from.1)),
                ("objsubid", Value::Int4(from.2)),
                ("refclassid", oid(to.0)),
                ("refobjid", oid(to.1)),
                ("refobjsubid", Value::Int4(to.2)),
                ("deptype", ch(deptype)),
            ])
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// pg_rewrite
// ---------------------------------------------------------------------------------------------

pub(super) const PG_REWRITE: &str = "oid:oid, rulename:name, ev_class:oid, ev_type:\"char\"=1, ev_enabled:\"char\"=O, \
    is_instead:bool=t, ev_qual:pg_node_tree=<>, ev_action:pg_node_tree";

/// A materialized view as a view (for deparsing its query).
fn matview_as_view(t: &TableDef) -> Option<ViewDef> {
    let mv = t.matview.as_ref()?;
    Some(ViewDef {
        oid: t.oid,
        schema: t.schema.clone(),
        name: t.name.clone(),
        columns: t.live_columns().map(|(_, c)| c.name.clone()).collect(),
        query: mv.query.clone(),
        search_path: mv.search_path.clone(),
        check_option: false,
        reloptions: t.reloptions.clone(),
    })
}

/// Views and materialized views: the relations with a `_RETURN` rule.
fn rule_relations(m: &Model) -> Vec<ViewDef> {
    let mut out: Vec<ViewDef> = m.views().into_iter().cloned().collect();
    out.extend(m.tables().into_iter().filter_map(matview_as_view));
    out
}

pub(super) fn pg_rewrite(ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let m = snapshot(ctx);
    let s = Spec::parse(PG_REWRITE);
    Ok(rule_relations(&m)
        .iter()
        .map(|v| {
            // Postgres stores the query's node tree; pgmem keeps the view's SQL text there.
            let action = crate::deparse::deparse_view(m.catalog, v).unwrap_or_default();
            s.row(vec![("oid", oid(rule_oid(v.oid))), ("rulename", name("_RETURN")), ("ev_class", oid(v.oid)), ("ev_action", txt(action))])
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Object addresses
// ---------------------------------------------------------------------------------------------

/// A relation as the object-address functions name it.
struct Rel {
    schema: String,
    name: String,
    /// `pg_identify_object`'s type: table, view, index, ...
    kind: &'static str,
    visible: bool,
    columns: Vec<String>,
}

fn rel_info(m: &Model, o: Oid) -> Option<Rel> {
    let catalog = m.catalog;
    let rel = |schema: &str, name: &str, kind, columns| Rel {
        schema: schema.to_owned(),
        name: name.to_owned(),
        kind,
        visible: catalog.relation_visible(schema, name),
        columns,
    };
    if let Some(r) = catalog.relation_by_oid(o) {
        return Some(match &r {
            Relation::Table(t) => {
                let kind = if t.matview.is_some() { "materialized view" } else { "table" };
                rel(&t.schema, &t.name, kind, Model::table_atts(t).into_iter().map(|a| a.name).collect())
            }
            Relation::View(v) => rel(&v.schema, &v.name, "view", v.columns.clone()),
            Relation::Sequence(s) => {
                rel(&s.schema, &s.name, "sequence", ["last_value", "log_cnt", "is_called"].map(str::to_owned).to_vec())
            }
            Relation::Index { table, index } => {
                let cols = catalog.table_q(table).map(|t| Model::index_atts(t, index).into_iter().map(|a| a.name).collect());
                rel(&table.schema, &index.name, "index", cols.unwrap_or_default())
            }
        });
    }
    for t in catalog.types() {
        if let TypeDefKind::Composite { fields, relid } = &t.kind
            && *relid == o
        {
            return Some(Rel {
                schema: t.schema.clone(),
                name: t.name.clone(),
                kind: "composite type",
                visible: catalog.type_visible(&t.schema, &t.name),
                columns: fields.iter().map(|f| f.name.clone()).collect(),
            });
        }
    }
    let d = super::system_relation_by_oid(o)?;
    Some(Rel {
        schema: d.schema.to_owned(),
        name: d.name.to_owned(),
        kind: if d.kind == 'v' { "view" } else { "table" },
        visible: d.schema == "pg_catalog",
        columns: Spec::parse(d.spec).columns().into_iter().map(|(n, _)| n).collect(),
    })
}

impl Rel {
    fn description(&self) -> String {
        let name = if self.visible { quote_ident(&self.name) } else { self.identity() };
        format!("{} {name}", self.kind)
    }

    fn column(&self, sub: i32) -> Option<&str> {
        usize::try_from(sub).ok().and_then(|i| i.checked_sub(1)).and_then(|i| self.columns.get(i)).map(String::as_str)
    }

    fn identity(&self) -> String {
        qualified(&self.schema, &self.name)
    }
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// A type: schema, typname, `format_type_be` text and `format_type_be_qualified` text.
struct TypeInfo {
    schema: String,
    typname: String,
    short: String,
    full: String,
}

fn type_info(m: &Model, o: Oid) -> Option<TypeInfo> {
    let catalog = m.catalog;
    for t in catalog.types() {
        if t.oid != o && t.array_oid != o {
            continue;
        }
        let visible = catalog.type_visible(&t.schema, &t.name);
        let short = if visible { quote_ident(&t.name) } else { qualified(&t.schema, &t.name) };
        let full = qualified(&t.schema, &t.name);
        let schema = t.schema.clone();
        return Some(if t.oid == o {
            TypeInfo { schema, typname: t.name.clone(), short, full }
        } else {
            TypeInfo { schema, typname: format!("_{}", t.name), short: format!("{short}[]"), full: format!("{full}[]") }
        });
    }
    if let Some((rel, array)) = rowtype_relation(m, o) {
        let r = rel_info(m, rel)?;
        let short = if r.visible { quote_ident(&r.name) } else { r.identity() };
        let full = r.identity();
        return Some(if array {
            TypeInfo { typname: format!("_{}", r.name), schema: r.schema, short: format!("{short}[]"), full: format!("{full}[]") }
        } else {
            TypeInfo { schema: r.schema, typname: r.name, short, full }
        });
    }
    let (typname, formatted) = builtin_type_names(m, o)?;
    let full = builtin_qualified(&typname, &formatted);
    Some(TypeInfo { schema: "pg_catalog".to_owned(), typname, short: formatted, full })
}

/// (typname, format_type) of a builtin type.
fn builtin_type_names(m: &Model, o: Oid) -> Option<(String, String)> {
    if let Some((_, n, f)) = super::builtin_type_name(|x, _| x == o) {
        return Some((n.to_owned(), f.to_owned()));
    }
    let b = super::pg_catalog::builtin_type(o)?;
    let typname = (*b.fields.get(1)?).to_owned();
    let formatted = crate::funcs::catalog::format_type(o, -1, m.catalog).unwrap_or_else(|| typname.clone());
    Some((typname, formatted))
}

/// `format_type_be_qualified` of a builtin type: SQL-standard names stay bare, the others get
/// `pg_catalog.`.
fn builtin_qualified(typname: &str, formatted: &str) -> String {
    if let Some(elem) = formatted.strip_suffix("[]") {
        let elem_name = typname.strip_prefix('_').unwrap_or(typname);
        return format!("{}[]", builtin_qualified(elem_name, elem));
    }
    if typname == formatted { format!("pg_catalog.{}", quote_ident(typname)) } else { formatted.to_owned() }
}

/// A function: schema, name, whether the search path finds it, input argument types, and
/// whether it is a procedure.
struct ProcInfo {
    schema: String,
    name: String,
    visible: bool,
    args: Vec<Oid>,
    procedure: bool,
}

fn proc_info(m: &Model, o: Oid) -> Option<ProcInfo> {
    if let Some(f) = m.catalog.function_by_oid(o) {
        let args = input_args(f).iter().map(|a| type_oid(m.catalog, &a.ty)).collect();
        return Some(ProcInfo {
            schema: f.schema.clone(),
            name: f.name.clone(),
            visible: m.catalog.function_visible(f),
            args,
            procedure: f.procedure,
        });
    }
    let p = super::builtin_procs().iter().find(|p| p.oid == o)?;
    Some(ProcInfo { schema: "pg_catalog".to_owned(), name: p.name.to_owned(), visible: true, args: p.args.clone(), procedure: false })
}

fn type_text(m: &Model, o: Oid, qualified: bool) -> String {
    type_info(m, o).map_or_else(|| "???".to_owned(), |t| if qualified { t.full } else { t.short })
}

fn namespace_name(m: &Model, o: Oid) -> Option<String> {
    match o {
        PG_CATALOG_NAMESPACE_OID => Some("pg_catalog".to_owned()),
        PG_TOAST_NAMESPACE_OID => Some("pg_toast".to_owned()),
        INFORMATION_SCHEMA_NAMESPACE_OID => Some("information_schema".to_owned()),
        _ => m.catalog.schema_by_oid(o).map(|s| s.name.clone()),
    }
}

fn language_name(o: Oid) -> Option<&'static str> {
    Some(match o {
        12 => "internal",
        13 => "c",
        14 => "sql",
        PLPGSQL_LANGUAGE_OID => "plpgsql",
        _ => return None,
    })
}

fn extension_name(m: &Model, o: Oid) -> Option<String> {
    if o == PLPGSQL_EXTENSION_OID {
        return Some("plpgsql".to_owned());
    }
    m.catalog.extensions().find(|e| e.oid == o).map(|e| e.name.clone())
}

/// What a constraint belongs to.
enum ConOwner {
    Table(Oid),
    Domain(Oid),
}

fn constraint_info(m: &Model, o: Oid) -> Option<(String, ConOwner)> {
    if let Some(c) = m.constraint_by_oid(o) {
        return Some((c.name, ConOwner::Table(c.table.oid)));
    }
    m.catalog
        .types()
        .find_map(|t| super::domain_constraints(m.catalog, t).into_iter().find(|c| c.oid == o).map(|c| (c.name, ConOwner::Domain(t.oid))))
}

/// The column a default belongs to: (table OID, attnum).
fn attrdef_info(m: &Model, o: Oid) -> Option<(Oid, i32)> {
    m.tables().into_iter().find_map(|t| {
        t.live_columns().find(|(i, c)| has_attrdef(m.catalog, t, c) && attrdef_oid(t.oid, i + 1) == o).map(|(i, _)| (t.oid, attnum(i)))
    })
}

fn rule_info(m: &Model, o: Oid) -> Option<Oid> {
    rule_relations(m).iter().map(|v| v.oid).find(|v| rule_oid(*v) == o)
}

/// A relation's description, or its column's (`objsubid` > 0).
fn rel_description(m: &Model, rel: Oid, sub: i32) -> Option<String> {
    let r = rel_info(m, rel)?;
    if sub == 0 {
        return Some(r.description());
    }
    Some(format!("column {} of {}", r.column(sub)?, r.description()))
}

/// `pg_describe_object(classid, objid, objsubid)`.
fn describe_object(m: &Model, class: Oid, o: Oid, sub: i32) -> Option<String> {
    match class {
        CLASS_CLASS_OID => rel_description(m, o, sub),
        TYPE_CLASS_OID => type_info(m, o).map(|t| format!("type {}", t.short)),
        PROC_CLASS_OID => {
            let p = proc_info(m, o)?;
            let fname = if p.visible { quote_ident(&p.name) } else { qualified(&p.schema, &p.name) };
            let args: Vec<String> = p.args.iter().map(|a| type_text(m, *a, false)).collect();
            Some(format!("function {fname}({})", args.join(",")))
        }
        NAMESPACE_CLASS_OID => namespace_name(m, o).map(|n| format!("schema {n}")),
        LANGUAGE_CLASS_OID => language_name(o).map(|n| format!("language {n}")),
        EXTENSION_CLASS_OID => extension_name(m, o).map(|n| format!("extension {n}")),
        CONSTRAINT_CLASS_OID => {
            let (cname, owner) = constraint_info(m, o)?;
            Some(match owner {
                ConOwner::Table(t) => format!("constraint {cname} on {}", rel_description(m, t, 0)?),
                ConOwner::Domain(_) => format!("constraint {cname}"),
            })
        }
        ATTRDEF_CLASS_OID => {
            let (t, a) = attrdef_info(m, o)?;
            Some(format!("default value for {}", rel_description(m, t, a)?))
        }
        REWRITE_CLASS_OID => Some(format!("rule _RETURN on {}", rel_description(m, rule_info(m, o)?, 0)?)),
        TRIGGER_CLASS_OID => {
            let t = m.catalog.triggers().find(|t| t.oid == o)?;
            Some(format!("trigger {} on {}", t.name, rel_description(m, t.table, 0)?))
        }
        _ => None,
    }
}

/// `pg_identify_object` (type, schema, name, identity) and `pg_identify_object_as_address`
/// (type, object_names, object_args) of an object.
struct Identity {
    kind: String,
    schema: Option<String>,
    name: Option<String>,
    identity: String,
    names: Vec<String>,
    args: Vec<String>,
}

fn class_kind(class: Oid) -> Option<&'static str> {
    Some(match class {
        CLASS_CLASS_OID => "relation",
        TYPE_CLASS_OID => "type",
        PROC_CLASS_OID => "function",
        NAMESPACE_CLASS_OID => "schema",
        LANGUAGE_CLASS_OID => "language",
        EXTENSION_CLASS_OID => "extension",
        CONSTRAINT_CLASS_OID => "constraint",
        ATTRDEF_CLASS_OID => "default value",
        REWRITE_CLASS_OID => "rule",
        TRIGGER_CLASS_OID => "trigger",
        _ => return None,
    })
}

fn ident(kind: &str, schema: Option<String>, name: Option<String>, identity: String, names: Vec<String>, args: Vec<String>) -> Identity {
    Identity { kind: kind.to_owned(), schema, name, identity, names, args }
}

/// A named object outside any schema (schema, language, extension).
fn global(kind: &str, n: String) -> Identity {
    ident(kind, None, Some(quote_ident(&n)), quote_ident(&n), vec![n], vec![])
}

fn identify(m: &Model, class: Oid, o: Oid, sub: i32) -> Option<Identity> {
    match class {
        CLASS_CLASS_OID => {
            let r = rel_info(m, o)?;
            if sub == 0 {
                return Some(ident(r.kind, Some(r.schema.clone()), Some(r.name.clone()), r.identity(), vec![r.schema, r.name], vec![]));
            }
            let col = r.column(sub)?.to_owned();
            let identity = format!("{}.{}", r.identity(), quote_ident(&col));
            let kind = format!("{} column", r.kind);
            Some(ident(&kind, Some(r.schema.clone()), Some(r.name.clone()), identity, vec![r.schema, r.name, col], vec![]))
        }
        TYPE_CLASS_OID => {
            let t = type_info(m, o)?;
            Some(ident("type", Some(t.schema), Some(quote_ident(&t.typname)), t.full.clone(), vec![t.full], vec![]))
        }
        PROC_CLASS_OID => {
            let p = proc_info(m, o)?;
            let args: Vec<String> = p.args.iter().map(|a| type_text(m, *a, true)).collect();
            let identity = format!("{}({})", qualified(&p.schema, &p.name), args.join(","));
            let kind = if p.procedure { "procedure" } else { "function" };
            Some(ident(kind, Some(p.schema.clone()), None, identity, vec![p.schema, p.name], args))
        }
        NAMESPACE_CLASS_OID => namespace_name(m, o).map(|n| global("schema", n)),
        LANGUAGE_CLASS_OID => language_name(o).map(|n| global("language", n.to_owned())),
        EXTENSION_CLASS_OID => extension_name(m, o).map(|n| global("extension", n)),
        CONSTRAINT_CLASS_OID => match constraint_info(m, o)? {
            (cname, ConOwner::Table(t)) => {
                let r = rel_info(m, t)?;
                let identity = format!("{} on {}", quote_ident(&cname), r.identity());
                Some(ident("table constraint", Some(r.schema.clone()), None, identity, vec![r.schema, r.name, cname], vec![]))
            }
            (cname, ConOwner::Domain(d)) => {
                let t = type_info(m, d)?;
                let identity = format!("{} on {}", quote_ident(&cname), t.full);
                Some(ident("domain constraint", Some(t.schema), None, identity, vec![t.full], vec![cname]))
            }
        },
        ATTRDEF_CLASS_OID => {
            let (t, a) = attrdef_info(m, o)?;
            let r = rel_info(m, t)?;
            let col = r.column(a)?.to_owned();
            let identity = format!("for {}.{}", r.identity(), quote_ident(&col));
            Some(ident("default value", None, None, identity, vec![r.schema, r.name, col], vec![]))
        }
        REWRITE_CLASS_OID => {
            let r = rel_info(m, rule_info(m, o)?)?;
            let identity = format!("{} on {}", quote_ident("_RETURN"), r.identity());
            Some(ident("rule", None, None, identity, vec![r.schema, r.name, "_RETURN".to_owned()], vec![]))
        }
        TRIGGER_CLASS_OID => {
            let t = m.catalog.triggers().find(|t| t.oid == o)?;
            let r = rel_info(m, t.table)?;
            let identity = format!("{} on {}", quote_ident(&t.name), r.identity());
            Some(ident("trigger", None, None, identity, vec![r.schema, r.name, t.name.clone()], vec![]))
        }
        _ => None,
    }
}

fn address_args(a: &[Value]) -> Option<(Oid, Oid, i32)> {
    let o = |v: &Value| match v {
        Value::Oid(o) | Value::Reg(_, o, _) => Some(*o),
        other => other.as_i64().and_then(|n| u32::try_from(n).ok()),
    };
    Some((o(a.first()?)?, o(a.get(1)?)?, a.get(2)?.as_i64().and_then(|n| i32::try_from(n).ok())?))
}

fn opt_text(s: Option<String>) -> Value {
    s.map_or(Value::Null, Value::Text)
}

fn text_array(items: Vec<String>) -> Value {
    Value::Array(items.into_iter().map(Value::Text).collect())
}

/// `pg_describe_object(classid oid, objid oid, objsubid int4)`.
pub fn describe_object_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let Some((class, o, sub)) = address_args(a) else { return Ok(Value::Null) };
    Ok(opt_text(describe_object(&snapshot(ctx), class, o, sub)))
}

/// `pg_identify_object(classid, objid, objsubid)`: one row (type, schema, name, identity).
pub fn identify_object_srf(a: &[Value], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let Some((class, o, sub)) = address_args(a) else { return Ok(vec![vec![Value::Null; 4]]) };
    let row = match identify(&snapshot(ctx), class, o, sub) {
        Some(i) => vec![Value::Text(i.kind), opt_text(i.schema), opt_text(i.name), Value::Text(i.identity)],
        None => vec![opt_text(class_kind(class).map(str::to_owned)), Value::Null, Value::Null, Value::Null],
    };
    Ok(vec![row])
}

/// `pg_identify_object_as_address(classid, objid, objsubid)`: one row (type, object_names,
/// object_args).
pub fn identify_object_as_address_srf(a: &[Value], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let Some((class, o, sub)) = address_args(a) else { return Ok(vec![vec![Value::Null; 3]]) };
    let row = match identify(&snapshot(ctx), class, o, sub) {
        Some(i) => vec![Value::Text(i.kind), text_array(i.names), text_array(i.args)],
        None => vec![opt_text(class_kind(class).map(str::to_owned)), Value::Null, Value::Null],
    };
    Ok(vec![row])
}
