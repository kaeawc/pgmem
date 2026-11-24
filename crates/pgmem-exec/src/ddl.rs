//! DDL execution against the transaction's catalog.

use crate::bind::ddl::{AlterOutcome, CreateTablePlan, Validation};
use crate::bind::{self, Binder, Ctx, Level, Scope};
use crate::cast::cast_value_ctx;
use crate::eval::{EvalCtx, StatementState, eval, eval_pred};
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, ConstraintKind, QualName, Relation, TableDef};
use pgmem_ir::{self as ir, ObjectType, Statement};
use pgmem_storage::Txn;
use pgmem_types::{GroupKey, SqlError, SqlResult, Value, code, text};
use std::sync::Arc;

/// `pg_type`'s OID, the `classoid` of comments on types.
const TYPE_CLASS_OID: u32 = 1247;

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

fn notice(t: &str, msg: String) -> QueryResult {
    QueryResult { tag: t.to_owned(), notices: vec![Notice { severity: "NOTICE", code: "00000", message: msg }], ..Default::default() }
}

/// The `pg_description` key a `COMMENT ON` refers to, or None for object kinds pgmem keeps no
/// catalog entry for.
fn comment_target(
    catalog: &pgmem_catalog::Catalog,
    target: &ir::CommentTarget,
    name: &ir::ObjectName,
) -> SqlResult<Option<(u32, u32, i32)>> {
    use crate::catalog_views::{CLASS_CLASS_OID, NAMESPACE_CLASS_OID};
    use ir::CommentTarget as T;
    let relation = |n: &ir::ObjectName| -> SqlResult<Relation> {
        catalog.lookup_relation(n)?.ok_or_else(|| SqlError::undefined_table(&n.to_string()))
    };
    let not_a = |r: &Relation, what: &str| SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not {what}", r.name()));
    Ok(Some(match target {
        T::Table | T::View | T::MaterializedView | T::Sequence | T::Index => {
            let r = relation(name)?;
            let ok = matches!(
                (target, &r),
                (T::Table, Relation::Table(_))
                    | (T::View, Relation::View(_))
                    | (T::Sequence, Relation::Sequence(_))
                    | (T::Index, Relation::Index { .. })
            );
            if !ok {
                return Err(not_a(
                    &r,
                    match target {
                        T::Table => "a table",
                        T::View => "a view",
                        T::MaterializedView => "a materialized view",
                        T::Sequence => "a sequence",
                        _ => "an index",
                    },
                ));
            }
            (CLASS_CLASS_OID, r.oid(), 0)
        }
        T::Column => {
            let (table, column) = match name.0.split_last() {
                Some((c, t)) if !t.is_empty() => (ir::ObjectName(t.to_vec()), c.clone()),
                _ => return Err(SqlError::syntax("column name must be qualified")),
            };
            let r = relation(&table)?;
            let attnum = match &r {
                Relation::Table(t) => t.column_index(&column).map(|i| i + 1),
                Relation::View(v) => v.columns.iter().position(|c| *c == column).map(|i| i + 1),
                _ => None,
            }
            .ok_or_else(|| SqlError::undefined_column(&format!("\"{column}\" of relation \"{}\"", r.name())))?;
            (CLASS_CLASS_OID, r.oid(), i32::try_from(attnum).unwrap_or(i32::MAX))
        }
        T::Schema => {
            let oid = catalog.namespace_oid(name.base()).ok_or_else(|| pgmem_catalog::undefined_schema(name.base()))?;
            (NAMESPACE_CLASS_OID, oid, 0)
        }
        T::Type | T::Domain => {
            let base = name.base();
            let ty = catalog
                .lookup_type(name)?
                .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{base}\" does not exist")))?;
            if *target == T::Domain && !matches!(ty.kind, pgmem_catalog::TypeDefKind::Domain { .. }) {
                return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is not a domain")));
            }
            (TYPE_CLASS_OID, ty.oid, 0)
        }
        T::Other => return Ok(None),
    }))
}

pub fn execute_ddl(stmt: &Statement, txn: &mut Txn, binder: &Binder, env: &SessionEnv, state: &StatementState) -> SqlResult<QueryResult> {
    match stmt {
        Statement::Comment { target, name, text } => {
            if let Some((classoid, objoid, subid)) = comment_target(txn.catalog(), target, name)? {
                // An empty comment removes it, as in Postgres.
                let text = text.clone().filter(|t| !t.is_empty());
                txn.catalog_mut().set_comment(classoid, objoid, subid, text);
            }
            Ok(tag("COMMENT"))
        }
        Statement::CreateTable(ct) => {
            let Some(CreateTablePlan { mut table, sequences }) = bind::bind_create_table(binder, ct)? else {
                let name = ct.name.base();
                return Ok(notice("CREATE TABLE", format!("relation \"{name}\" already exists, skipping")));
            };
            let cat = txn.catalog_mut();
            for mut seq in sequences {
                seq.oid = cat.alloc_oid();
                cat.create_sequence(seq)?;
            }
            assign_oids(cat, &mut table);
            let oid = table.oid;
            cat.create_table(table)?;
            txn.created_table(oid);
            Ok(tag("CREATE TABLE"))
        }
        Statement::CreateView(cv) => {
            let mut def = bind::bind_create_view(binder, cv)?;
            let cat = txn.catalog_mut();
            def.oid = cat.alloc_oid();
            cat.create_view(def, cv.or_replace)?;
            Ok(tag("CREATE VIEW"))
        }
        Statement::CreateSequence(cs) => {
            let Some(mut seq) = bind::bind_create_sequence(binder, cs)? else {
                let name = cs.name.base();
                return Ok(notice("CREATE SEQUENCE", format!("relation \"{name}\" already exists, skipping")));
            };
            let cat = txn.catalog_mut();
            seq.oid = cat.alloc_oid();
            cat.create_sequence(seq)?;
            Ok(tag("CREATE SEQUENCE"))
        }
        Statement::CreateIndex(ci) => {
            let Some((tname, mut idx)) = bind::bind_create_index(binder, ci)? else {
                return Ok(notice(
                    "CREATE INDEX",
                    format!("relation \"{}\" already exists, skipping", ci.name.clone().unwrap_or_default()),
                ));
            };
            let table = txn.catalog().table_q(&tname).cloned().ok_or_else(|| SqlError::undefined_table(&tname.name))?;
            let mut def = (*table).clone();
            idx.oid = txn.catalog_mut().alloc_oid();
            let unique = idx.unique;
            let name = idx.name.clone();
            def.indexes.push(idx);
            if unique {
                validate_unique(txn, &def, &name, env, state)?;
            }
            txn.catalog_mut().replace_table(&tname, def);
            Ok(tag("CREATE INDEX"))
        }
        Statement::AlterTable(at) => {
            let name = at.name.base().to_owned();
            let current = match txn.catalog().lookup_relation(&at.name)? {
                Some(Relation::Table(t)) => t.qualified(),
                Some(r) => {
                    return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{name}\" is not a table (it is a {})", r.kind_name())));
                }
                None if at.if_exists => return Ok(notice("ALTER TABLE", format!("relation \"{name}\" does not exist, skipping"))),
                None => return Err(SqlError::undefined_table(&at.name.to_string())),
            };
            let mut current = current;
            for action in &at.actions {
                current = alter_table_action(txn, &current, action, env, state)?;
            }
            Ok(tag("ALTER TABLE"))
        }
        Statement::CreateType(ct) => crate::typeddl::create_type(ct, txn, binder),
        Statement::CreateDomain(cd) => crate::typeddl::create_domain(cd, txn, binder),
        Statement::AlterType(at) => crate::typeddl::alter_type(at, txn, binder),
        Statement::AlterDomain(ad) => crate::typeddl::alter_domain(ad, txn, binder, env, state),
        Statement::Drop(d) if matches!(d.object_type, ObjectType::Type | ObjectType::Domain) => {
            crate::typeddl::drop_types(d, txn, env, state)
        }
        Statement::Drop(d) => drop(d, txn, binder),
        Statement::Truncate(t) => truncate(t, txn, binder),
        Statement::SetSchema { object_type, name, if_exists, schema } => set_schema(*object_type, name, *if_exists, schema, txn),
        other => Err(SqlError::feature_not_supported(format!("statement {other:?} is not supported by pgmem"))),
    }
}

/// Applies one ALTER TABLE action to table `current`; returns the table's name afterwards.
pub(crate) fn alter_table_action(
    txn: &mut Txn,
    current: &QualName,
    action: &ir::AlterTableAction,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QualName> {
    let table = txn.catalog().table_q(current).cloned().ok_or_else(|| SqlError::undefined_table(&current.name))?;
    let has_rows = !txn.scan(table.oid).is_empty();
    let snapshot = txn.catalog().clone();
    let b = Binder::new(&snapshot, &[]);
    match bind::bind_alter_table(&b, &table, action, has_rows)? {
        AlterOutcome::Nothing => Ok(current.clone()),
        AlterOutcome::Replace { def, rename_from, sequences, validate } => {
            let mut def = *def;
            assign_oids(txn.catalog_mut(), &mut def);
            let mut renamed_seqs = Vec::new();
            for mut seq in sequences {
                seq.oid = txn.catalog_mut().alloc_oid();
                renamed_seqs.push(seq);
            }
            for seq in renamed_seqs {
                txn.catalog_mut().create_sequence(seq)?;
            }
            if let ir::AlterTableAction::RenameColumn { old, new } = action {
                rename_column_refs(&mut def, old, new);
            }
            apply_validation(txn, &table, &def, validate, &snapshot, env, state)?;
            if let Some(old) = &rename_from {
                rename_table_refs(txn.catalog_mut(), old, &def.name);
            }
            let new_name = def.qualified();
            txn.catalog_mut().replace_table(rename_from.as_ref().unwrap_or(current), def);
            Ok(new_name)
        }
    }
}

fn assign_oids(cat: &mut Catalog, table: &mut TableDef) {
    if table.oid == 0 {
        table.oid = cat.alloc_oid();
    }
    for c in &mut table.constraints {
        if c.oid == 0 {
            c.oid = cat.alloc_oid();
        }
    }
    for i in &mut table.indexes {
        if i.oid == 0 {
            i.oid = cat.alloc_oid();
        }
    }
}

fn display(v: &Value) -> String {
    if v.is_null() { "null".into() } else { text::encode(v, &v.natural_type()).unwrap_or_default() }
}

pub(crate) fn padded_rows(txn: &Txn, def: &TableDef) -> Vec<(u64, Vec<Value>)> {
    txn.scan(def.oid)
        .into_iter()
        .map(|(id, mut r)| {
            while r.len() < def.columns.len() {
                r.push(def.columns[r.len()].missing.clone());
            }
            (id, r)
        })
        .collect()
}

/// `CREATE UNIQUE INDEX` / `ADD PRIMARY KEY` over existing rows.
fn validate_unique(txn: &Txn, def: &TableDef, index: &str, env: &SessionEnv, state: &StatementState) -> SqlResult<()> {
    let mut catalog = txn.catalog().clone();
    catalog.replace_table(&def.qualified(), def.clone());
    let b = Binder::new(&catalog, &[]);
    let rules = b.bind_row_rules(&Arc::new(def.clone()))?;
    let Some(u) = rules.uniques.iter().find(|u| u.name == index) else { return Ok(()) };
    let ctx = EvalCtx::new(txn, &[], env, state);
    let mut seen = std::collections::HashSet::new();
    for (_, row) in padded_rows(txn, def) {
        if let Some(p) = &u.predicate
            && !eval_pred(p, &row, &ctx)?
        {
            continue;
        }
        let vals: Vec<Value> = if u.exprs.is_empty() {
            u.columns.iter().map(|c| row[*c].clone()).collect()
        } else {
            u.exprs.iter().map(|e| eval(e, &row, &ctx)).collect::<SqlResult<_>>()?
        };
        if !u.nulls_not_distinct && vals.iter().any(Value::is_null) {
            continue;
        }
        let key: Vec<GroupKey> = vals.iter().map(Value::group_key).collect();
        if !seen.insert(key) {
            let cols: Vec<String> =
                if u.exprs.is_empty() { u.columns.iter().map(|c| def.columns[*c].name.clone()).collect() } else { vec!["expr".into()] };
            return Err(SqlError::new(code::UNIQUE_VIOLATION, format!("could not create unique index \"{index}\"")).with_detail(format!(
                "Key ({})=({}) is duplicated.",
                cols.join(", "),
                vals.iter().map(display).collect::<Vec<_>>().join(", ")
            )));
        }
    }
    Ok(())
}

fn apply_validation(
    txn: &mut Txn,
    old: &TableDef,
    def: &TableDef,
    validate: Validation,
    snapshot: &Catalog,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<()> {
    match validate {
        Validation::None => Ok(()),
        Validation::NotNull(slot) => {
            if padded_rows(txn, def).iter().any(|(_, r)| r[slot].is_null()) {
                return Err(SqlError::new(
                    code::NOT_NULL_VIOLATION,
                    format!("column \"{}\" of relation \"{}\" contains null values", def.columns[slot].name, def.name),
                ));
            }
            Ok(())
        }
        Validation::Constraint(name) => {
            let con = def.constraints.iter().find(|c| c.name == name).cloned().ok_or_else(|| SqlError::internal("constraint vanished"))?;
            match &con.kind {
                ConstraintKind::Check { expr } => {
                    let mut catalog = snapshot.clone();
                    catalog.replace_table(&def.qualified(), def.clone());
                    let b = Binder::new(&catalog, &[]);
                    let level = Level::new(bind::dml::physical_scope(def, &def.name));
                    let pred = b.bind_predicate(expr, "CHECK", &Ctx::new(&level))?;
                    let ctx = EvalCtx::new(txn, &[], env, state);
                    for (_, row) in padded_rows(txn, def) {
                        if matches!(eval(&pred, &row, &ctx)?, Value::Bool(false)) {
                            return Err(SqlError::new(
                                code::CHECK_VIOLATION,
                                format!("check constraint \"{name}\" of relation \"{}\" is violated by some row", def.name),
                            ));
                        }
                    }
                    Ok(())
                }
                ConstraintKind::PrimaryKey { columns } | ConstraintKind::Unique { columns, .. } => {
                    if matches!(con.kind, ConstraintKind::PrimaryKey { .. }) {
                        for (_, row) in padded_rows(txn, def) {
                            if let Some(c) = columns.iter().find(|c| row[**c].is_null()) {
                                return Err(SqlError::new(
                                    code::NOT_NULL_VIOLATION,
                                    format!("column \"{}\" of relation \"{}\" contains null values", def.columns[*c].name, def.name),
                                ));
                            }
                        }
                    }
                    validate_unique(txn, def, &name, env, state)
                }
                ConstraintKind::ForeignKey { columns, ref_table, ref_columns, .. } => {
                    let parent = if *ref_table == def.qualified() {
                        Arc::new(def.clone())
                    } else {
                        txn.catalog().table_q(ref_table).cloned().ok_or_else(|| SqlError::undefined_table(&ref_table.name))?
                    };
                    let ref_table = &ref_table.name;
                    let parents = padded_rows(txn, &parent);
                    for (_, row) in padded_rows(txn, def) {
                        let key: Vec<Value> = columns.iter().map(|c| row[*c].clone()).collect();
                        if key.iter().any(Value::is_null) {
                            continue;
                        }
                        let found = parents.iter().any(|(_, p)| {
                            ref_columns.iter().zip(&key).all(|(rc, k)| pgmem_types::compare_values(&p[*rc], k).is_ok_and(|o| o.is_eq()))
                        });
                        if !found {
                            let names: Vec<&str> = columns.iter().map(|c| def.columns[*c].name.as_str()).collect();
                            return Err(SqlError::new(
                                code::FOREIGN_KEY_VIOLATION,
                                format!("insert or update on table \"{}\" violates foreign key constraint \"{name}\"", def.name),
                            )
                            .with_detail(format!(
                                "Key ({})=({}) is not present in table \"{ref_table}\".",
                                names.join(", "),
                                key.iter().map(display).collect::<Vec<_>>().join(", ")
                            )));
                        }
                    }
                    Ok(())
                }
            }
        }
        Validation::ConvertColumn { slot, using } => {
            let column = def.columns[slot].clone();
            // Compute new values with an immutable view, then rewrite.
            let mut catalog = snapshot.clone();
            catalog.replace_table(&old.qualified(), old.clone());
            let b = Binder::new(&catalog, &[]);
            let new_slot = slot >= old.columns.len();
            let (using_expr, default_expr, generated_expr) = {
                let level = Level::new(bind::dml::physical_scope(old, &old.name));
                let ctx = Ctx::new(&level);
                let u = using.as_ref().map(|u| b.bind_expr(u, &ctx)).transpose()?;
                let rules_def = Arc::new(def.clone());
                let mut cat2 = snapshot.clone();
                cat2.replace_table(&def.qualified(), def.clone());
                let b2 = Binder::new(&cat2, &[]);
                let rules = b2.bind_row_rules(&rules_def)?;
                (u, rules.defaults[slot].clone(), rules.generated[slot].clone())
            };
            let rows = padded_rows(txn, old);
            let mut new_values = Vec::with_capacity(rows.len());
            // Converting into a domain checks its constraints on every value.
            let domain_check = if column.ty.is_domain() {
                Some(bind::coerce_assignment(&b, crate::plan::Expr::Col(0), &column.ty.base(), &column.ty, column.typmod, &column.name)?)
            } else {
                None
            };
            {
                let ctx = EvalCtx::new(txn, &[], env, state);
                for (_, row) in &rows {
                    let v = if new_slot {
                        let mut r = row.clone();
                        r.push(Value::Null);
                        match (&generated_expr, &default_expr) {
                            (Some(g), _) => eval(g, &r, &ctx)?,
                            (None, Some(d)) => eval(d, &[], &ctx)?,
                            (None, None) => Value::Null,
                        }
                    } else {
                        let src = match &using_expr {
                            Some((e, _)) => eval(e, row, &ctx)?,
                            None => row[slot].clone(),
                        };
                        cast_value_ctx(src, &column.ty, column.typmod, false).map_err(|e| {
                            if e.code == code::CANNOT_COERCE {
                                SqlError::datatype_mismatch(format!(
                                    "column \"{}\" cannot be cast automatically to type {}",
                                    column.name,
                                    column.ty.name()
                                ))
                                .with_hint(format!(
                                    "You might need to specify \"USING {}::{}\".",
                                    column.name,
                                    column.ty.name()
                                ))
                            } else {
                                e
                            }
                        })?
                    };
                    if column.not_null && v.is_null() {
                        return Err(SqlError::new(
                            code::NOT_NULL_VIOLATION,
                            format!("column \"{}\" of relation \"{}\" contains null values", column.name, def.name),
                        ));
                    }
                    if let Some(check) = &domain_check {
                        eval(check, std::slice::from_ref(&v), &ctx)?;
                    }
                    new_values.push(v);
                }
            }
            let mut i = 0;
            txn.rewrite(old.oid, |row| {
                while row.len() < old.columns.len() {
                    row.push(old.columns[row.len()].missing.clone());
                }
                if new_slot {
                    row.push(new_values.get(i).cloned().unwrap_or(Value::Null));
                } else if let Some(v) = new_values.get(i) {
                    row[slot] = v.clone();
                }
                i += 1;
            });
            Ok(())
        }
    }
}

fn rename_in_expr(e: &mut ir::Expr, table: &str, old: &str, new: &str) {
    if let ir::Expr::Column(parts) = e {
        let n = parts.len();
        if parts[n - 1] == old && (n == 1 || parts[n - 2] == table) {
            parts[n - 1] = new.to_owned();
        }
        return;
    }
    // Walk children via Debug-free structural recursion.
    match e {
        ir::Expr::Unary { expr, .. }
        | ir::Expr::Not(expr)
        | ir::Expr::IsNull { expr, .. }
        | ir::Expr::IsBool { expr, .. }
        | ir::Expr::Cast { expr, .. }
        | ir::Expr::Collate { expr, .. } => rename_in_expr(expr, table, old, new),
        ir::Expr::Binary { left, right, .. }
        | ir::Expr::And(left, right)
        | ir::Expr::Or(left, right)
        | ir::Expr::IsDistinctFrom { left, right, .. } => {
            rename_in_expr(left, table, old, new);
            rename_in_expr(right, table, old, new);
        }
        ir::Expr::Between { expr, low, high, .. } => {
            rename_in_expr(expr, table, old, new);
            rename_in_expr(low, table, old, new);
            rename_in_expr(high, table, old, new);
        }
        ir::Expr::InList { expr, list, .. } => {
            rename_in_expr(expr, table, old, new);
            list.iter_mut().for_each(|x| rename_in_expr(x, table, old, new));
        }
        ir::Expr::Like { expr, pattern, .. } | ir::Expr::SimilarTo { expr, pattern, .. } => {
            rename_in_expr(expr, table, old, new);
            rename_in_expr(pattern, table, old, new);
        }
        ir::Expr::Case { operand, whens, else_result } => {
            if let Some(o) = operand {
                rename_in_expr(o, table, old, new);
            }
            for (w, t) in whens {
                rename_in_expr(w, table, old, new);
                rename_in_expr(t, table, old, new);
            }
            if let Some(x) = else_result {
                rename_in_expr(x, table, old, new);
            }
        }
        ir::Expr::Function(f) => f.args.iter_mut().for_each(|x| rename_in_expr(x, table, old, new)),
        ir::Expr::Array(items) | ir::Expr::Row(items) => items.iter_mut().for_each(|x| rename_in_expr(x, table, old, new)),
        ir::Expr::AnyAll { left, right, .. } => {
            rename_in_expr(left, table, old, new);
            if let ir::AnyAllRight::Array(a) = right {
                rename_in_expr(a, table, old, new);
            }
        }
        _ => {}
    }
}

/// Keeps stored expressions pointing at a renamed column (Postgres stores attnums).
fn rename_column_refs(def: &mut TableDef, old: &str, new: &str) {
    let table = def.name.clone();
    for c in &mut def.columns {
        if let Some(d) = &mut c.default {
            rename_in_expr(d, &table, old, new);
        }
        if let Some(g) = &mut c.generated {
            rename_in_expr(g, &table, old, new);
        }
    }
    for con in &mut def.constraints {
        if let ConstraintKind::Check { expr } = &mut con.kind {
            rename_in_expr(expr, &table, old, new);
        }
    }
    for idx in &mut def.indexes {
        idx.exprs.iter_mut().for_each(|e| rename_in_expr(e, &table, old, new));
        if let Some(p) = &mut idx.predicate {
            rename_in_expr(p, &table, old, new);
        }
    }
}

/// Updates references to a renamed table in other tables' foreign keys and sequence owners.
fn rename_table_refs(cat: &mut Catalog, old: &QualName, new: &str) {
    let renamed = QualName::new(&old.schema, new);
    let tables: Vec<TableDef> = cat.tables().map(|t| (**t).clone()).collect();
    for mut t in tables {
        let mut changed = false;
        for con in &mut t.constraints {
            if let ConstraintKind::ForeignKey { ref_table, .. } = &mut con.kind
                && ref_table == old
            {
                *ref_table = renamed.clone();
                changed = true;
            }
        }
        if changed {
            let key = t.qualified();
            cat.replace_table(&key, t);
        }
    }
    let seqs: Vec<pgmem_catalog::SequenceDef> = cat
        .sequences()
        .filter(|s| s.schema == old.schema && s.owned_by.as_ref().is_some_and(|(t, _)| *t == old.name))
        .map(|s| (**s).clone())
        .collect();
    for mut s in seqs {
        let key = QualName::new(&s.schema, &s.name);
        if let Some((t, _)) = &mut s.owned_by {
            *t = new.to_owned();
        }
        cat.replace_sequence(&key, s);
    }
}

fn dependents_error(what: &str, name: &str, detail: String) -> SqlError {
    SqlError::new(code::DEPENDENT_OBJECTS_STILL_EXIST, format!("cannot drop {what} {name} because other objects depend on it"))
        .with_detail(detail)
        .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
}

fn drop(d: &ir::Drop, txn: &mut Txn, binder: &Binder) -> SqlResult<QueryResult> {
    let kind = match d.object_type {
        ObjectType::Table => "TABLE",
        ObjectType::View => "VIEW",
        ObjectType::Index => "INDEX",
        ObjectType::Sequence => "SEQUENCE",
        ObjectType::Schema => return drop_schemas(d, txn),
        ObjectType::Extension => return Ok(tag("DROP EXTENSION")),
        ObjectType::Type | ObjectType::Domain => return Err(SqlError::internal("DROP TYPE is handled by typeddl")),
        ObjectType::Function => return Err(SqlError::feature_not_supported("DROP FUNCTION is not supported by pgmem")),
    };
    let _ = binder;
    let mut notices = Vec::new();
    for name in &d.names {
        let base = name.base().to_owned();
        let rel = match txn.catalog().lookup_relation(name) {
            Err(e) if d.if_exists && e.code == code::INVALID_SCHEMA_NAME => {
                notices.push(Notice { severity: "NOTICE", code: "00000", message: format!("{}, skipping", e.message) });
                continue;
            }
            other => other?,
        };
        let missing = |what: &str| -> SqlResult<()> {
            if d.if_exists { Ok(()) } else { Err(SqlError::new(code::UNDEFINED_TABLE, format!("{what} \"{base}\" does not exist"))) }
        };
        match (d.object_type, rel) {
            (ObjectType::Table, Some(Relation::Table(t))) => {
                let referencing = referencing_fks(txn.catalog(), &t);
                if !referencing.is_empty() {
                    if !d.cascade {
                        let (child, con) = &referencing[0];
                        let child = display_name(txn.catalog(), child);
                        return Err(dependents_error("table", &base, format!("constraint {con} on table {child} depends on table {base}")));
                    }
                    for (child, con) in &referencing {
                        drop_fk(txn, child, con);
                        let child = display_name(txn.catalog(), child);
                        notices.push(Notice {
                            severity: "NOTICE",
                            code: "00000",
                            message: format!("drop cascades to constraint {con} on table {child}"),
                        });
                    }
                }
                txn.catalog_mut().drop_table(&t.qualified());
            }
            (ObjectType::Table, Some(other)) => {
                return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is not a table")).with_hint(format!(
                    "Use DROP {} to remove a {}.",
                    other.kind_name().to_uppercase(),
                    other.kind_name()
                )));
            }
            (ObjectType::View, Some(Relation::View(v))) => {
                txn.catalog_mut().drop_view(&QualName::new(&v.schema, &v.name));
            }
            (ObjectType::View, Some(_)) => return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is not a view"))),
            (ObjectType::Sequence, Some(Relation::Sequence(s))) => {
                txn.catalog_mut().drop_sequence(&QualName::new(&s.schema, &s.name));
            }
            (ObjectType::Sequence, Some(_)) => return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is not a sequence"))),
            (ObjectType::Index, Some(Relation::Index { table, index })) => {
                let t = txn.catalog().table_q(&table).cloned().ok_or_else(|| SqlError::undefined_table(&table.name))?;
                if t.has_constraint(&index.name) {
                    let tname = display_name(txn.catalog(), &table);
                    return Err(dependents_error(
                        "index",
                        &base,
                        format!("constraint {} on table {tname} requires index {}", index.name, index.name),
                    )
                    .with_hint(format!("You can drop constraint {} on table {tname} instead.", index.name)));
                }
                let mut def = (*t).clone();
                def.indexes.retain(|i| i.name != index.name);
                txn.catalog_mut().replace_table(&table, def);
            }
            (ObjectType::Index, Some(_)) => return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{base}\" is not an index"))),
            (_, None) => {
                let what = match d.object_type {
                    ObjectType::Table => "table",
                    ObjectType::View => "view",
                    ObjectType::Index => "index",
                    _ => "sequence",
                };
                missing(what)?;
                notices.push(Notice { severity: "NOTICE", code: "00000", message: format!("{what} \"{base}\" does not exist, skipping") });
            }
            _ => {}
        }
    }
    Ok(QueryResult { tag: format!("DROP {kind}"), notices, ..Default::default() })
}

fn truncate(t: &ir::Truncate, txn: &mut Txn, binder: &Binder) -> SqlResult<QueryResult> {
    let _ = binder;
    let mut names: Vec<QualName> = Vec::new();
    for n in &t.tables {
        names.push(txn.catalog().lookup_table(n)?.qualified());
    }
    // Tables referencing a truncated table must be truncated too (CASCADE adds them).
    let mut i = 0;
    while i < names.len() {
        let target = names[i].clone();
        let referencing: Vec<QualName> = txn
            .catalog()
            .tables()
            .filter(|o| {
                o.constraints.iter().any(|c| matches!(&c.kind, ConstraintKind::ForeignKey { ref_table, .. } if *ref_table == target))
            })
            .map(|o| o.qualified())
            .collect();
        for r in referencing {
            if names.contains(&r) {
                continue;
            }
            if !t.cascade {
                return Err(SqlError::feature_not_supported("cannot truncate a table referenced in a foreign key constraint")
                    .with_detail(format!("Table \"{}\" references \"{}\".", r.name, target.name))
                    .with_hint(format!("Truncate table \"{}\" at the same time, or use TRUNCATE ... CASCADE.", r.name)));
            }
            names.push(r);
        }
        i += 1;
    }
    for name in &names {
        let def = txn.catalog().table_q(name).cloned().ok_or_else(|| SqlError::undefined_table(&name.name))?;
        txn.truncate(def.oid);
        if t.restart_identity {
            let seqs: Vec<pgmem_catalog::SequenceDef> = txn.catalog().owned_sequences(&def).map(|s| (**s).clone()).collect();
            for s in seqs {
                txn.reset_sequence(&s);
            }
        }
    }
    Ok(tag("TRUNCATE TABLE"))
}

/// How an object name prints in messages: unqualified when visible on the search path.
pub(crate) fn display_name(cat: &Catalog, q: &QualName) -> String {
    if cat.relation_visible(&q.schema, &q.name) { q.name.clone() } else { q.to_string() }
}

/// Foreign keys in other tables that reference `t`: (table, constraint name).
fn referencing_fks(cat: &Catalog, t: &TableDef) -> Vec<(QualName, String)> {
    let target = t.qualified();
    cat.tables()
        .filter(|o| o.qualified() != target)
        .flat_map(|o| {
            o.constraints.iter().filter_map(|c| match &c.kind {
                ConstraintKind::ForeignKey { ref_table, .. } if *ref_table == target => Some((o.qualified(), c.name.clone())),
                _ => None,
            })
        })
        .collect()
}

fn drop_fk(txn: &mut Txn, child: &QualName, con: &str) {
    if let Some(ct) = txn.catalog().table_q(child).cloned() {
        let mut def = (*ct).clone();
        def.constraints.retain(|c| c.name != con);
        txn.catalog_mut().replace_table(child, def);
    }
}

pub(crate) fn create_schema(cs: &ir::CreateSchema, txn: &mut Txn, user: &str) -> SqlResult<QueryResult> {
    let name = match (&cs.name, &cs.authorization) {
        (Some(n), _) => n.clone(),
        (None, Some(a)) => a.clone(),
        (None, None) => return Err(SqlError::syntax("syntax error at or near \"SCHEMA\"")),
    };
    if name.starts_with("pg_") {
        return Err(SqlError::new(code::RESERVED_NAME, format!("unacceptable schema name \"{name}\""))
            .with_detail("The prefix \"pg_\" is reserved for system schemas."));
    }
    if txn.catalog().schema_exists(&name) {
        if cs.if_not_exists {
            return Ok(notice("CREATE SCHEMA", format!("schema \"{name}\" already exists, skipping")));
        }
        return Err(SqlError::new(code::DUPLICATE_SCHEMA, format!("schema \"{name}\" already exists")));
    }
    let cat = txn.catalog_mut();
    let oid = cat.alloc_oid();
    let owner = cs.authorization.clone().unwrap_or_else(|| user.to_owned());
    cat.create_schema(pgmem_catalog::SchemaDef { oid, name, owner })?;
    Ok(tag("CREATE SCHEMA"))
}

/// The objects in a schema, as `DROP SCHEMA ... CASCADE` reports them ("table app.items").
fn schema_members(cat: &Catalog, schema: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in cat.tables().filter(|t| t.schema == schema) {
        out.push(format!("table {schema}.{}", t.name));
    }
    for v in cat.views().filter(|v| v.schema == schema) {
        out.push(format!("view {schema}.{}", v.name));
    }
    for s in cat.sequences().filter(|s| s.schema == schema && s.owned_by.is_none()) {
        out.push(format!("sequence {schema}.{}", s.name));
    }
    for t in cat.types().filter(|t| t.schema == schema) {
        out.push(format!("{} {schema}.{}", t.kind_name(), t.name));
    }
    out
}

fn drop_schemas(d: &ir::Drop, txn: &mut Txn) -> SqlResult<QueryResult> {
    let mut notices = Vec::new();
    for name in &d.names {
        let schema = name.base().to_owned();
        if pgmem_catalog::is_system_schema(&schema) || schema == "pg_toast" {
            return Err(SqlError::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("cannot drop schema {schema} because it is required by the database system"),
            ));
        }
        if txn.catalog().schema(&schema).is_none() {
            if d.if_exists {
                notices.push(Notice {
                    severity: "NOTICE",
                    code: "00000",
                    message: format!("schema \"{schema}\" does not exist, skipping"),
                });
                continue;
            }
            return Err(pgmem_catalog::undefined_schema(&schema));
        }
        let members = schema_members(txn.catalog(), &schema);
        if !members.is_empty() {
            if !d.cascade {
                let detail: Vec<String> = members.iter().map(|m| format!("{m} depends on schema {schema}")).collect();
                return Err(dependents_error("schema", &schema, detail.join("\n")));
            }
            drop_schema_contents(txn, &schema);
            notices.push(cascade_notice(&members));
        }
        txn.catalog_mut().drop_schema(&schema);
    }
    Ok(QueryResult { tag: "DROP SCHEMA".into(), notices, ..Default::default() })
}

/// Postgres' "drop cascades to ..." notice: one line, or a count with the list as detail.
fn cascade_notice(members: &[String]) -> Notice {
    let message = if members.len() == 1 {
        format!("drop cascades to {}", members[0])
    } else {
        let lines: Vec<String> = members.iter().map(|m| format!("drop cascades to {m}")).collect();
        format!("drop cascades to {} other objects\n{}", members.len(), lines.join("\n"))
    };
    Notice { severity: "NOTICE", code: "00000", message }
}

/// Drops every object in a schema (foreign keys elsewhere that reference its tables go too).
pub(crate) fn drop_schema_contents(txn: &mut Txn, schema: &str) {
    let tables: Vec<Arc<TableDef>> = txn.catalog().tables().filter(|t| t.schema == schema).cloned().collect();
    for t in &tables {
        for (child, con) in referencing_fks(txn.catalog(), t) {
            if child.schema != schema {
                drop_fk(txn, &child, &con);
            }
        }
    }
    let cat = txn.catalog_mut();
    for t in &tables {
        cat.drop_comments(t.oid);
        cat.drop_table(&t.qualified());
    }
    let views: Vec<QualName> = cat.views().filter(|v| v.schema == schema).map(|v| QualName::new(&v.schema, &v.name)).collect();
    for v in views {
        cat.drop_view(&v);
    }
    let seqs: Vec<QualName> = cat.sequences().filter(|s| s.schema == schema).map(|s| QualName::new(&s.schema, &s.name)).collect();
    for s in seqs {
        cat.drop_sequence(&s);
    }
    let types: Vec<QualName> = cat.types().filter(|t| t.schema == schema).map(|t| QualName::new(&t.schema, &t.name)).collect();
    for t in types {
        cat.drop_type(&t);
    }
}

pub(crate) fn alter_schema(name: &str, rename_to: &str, txn: &mut Txn) -> SqlResult<QueryResult> {
    if txn.catalog().schema(name).is_none() {
        return Err(pgmem_catalog::undefined_schema(name));
    }
    if rename_to.starts_with("pg_") {
        return Err(SqlError::new(code::RESERVED_NAME, format!("unacceptable schema name \"{rename_to}\""))
            .with_detail("The prefix \"pg_\" is reserved for system schemas."));
    }
    txn.catalog_mut().rename_schema(name, rename_to)?;
    // Stored references (serial defaults) name the schema.
    let tables: Vec<TableDef> = txn.catalog().tables().filter(|t| t.schema == rename_to).map(|t| (**t).clone()).collect();
    for mut t in tables {
        for c in &mut t.columns {
            if let Some(ir::Expr::Function(f)) = &mut c.default
                && f.name.base() == "nextval"
                && let Some(ir::Expr::Literal(ir::Literal::String(s))) = f.args.first_mut()
                && let Ok(parts) = crate::reg::parse_qualified(s)
                && parts.len() == 2
                && parts[0] == name
            {
                *s = format!("{}.{}", crate::funcs::string::quote_ident(rename_to), crate::funcs::string::quote_ident(&parts[1]));
            }
        }
        let key = t.qualified();
        txn.catalog_mut().replace_table(&key, t);
    }
    Ok(tag("ALTER SCHEMA"))
}

/// `ALTER ... SET SCHEMA`: moves a relation or type (a table takes its indexes, row type and
/// owned sequences along, as `AlterTableNamespace` does).
fn set_schema(kind: ObjectType, name: &ir::ObjectName, if_exists: bool, schema: &str, txn: &mut Txn) -> SqlResult<QueryResult> {
    let tag_name = match kind {
        ObjectType::Table => "ALTER TABLE",
        ObjectType::View => "ALTER VIEW",
        ObjectType::Sequence => "ALTER SEQUENCE",
        ObjectType::Domain => "ALTER DOMAIN",
        _ => "ALTER TYPE",
    };
    let target = txn.catalog().target_schema(schema, name.base())?;
    if matches!(kind, ObjectType::Type | ObjectType::Domain) {
        let def = txn
            .catalog()
            .lookup_type(name)?
            .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{}\" does not exist", name.base())))?;
        if def.schema == target {
            return Ok(tag(tag_name));
        }
        if txn.catalog().type_by_name(&target, &def.name).is_some() {
            return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("type \"{}\" already exists in schema \"{target}\"", def.name)));
        }
        let mut moved = (*def).clone();
        moved.schema = target;
        txn.catalog_mut().replace_type(&QualName::new(&def.schema, &def.name), moved);
        return Ok(tag(tag_name));
    }
    let Some(rel) = txn.catalog().lookup_relation(name)? else {
        if if_exists {
            return Ok(notice(tag_name, format!("relation \"{}\" does not exist, skipping", name.base())));
        }
        return Err(SqlError::undefined_table(&name.to_string()));
    };
    let old = QualName::new(rel.schema(), rel.name());
    if old.schema == target {
        return Ok(tag(tag_name));
    }
    if txn.catalog().relation(&target, &old.name).is_some() {
        return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{}\" already exists in schema \"{target}\"", old.name)));
    }
    match rel {
        Relation::Table(t) => {
            let owned: Vec<pgmem_catalog::SequenceDef> = txn.catalog().owned_sequences(&t).map(|s| (**s).clone()).collect();
            let mut def = (*t).clone();
            def.schema = target.clone();
            for seq in owned {
                let key = QualName::new(&seq.schema, &seq.name);
                let mut moved = seq.clone();
                moved.schema = target.clone();
                txn.catalog_mut().replace_sequence(&key, moved);
                retarget_nextval(&mut def, &key, &target);
            }
            txn.catalog_mut().replace_table(&old, def);
            // Foreign keys elsewhere follow the table.
            let renamed = QualName::new(&target, &old.name);
            let others: Vec<TableDef> = txn.catalog().tables().map(|t| (**t).clone()).collect();
            for mut o in others {
                let mut changed = false;
                for con in &mut o.constraints {
                    if let ConstraintKind::ForeignKey { ref_table, .. } = &mut con.kind
                        && *ref_table == old
                    {
                        *ref_table = renamed.clone();
                        changed = true;
                    }
                }
                if changed {
                    let key = o.qualified();
                    txn.catalog_mut().replace_table(&key, o);
                }
            }
        }
        Relation::View(v) => {
            let mut def = (*v).clone();
            def.schema = target;
            txn.catalog_mut().drop_view(&old);
            txn.catalog_mut().create_view(def, false)?;
        }
        Relation::Sequence(s) => {
            if s.owned_by.is_some() {
                return Err(SqlError::feature_not_supported("cannot move an owned sequence into another schema"));
            }
            let mut def = (*s).clone();
            def.schema = target;
            txn.catalog_mut().replace_sequence(&old, def);
        }
        Relation::Index { .. } => {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", old.name)));
        }
    }
    Ok(tag(tag_name))
}

/// Points `nextval('schema.seq')` defaults at a sequence's new schema.
fn retarget_nextval(def: &mut TableDef, seq: &QualName, schema: &str) {
    for c in &mut def.columns {
        if let Some(ir::Expr::Function(f)) = &mut c.default
            && f.name.base() == "nextval"
            && let Some(ir::Expr::Literal(ir::Literal::String(s))) = f.args.first_mut()
            && let Ok(parts) = crate::reg::parse_qualified(s)
            && parts.last() == Some(&seq.name)
            && (parts.len() == 1 || parts[0] == seq.schema)
        {
            *s = format!("{}.{}", crate::funcs::string::quote_ident(schema), crate::funcs::string::quote_ident(&seq.name));
        }
    }
}

#[allow(dead_code)]
fn unused(_: Scope) {}
