//! DDL analysis: turns CREATE / ALTER statements into catalog definitions, validating them
//! the way Postgres does (types exist, referenced columns exist, foreign keys target a unique
//! key, expressions bind) and naming constraints with Postgres' defaults.

use super::dml::physical_scope;
use super::{Binder, Clause, Ctx, Level, Scope, resolve_type_name};
use pgmem_catalog::{
    Catalog, ColumnDef, Constraint, ConstraintKind, FkAction, Identity, IndexDef, QualName, SequenceDef, TableDef, ViewDef,
    default_constraint_name, truncate_identifier,
};
use pgmem_ir as ir;
use pgmem_types::{SqlError, SqlResult, Type, code};

fn fk_action(a: ir::ReferentialAction) -> FkAction {
    match a {
        ir::ReferentialAction::NoAction => FkAction::NoAction,
        ir::ReferentialAction::Restrict => FkAction::Restrict,
        ir::ReferentialAction::Cascade => FkAction::Cascade,
        ir::ReferentialAction::SetNull => FkAction::SetNull,
        ir::ReferentialAction::SetDefault => FkAction::SetDefault,
    }
}

/// Serial pseudo-types: (column type, sequence max).
fn serial_type(name: &str) -> Option<(Type, i64)> {
    match name {
        "serial" | "serial4" => Some((Type::Int4, i64::from(i32::MAX))),
        "bigserial" | "serial8" => Some((Type::Int8, i64::MAX)),
        "smallserial" | "serial2" => Some((Type::Int2, i64::from(i16::MAX))),
        _ => None,
    }
}

/// `nextval('schema.seq')`: the sequence is named with its schema so the default keeps
/// working whatever the search path (Postgres stores the OID; the deparser prints the name
/// unqualified when it is visible).
pub fn nextval_default(schema: &str, seq: &str) -> ir::Expr {
    ir::Expr::Function(Box::new(nextval_call(schema, seq)))
}

fn nextval_call(schema: &str, seq: &str) -> ir::FunctionCall {
    let qualified = format!("{}.{}", crate::funcs::string::quote_ident(schema), crate::funcs::string::quote_ident(seq));
    ir::FunctionCall::new("nextval", vec![ir::Expr::Literal(ir::Literal::String(qualified))])
}

/// Picks a constraint/index name not used in the table or its schema (`base`, `base1`, ...).
fn choose_name(catalog: &Catalog, schema: &str, taken: &[String], base: String) -> String {
    catalog.choose_name(schema, &truncate_identifier(base), taken)
}

/// The result of analyzing CREATE TABLE: the table and the sequences it owns.
pub struct CreateTablePlan {
    pub table: TableDef,
    pub sequences: Vec<SequenceDef>,
    /// Column comments copied by `LIKE ... INCLUDING COMMENTS`: (column, text).
    pub comments: Vec<(String, String)>,
}

/// Validates an expression stored in the catalog by binding it against the table row.
fn check_expr_binds(b: &Binder, table: &TableDef, e: &ir::Expr, clause: Clause) -> SqlResult<()> {
    let level = Level::new(physical_scope(table, &table.name));
    let ctx = Ctx::new(&level).with_clause(clause);
    let (bound, _) = b.bind_expr(e, &ctx)?;
    if super::expr::contains_agg(&bound) {
        return Err(super::agg_misuse(clause, "aggregate functions"));
    }
    Ok(())
}

/// Binds `e` against the row of `table` (an expression stored in the catalog) and returns its
/// type; `what` names aggregates in the error (`statistics expressions`).
pub(crate) fn bind_catalog_expr(b: &Binder, table: &TableDef, e: &ir::Expr, what: &str) -> SqlResult<Type> {
    let level = Level::new(physical_scope(table, &table.name));
    let ctx = Ctx::new(&level).with_clause(Clause::Other);
    let (bound, ty) = b.bind_expr(e, &ctx)?;
    if super::expr::contains_agg(&bound) {
        return Err(SqlError::new(code::GROUPING_ERROR, format!("aggregate functions are not allowed in {what}")));
    }
    Ok(ty)
}

fn check_default_binds(b: &Binder, col: &ColumnDef, e: &ir::Expr) -> SqlResult<()> {
    let empty = Level::new(Scope::default());
    let ctx = Ctx::new(&empty).with_clause(Clause::Default);
    let (bound, t) = b.bind_expr(e, &ctx).map_err(|err| {
        if err.code == code::UNDEFINED_COLUMN {
            SqlError::feature_not_supported("cannot use column reference in DEFAULT expression")
        } else {
            err
        }
    })?;
    super::coerce_assignment(b, bound, &t, &col.ty, col.typmod, &col.name)?;
    Ok(())
}

pub fn bind_create_table(b: &Binder, ct: &ir::CreateTable) -> SqlResult<Option<CreateTablePlan>> {
    let catalog = b.catalog;
    let schema = catalog.creation_schema(&ct.name)?;
    let name = ct.name.base().to_owned();
    if catalog.relation(&schema, &name).is_some() {
        if ct.if_not_exists {
            return Ok(None);
        }
        return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{name}\" already exists")));
    }
    let mut table = TableDef {
        oid: 0,
        schema: schema.clone(),
        name: name.clone(),
        columns: Vec::new(),
        constraints: Vec::new(),
        indexes: Vec::new(),
        matview: None,
        unlogged: ct.options.unlogged,
    };
    let mut sequences = Vec::new();
    let mut taken: Vec<String> = vec![name.clone()];
    // Columns.
    for c in &ct.columns {
        if table.columns.iter().any(|x| x.name == c.name) {
            return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{}\" specified more than once", c.name)));
        }
        let mut col = if let Some((ty, _)) = serial_type(&c.ty.name).filter(|_| c.ty.array_dims == 0) {
            let seq = choose_name(catalog, &schema, &taken, default_constraint_name(&name, &[&c.name], "seq"));
            taken.push(seq.clone());
            let mut def = SequenceDef::new(schema.clone(), seq.clone(), ty.clone());
            def.owned_by = Some((name.clone(), c.name.clone()));
            sequences.push(def);
            let mut col = ColumnDef::new(c.name.clone(), ty);
            col.not_null = true;
            col.default = Some(nextval_default(&schema, &seq));
            col
        } else {
            let (ty, typmod) = resolve_type_name(catalog, &c.ty)?;
            let mut col = ColumnDef::new(c.name.clone(), ty);
            col.typmod = typmod;
            col
        };
        for con in &c.constraints {
            match &con.kind {
                ir::ColumnConstraintKind::NotNull => {
                    col.not_null = true;
                    col.not_null_name.clone_from(&con.name);
                }
                ir::ColumnConstraintKind::Null => {}
                ir::ColumnConstraintKind::Default(e) => col.default = Some(e.clone()),
                ir::ColumnConstraintKind::Generated(e) => col.generated = Some(e.clone()),
                ir::ColumnConstraintKind::Collate(n) => col.collation = column_collation(&col.ty, n)?,
                ir::ColumnConstraintKind::Identity { always, options } => {
                    if col.default.is_some() && serial_type(&c.ty.name).is_some() {
                        return Err(SqlError::syntax(format!(
                            "both default and identity specified for column \"{}\" of table \"{name}\"",
                            c.name
                        )));
                    }
                    let seq = identity_sequence(catalog, &schema, &name, &c.name, &col.ty, options, &mut taken)?;
                    let seq_name = seq.name.clone();
                    sequences.push(seq);
                    let seq = seq_name;
                    col.identity = Some(if *always { Identity::Always } else { Identity::ByDefault });
                    col.not_null = true;
                    col.default = Some(nextval_default(&schema, &seq));
                }
                // Key, check and reference constraints are table constraints in disguise.
                _ => {}
            }
        }
        if col.generated.is_some() && col.default.is_some() && col.identity.is_none() {
            return Err(SqlError::syntax(format!(
                "both default and generation expression specified for column \"{}\" of table \"{name}\"",
                c.name
            )));
        }
        table.columns.push(col);
    }
    let likes = super::like::expand(b, ct, &mut table, &mut sequences, &mut taken)?;
    if !ct.options.likes.is_empty() {
        for (i, c) in table.columns.iter().enumerate() {
            if table.columns[..i].iter().any(|x| x.name == c.name) {
                return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{}\" specified more than once", c.name)));
            }
        }
    }
    // Validate expressions now that every column is known.
    for (slot, col) in table.columns.iter().enumerate() {
        if col.generated.is_some() {
            check_generated(b, &table, slot)?;
        }
        if let Some(d) = &col.default
            && col.identity.is_none()
        {
            check_default_binds(b, col, d)?;
        }
    }
    // Constraints, column-level first (in column order), then table-level.
    let mut pending: Vec<(Option<String>, ir::TableConstraintKind, bool, ir::ConstraintTiming)> = Vec::new();
    for c in &ct.columns {
        for con in &c.constraints {
            let kind = match &con.kind {
                ir::ColumnConstraintKind::PrimaryKey => ir::TableConstraintKind::PrimaryKey(vec![c.name.clone()]),
                ir::ColumnConstraintKind::Unique { nulls_not_distinct } => {
                    ir::TableConstraintKind::Unique { columns: vec![c.name.clone()], nulls_not_distinct: *nulls_not_distinct }
                }
                ir::ColumnConstraintKind::Check(e) => ir::TableConstraintKind::Check(e.clone()),
                ir::ColumnConstraintKind::References(r) => {
                    ir::TableConstraintKind::ForeignKey { columns: vec![c.name.clone()], references: r.clone() }
                }
                _ => continue,
            };
            pending.push((con.name.clone(), kind, true, con.timing));
        }
    }
    for con in &ct.constraints {
        pending.push((con.name.clone(), con.kind.clone(), false, con.timing));
    }
    for (name, kind, timing) in likes.constraints {
        pending.push((name, kind, false, timing));
    }
    for (cname, kind, column_level, timing) in pending {
        add_constraint(b, &mut table, cname, &kind, column_level, &mut taken, timing)?;
    }
    for li in &likes.indexes {
        let idx = super::like::build_index(b, &table, li, &mut taken)?;
        table.indexes.push(idx);
    }
    Ok(Some(CreateTablePlan { table, sequences, comments: likes.comments }))
}

/// Functions whose result is not immutable: a generation expression may not call them.
const NOT_IMMUTABLE: &[&str] = &[
    "now",
    "current_timestamp",
    "current_date",
    "current_time",
    "localtime",
    "localtimestamp",
    "clock_timestamp",
    "statement_timestamp",
    "transaction_timestamp",
    "timeofday",
    "random",
    "random_normal",
    "gen_random_uuid",
    "uuidv4",
    "uuidv7",
    "nextval",
    "currval",
    "lastval",
    "setval",
    "current_setting",
    "current_user",
    "session_user",
    "current_schema",
    "txid_current",
    "pg_backend_pid",
];

/// A generation expression's rules: it binds against the row, reads no other generated
/// column and is immutable.
fn check_generated(b: &Binder, table: &TableDef, slot: usize) -> SqlResult<()> {
    let col = &table.columns[slot];
    let Some(g) = &col.generated else { return Ok(()) };
    check_expr_binds(b, table, g, Clause::Check)?;
    for other in &table.columns {
        if other.generated.is_some() && !other.dropped && other.name != col.name && expr_mentions(g, &other.name) {
            return Err(SqlError::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("cannot use generated column \"{}\" in column generation expression", other.name),
            )
            .with_detail("A generated column cannot reference another generated column."));
        }
    }
    let mut volatile = false;
    g.walk(&mut |e| {
        if let ir::Expr::Function(f) = e
            && NOT_IMMUTABLE.contains(&f.name.base())
        {
            volatile = true;
        }
    });
    if volatile {
        return Err(SqlError::new(code::INVALID_OBJECT_DEFINITION, "generation expression is not immutable"));
    }
    Ok(())
}

fn expr_mentions(e: &ir::Expr, column: &str) -> bool {
    e.referenced_columns().iter().any(|c| c == column)
}

/// Adds one constraint (and its backing index) to `table`. `rows` lets ALTER TABLE validate
/// existing data.
pub fn add_constraint(
    b: &Binder,
    table: &mut TableDef,
    cname: Option<String>,
    kind: &ir::TableConstraintKind,
    column_level: bool,
    taken: &mut Vec<String>,
    timing: ir::ConstraintTiming,
) -> SqlResult<()> {
    let catalog = b.catalog;
    let tname = table.name.clone();
    let schema = table.schema.clone();
    let col_index = |t: &TableDef, n: &str| -> SqlResult<usize> {
        t.column_index(n).ok_or_else(|| SqlError::undefined_column(&format!("\"{n}\" named in key does not exist")))
    };
    let mut used: Vec<String> = taken.clone();
    used.extend(table.constraints.iter().map(|c| c.name.clone()));
    used.extend(table.indexes.iter().map(|i| i.name.clone()));
    // A key constraint's index is a relation: its name clashes with any relation first.
    if let Some(n) = &cname
        && matches!(kind, ir::TableConstraintKind::PrimaryKey(_) | ir::TableConstraintKind::Unique { .. })
        && (table.indexes.iter().any(|i| &i.name == n) || catalog.relation(&schema, n).is_some())
    {
        return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{n}\" already exists")));
    }
    if let Some(n) = &cname
        && table.has_constraint(n)
    {
        return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("constraint \"{n}\" for relation \"{tname}\" already exists")));
    }
    match kind {
        ir::TableConstraintKind::Exclude { method, elements, predicate } => {
            let method = method.clone().unwrap_or_else(|| "btree".into());
            add_exclusion(b, table, cname, &method, elements.clone(), predicate.clone(), None, taken, &used, timing)?;
        }
        ir::TableConstraintKind::Temporal { primary, columns } => {
            let n = columns.len();
            let elements: Vec<(ir::Expr, String)> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| (ir::Expr::Column(vec![c.clone()]), if i + 1 == n { "&&" } else { "=" }.to_owned()))
                .collect();
            for c in columns {
                col_index(table, c)?;
            }
            if *primary {
                if table.primary_key().is_some() || table.constraints.iter().any(|c| c.contype() == 'p') {
                    return Err(SqlError::new(
                        code::INVALID_TABLE_DEFINITION,
                        format!("multiple primary keys for table \"{tname}\" are not allowed"),
                    ));
                }
                for c in columns {
                    let slot = col_index(table, c)?;
                    table.columns[slot].not_null = true;
                }
            }
            add_exclusion(b, table, cname, "gist", elements, None, Some(*primary), taken, &used, timing)?;
        }
        ir::TableConstraintKind::UsingIndex { primary, index } => {
            let pos = table
                .indexes
                .iter()
                .position(|i| &i.name == index)
                .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("index \"{index}\" does not exist")))?;
            if table.constraints.iter().any(|c| &c.name == index) {
                return Err(SqlError::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("index \"{index}\" is already associated with a constraint"),
                ));
            }
            let idx = &table.indexes[pos];
            let unusable = if !idx.unique {
                Some(format!("\"{index}\" is not a unique index"))
            } else if idx.predicate.is_some() {
                Some(format!("\"{index}\" is a partial index"))
            } else if !idx.exprs.is_empty() {
                Some(format!("index \"{index}\" contains expressions"))
            } else {
                None
            };
            if let Some(msg) = unusable {
                return Err(SqlError::new(code::WRONG_OBJECT_TYPE, msg)
                    .with_detail("Cannot create a primary key or unique constraint using such an index."));
            }
            if *primary && table.primary_key().is_some() {
                return Err(SqlError::new(
                    code::INVALID_TABLE_DEFINITION,
                    format!("multiple primary keys for table \"{tname}\" are not allowed"),
                ));
            }
            let name = cname.unwrap_or_else(|| index.clone());
            let columns = idx.columns.clone();
            let nulls_not_distinct = idx.nulls_not_distinct;
            if *primary {
                for &i in &columns {
                    table.columns[i].not_null = true;
                }
            }
            table.indexes[pos].name.clone_from(&name);
            table.indexes[pos].primary = *primary;
            taken.push(name.clone());
            let ckind =
                if *primary { ConstraintKind::PrimaryKey { columns } } else { ConstraintKind::Unique { columns, nulls_not_distinct } };
            table.constraints.push(Constraint {
                oid: 0,
                name,
                kind: ckind,
                deferrable: timing.deferrable,
                initially_deferred: timing.initially_deferred,
                validated: true,
            });
        }
        ir::TableConstraintKind::PrimaryKey(cols) | ir::TableConstraintKind::Unique { columns: cols, .. } => {
            let primary = matches!(kind, ir::TableConstraintKind::PrimaryKey(_));
            if primary && table.primary_key().is_some() {
                return Err(SqlError::new(
                    code::INVALID_TABLE_DEFINITION,
                    format!("multiple primary keys for table \"{tname}\" are not allowed"),
                ));
            }
            let idx_cols = cols.iter().map(|c| col_index(table, c)).collect::<SqlResult<Vec<_>>>()?;
            let nulls_not_distinct = matches!(kind, ir::TableConstraintKind::Unique { nulls_not_distinct: true, .. });
            // Identical unique constraints are merged, as Postgres does.
            if !primary
                && table.indexes.iter().any(|i| i.unique && i.exprs.is_empty() && i.columns == idx_cols && i.predicate.is_none())
                && cname.is_none()
            {
                return Ok(());
            }
            if primary {
                for &i in &idx_cols {
                    table.columns[i].not_null = true;
                }
            }
            let names: Vec<&str> = cols.iter().map(String::as_str).collect();
            let name = cname.unwrap_or_else(|| {
                choose_name(
                    catalog,
                    &schema,
                    &used,
                    if primary { default_constraint_name(&tname, &[], "pkey") } else { default_constraint_name(&tname, &names, "key") },
                )
            });
            taken.push(name.clone());
            table.indexes.push(IndexDef {
                oid: 0,
                name: name.clone(),
                columns: idx_cols.clone(),
                exprs: Vec::new(),
                unique: true,
                primary,
                nulls_not_distinct,
                predicate: None,
            });
            let ckind = if primary {
                ConstraintKind::PrimaryKey { columns: idx_cols }
            } else {
                ConstraintKind::Unique { columns: idx_cols, nulls_not_distinct }
            };
            table.constraints.push(Constraint {
                oid: 0,
                name,
                kind: ckind,
                deferrable: timing.deferrable,
                initially_deferred: timing.initially_deferred,
                validated: true,
            });
        }
        ir::TableConstraintKind::Check(e) => {
            check_expr_binds(b, table, e, Clause::Check)?;
            // Postgres names a CHECK after its column when the expression references exactly one
            // column of the table, wherever the constraint was written.
            let _ = column_level;
            let cols: Vec<String> = e.referenced_columns().into_iter().filter(|c| table.column_index(c).is_some()).collect();
            let base = match cols.as_slice() {
                [only] => default_constraint_name(&tname, &[only], "check"),
                _ => default_constraint_name(&tname, &[], "check"),
            };
            let name = cname.unwrap_or_else(|| choose_name(catalog, &schema, &used, base));
            taken.push(name.clone());
            table.constraints.push(Constraint {
                oid: 0,
                name,
                kind: ConstraintKind::Check { expr: e.clone() },
                deferrable: false,
                initially_deferred: false,
                validated: !timing.not_valid,
            });
        }
        ir::TableConstraintKind::ForeignKey { columns, references } => {
            let local = columns.iter().map(|c| col_index(table, c)).collect::<SqlResult<Vec<_>>>()?;
            // A reference to the table itself (which isn't in the catalog yet when creating).
            let self_ref = references.table.base() == tname
                && match references.table.0.as_slice() {
                    [_] => catalog.lookup_relation(&references.table)?.is_none_or(|r| r.oid() == table.oid && table.oid != 0),
                    [s, _] | [_, s, _] => catalog.resolve_schema_name(s) == schema,
                    _ => false,
                };
            let parent: TableDef = if self_ref { table.clone() } else { (*catalog.lookup_table(&references.table)?).clone() };
            let ref_name = parent.name.clone();
            let ref_q = QualName::new(&parent.schema, &parent.name);
            let ref_cols = if references.columns.is_empty() {
                parent.primary_key().map(<[usize]>::to_vec).ok_or_else(|| {
                    SqlError::new(code::INVALID_FOREIGN_KEY, format!("there is no primary key for referenced table \"{ref_name}\""))
                })?
            } else {
                references
                    .columns
                    .iter()
                    .map(|c| {
                        parent.column_index(c).ok_or_else(|| {
                            SqlError::undefined_column(&format!("\"{c}\" referenced in foreign key constraint does not exist"))
                        })
                    })
                    .collect::<SqlResult<Vec<_>>>()?
            };
            let mut sorted = ref_cols.clone();
            sorted.sort_unstable();
            let has_unique = parent.indexes.iter().any(|i| {
                let mut c = i.columns.clone();
                c.sort_unstable();
                i.unique && i.exprs.is_empty() && i.predicate.is_none() && c == sorted
            });
            if !has_unique {
                return Err(SqlError::new(
                    code::INVALID_FOREIGN_KEY,
                    format!("there is no unique constraint matching given keys for referenced table \"{ref_name}\""),
                ));
            }
            if ref_cols.len() != local.len() {
                return Err(SqlError::new(
                    code::INVALID_FOREIGN_KEY,
                    "number of referencing and referenced columns for foreign key disagree",
                ));
            }
            let names: Vec<&str> = columns.iter().map(String::as_str).collect();
            let name = cname.unwrap_or_else(|| choose_name(catalog, &schema, &used, default_constraint_name(&tname, &names, "fkey")));
            for (l, r) in local.iter().zip(&ref_cols) {
                let (lt, rt) = (&table.columns[*l].ty, &parent.columns[*r].ty);
                if crate::funcs::common_type(lt, rt).is_none() {
                    return Err(SqlError::datatype_mismatch(format!("foreign key constraint \"{name}\" cannot be implemented")).with_detail(format!(
                        "Key columns \"{}\" of the referencing table and \"{}\" of the referenced table are of incompatible types: {} and {}.",
                        table.columns[*l].name,
                        parent.columns[*r].name,
                        lt.name(),
                        rt.name()
                    )));
                }
            }
            let delete_set_columns = references
                .delete_set_columns
                .iter()
                .map(|c| {
                    table.column_index(c).filter(|i| local.contains(i)).ok_or_else(|| {
                        SqlError::new(
                            code::INVALID_COLUMN_REFERENCE,
                            format!("column \"{c}\" referenced in ON DELETE SET action must be part of foreign key"),
                        )
                    })
                })
                .collect::<SqlResult<Vec<_>>>()?;
            taken.push(name.clone());
            table.constraints.push(Constraint {
                oid: 0,
                name,
                kind: ConstraintKind::ForeignKey {
                    columns: local,
                    ref_table: ref_q,
                    ref_columns: ref_cols,
                    on_delete: fk_action(references.on_delete),
                    on_update: fk_action(references.on_update),
                    match_full: references.match_full,
                    delete_set_columns,
                },
                deferrable: timing.deferrable,
                initially_deferred: timing.initially_deferred,
                validated: !timing.not_valid,
            });
        }
    }
    Ok(())
}

pub fn bind_create_view(b: &Binder, cv: &ir::CreateView) -> SqlResult<ViewDef> {
    let schema = b.catalog.creation_schema(&cv.name)?;
    let name = cv.name.base().to_owned();
    let top = Level::new(Scope::default());
    let ctx = Ctx::new(&top);
    let bound = b.bind_query(&cv.query, &ctx)?;
    let mut columns: Vec<String> = bound.columns.iter().map(|c| c.name.clone()).collect();
    if cv.columns.len() > columns.len() {
        return Err(SqlError::syntax("CREATE VIEW specifies more column names than columns"));
    }
    for (c, n) in columns.iter_mut().zip(&cv.columns) {
        *c = n.clone();
    }
    if b.params.borrow().iter().any(|_| true) {
        return Err(SqlError::new(code::UNDEFINED_PARAMETER, "there is no parameter $1"));
    }
    let search_path = b.catalog.resolved_search_path();
    Ok(ViewDef { oid: 0, schema, name, columns, query: (*cv.query).clone(), search_path, check_option: cv.check_option })
}

pub fn bind_create_index(b: &Binder, ci: &ir::CreateIndex) -> SqlResult<Option<(QualName, IndexDef)>> {
    let catalog = b.catalog;
    let table = catalog.lookup_table(&ci.table)?;
    let tname = table.name.clone();
    let mut columns = Vec::new();
    let mut exprs = Vec::new();
    let mut names = Vec::new();
    for key in &ci.columns {
        match &key.expr {
            ir::Expr::Column(parts) if parts.len() == 1 => {
                let idx = table.column_index(&parts[0]).ok_or_else(|| SqlError::undefined_column(&format!("\"{}\"", parts[0])))?;
                columns.push(idx);
                names.push(parts[0].clone());
            }
            e => {
                check_expr_binds(b, &table, e, Clause::Other)?;
                exprs.push(e.clone());
                names.push(super::like::index_key_name(e));
            }
        }
    }
    if !exprs.is_empty() {
        // Mixed column/expression keys: treat all keys as expressions.
        exprs = ci.columns.iter().map(|k| k.expr.clone()).collect();
        columns.clear();
    }
    if let Some(p) = &ci.predicate {
        check_expr_binds(b, &table, p, Clause::Where)?;
    }
    if let Some(m) = &ci.method {
        if !["btree", "hash", "gist", "gin", "spgist", "brin"].contains(&m.as_str()) {
            return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("access method \"{m}\" does not exist")));
        }
        if ci.unique && m != "btree" {
            return Err(SqlError::feature_not_supported(format!("access method \"{m}\" does not support unique indexes")));
        }
    }
    let name = match &ci.name {
        Some(n) => {
            if catalog.relation(&table.schema, n).is_some() {
                if ci.if_not_exists {
                    return Ok(None);
                }
                return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{n}\" already exists")));
            }
            n.clone()
        }
        None => {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            catalog.choose_name(&table.schema, &default_constraint_name(&tname, &refs, "idx"), &[])
        }
    };
    Ok(Some((
        table.qualified(),
        IndexDef {
            oid: 0,
            name,
            columns,
            exprs,
            unique: ci.unique,
            primary: false,
            nulls_not_distinct: ci.nulls_not_distinct,
            predicate: ci.predicate.clone(),
        },
    )))
}

pub fn bind_create_sequence(b: &Binder, cs: &ir::CreateSequence) -> SqlResult<Option<SequenceDef>> {
    let schema = b.catalog.creation_schema(&cs.name)?;
    let name = cs.name.base().to_owned();
    if b.catalog.relation(&schema, &name).is_some() {
        if cs.if_not_exists {
            return Ok(None);
        }
        return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{name}\" already exists")));
    }
    let mut seq = SequenceDef::new(schema.clone(), name, Type::Int8);
    crate::sequences::apply_options(b.catalog, &mut seq, &cs.options, true)?;
    if let Some(Some(owner)) = &cs.options.owned_by {
        seq.owned_by = Some(crate::sequences::resolve_owner(b.catalog, &schema, owner)?);
    }
    Ok(Some(seq))
}

/// The sequence behind identity column `column` of `table` (of type `ty`), named like
/// Postgres does (`table_column_seq`).
pub fn identity_sequence(
    catalog: &Catalog,
    schema: &str,
    table: &str,
    column: &str,
    ty: &Type,
    options: &ir::SequenceOptions,
    taken: &mut Vec<String>,
) -> SqlResult<SequenceDef> {
    if !matches!(ty, Type::Int2 | Type::Int4 | Type::Int8) {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "identity column type must be smallint, integer, or bigint"));
    }
    let name = choose_name(catalog, schema, taken, default_constraint_name(table, &[column], "seq"));
    taken.push(name.clone());
    let mut seq = SequenceDef::new(schema, name, ty.clone());
    crate::sequences::apply_options(catalog, &mut seq, options, true)?;
    seq.owned_by = Some((table.to_owned(), column.to_owned()));
    Ok(seq)
}

/// Result of analyzing one ALTER TABLE action.
pub enum AlterOutcome {
    /// Replace the table definition; optionally append a slot initialized to the value per
    /// existing row, or check existing rows against new rules.
    Replace {
        def: Box<TableDef>,
        rename_from: Option<QualName>,
        sequences: Vec<SequenceDef>,
        validate: Validation,
    },
    Nothing,
}

/// Data checks ALTER TABLE must run against existing rows.
pub enum Validation {
    None,
    /// Every row must be non-NULL in this slot.
    NotNull(usize),
    /// Every row must satisfy the named CHECK / be unique / have parents.
    Constraint(String),
    /// ALTER COLUMN TYPE: convert this slot.
    ConvertColumn {
        slot: usize,
        using: Option<ir::Expr>,
    },
}

pub fn bind_alter_table(b: &Binder, table: &TableDef, action: &ir::AlterTableAction, has_rows: bool) -> SqlResult<AlterOutcome> {
    let tname = table.name.clone();
    let mut def = table.clone();
    let not_found = |c: &str| SqlError::undefined_column(&format!("\"{c}\" of relation \"{tname}\""));
    match action {
        ir::AlterTableAction::AttachPartition { .. } | ir::AlterTableAction::DetachPartition { .. } => {
            Err(SqlError::internal("ATTACH / DETACH PARTITION are applied by the executor"))
        }
        ir::AlterTableAction::AddColumn { if_not_exists, column } => {
            if def.column_index(&column.name).is_some() {
                if *if_not_exists {
                    return Ok(AlterOutcome::Nothing);
                }
                return Err(SqlError::new(
                    code::DUPLICATE_COLUMN,
                    format!("column \"{}\" of relation \"{tname}\" already exists", column.name),
                ));
            }
            // Analyze the column through a one-column CREATE TABLE to reuse all the rules (a
            // generation expression is checked against the whole table afterwards).
            let mut probe_column = column.clone();
            let generated = probe_column.constraints.iter().find_map(|c| match &c.kind {
                ir::ColumnConstraintKind::Generated(e) => Some(e.clone()),
                _ => None,
            });
            probe_column.constraints.retain(|c| !matches!(c.kind, ir::ColumnConstraintKind::Generated(_)));
            let ct = ir::CreateTable {
                name: ir::ObjectName(vec![table.schema.clone(), format!("{tname}__pgmem_add")]),
                if_not_exists: false,
                temporary: false,
                columns: vec![probe_column],
                constraints: vec![],
                as_query: None,
                options: ir::CreateTableOptions::default(),
            };
            let probe = bind_create_table(b, &ct)?.ok_or_else(|| SqlError::internal("ALTER TABLE ADD COLUMN analysis"))?;
            let mut col = probe.table.columns.into_iter().next().ok_or_else(|| SqlError::internal("no column"))?;
            if generated.is_some() && col.default.is_some() {
                return Err(SqlError::syntax(format!(
                    "both default and generation expression specified for column \"{}\" of table \"{tname}\"",
                    col.name
                )));
            }
            col.generated.clone_from(&generated);
            let mut sequences = probe.sequences;
            for s in &mut sequences {
                s.name = s.name.replacen(&format!("{tname}__pgmem_add"), &tname, 1);
                s.owned_by = Some((tname.clone(), col.name.clone()));
                if let Some(ir::Expr::Function(f)) = &mut col.default
                    && f.name.base() == "nextval"
                {
                    **f = nextval_call(&s.schema, &s.name);
                }
            }
            // Existing rows read the new column as its default when the default is a constant
            // (Postgres' fast default); volatile defaults force a rewrite.
            let needs_rewrite = col.default.as_ref().is_some_and(|d| !is_constant_expr(d)) || col.generated.is_some();
            if !needs_rewrite && let Some(d) = &col.default {
                let empty = Level::new(Scope::default());
                let ctx = Ctx::new(&empty);
                let (e, t) = b.bind_expr(d, &ctx)?;
                let e = super::coerce_assignment(b, e, &t, &col.ty, col.typmod, &col.name)?;
                if let crate::plan::Expr::Const(v) = e {
                    col.missing = v;
                }
            }
            if col.not_null && col.missing.is_null() && has_rows && !needs_rewrite {
                return Err(SqlError::new(
                    code::NOT_NULL_VIOLATION,
                    format!("column \"{}\" of relation \"{tname}\" contains null values", col.name),
                ));
            }
            def.columns.push(col);
            let slot = def.columns.len() - 1;
            check_generated(b, &def, slot)?;
            let mut taken = Vec::new();
            for con in &column.constraints {
                let kind = match &con.kind {
                    ir::ColumnConstraintKind::PrimaryKey => ir::TableConstraintKind::PrimaryKey(vec![column.name.clone()]),
                    ir::ColumnConstraintKind::Unique { nulls_not_distinct } => {
                        ir::TableConstraintKind::Unique { columns: vec![column.name.clone()], nulls_not_distinct: *nulls_not_distinct }
                    }
                    ir::ColumnConstraintKind::Check(e) => ir::TableConstraintKind::Check(e.clone()),
                    ir::ColumnConstraintKind::References(r) => {
                        ir::TableConstraintKind::ForeignKey { columns: vec![column.name.clone()], references: r.clone() }
                    }
                    _ => continue,
                };
                add_constraint(b, &mut def, con.name.clone(), &kind, true, &mut taken, con.timing)?;
            }
            let validate = if needs_rewrite { Validation::ConvertColumn { slot, using: generated } } else { Validation::None };
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences, validate })
        }
        ir::AlterTableAction::DropColumn { name, if_exists, cascade } => {
            let Some(slot) = def.column_index(name) else {
                if *if_exists {
                    return Ok(AlterOutcome::Nothing);
                }
                return Err(not_found(name));
            };
            // Constraints and indexes over the column go with it (Postgres requires CASCADE
            // only for objects in other tables, e.g. views; those are not tracked here).
            let _ = cascade;
            def.constraints.retain(|c| !constraint_uses(c, slot, name));
            def.indexes.retain(|i| !i.columns.contains(&slot));
            let col = &mut def.columns[slot];
            col.dropped = true;
            col.name = format!("........pg.dropped.{}........", slot + 1);
            col.not_null = false;
            col.default = None;
            col.generated = None;
            col.identity = None;
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
        ir::AlterTableAction::RenameColumn { old, new } => {
            let slot = def.column_index(old).ok_or_else(|| not_found(old))?;
            if def.column_index(new).is_some() {
                return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{new}\" of relation \"{tname}\" already exists")));
            }
            def.columns[slot].name = new.clone();
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
        ir::AlterTableAction::AddIdentity { .. }
        | ir::AlterTableAction::SetIdentity { .. }
        | ir::AlterTableAction::DropIdentity { .. }
        | ir::AlterTableAction::EnableTrigger { .. }
        | ir::AlterTableAction::OwnerTo(_) => Err(SqlError::internal("identity, trigger and owner actions run in ddl::alter_table_action")),
        ir::AlterTableAction::SetExpression { column, expr } => {
            let slot = def.column_index(column).ok_or_else(|| not_found(column))?;
            if def.columns[slot].generated.is_none() {
                return Err(SqlError::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("column \"{column}\" of relation \"{tname}\" is not a generated column"),
                ));
            }
            def.columns[slot].generated = Some(expr.clone());
            check_generated(b, &def, slot)?;
            Ok(AlterOutcome::Replace {
                def: Box::new(def),
                rename_from: None,
                sequences: vec![],
                validate: Validation::ConvertColumn { slot, using: Some(expr.clone()) },
            })
        }
        ir::AlterTableAction::DropExpression { column, if_exists } => {
            let slot = def.column_index(column).ok_or_else(|| not_found(column))?;
            if def.columns[slot].generated.is_none() {
                if *if_exists {
                    return Ok(AlterOutcome::Nothing);
                }
                return Err(SqlError::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("column \"{column}\" of relation \"{tname}\" is not a generated column"),
                ));
            }
            def.columns[slot].generated = None;
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
        ir::AlterTableAction::RenameConstraint { old, new } => {
            let nn_name =
                |c: &ColumnDef| c.not_null_name.clone().unwrap_or_else(|| crate::catalog_views::not_null_constraint_name(&tname, &c.name));
            let exists = |n: &str| def.has_constraint(n) || def.live_columns().any(|(_, c)| c.not_null && nn_name(c) == n);
            if !exists(old) {
                return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("constraint \"{old}\" for table \"{tname}\" does not exist")));
            }
            if exists(new) {
                return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("constraint \"{new}\" for relation \"{tname}\" already exists")));
            }
            if let Some(c) = def.constraints.iter_mut().find(|c| &c.name == old) {
                c.name.clone_from(new);
                if let Some(i) = def.indexes.iter_mut().find(|i| &i.name == old) {
                    i.name.clone_from(new);
                }
            } else if let Some(c) = def.columns.iter_mut().find(|c| !c.dropped && c.not_null && nn_name(c) == *old) {
                c.not_null_name = Some(new.clone());
            }
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
        ir::AlterTableAction::RenameTable { new } => {
            if b.catalog.relation(&table.schema, new).is_some() {
                return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{new}\" already exists")));
            }
            def.name = new.clone();
            Ok(AlterOutcome::Replace {
                def: Box::new(def),
                rename_from: Some(table.qualified()),
                sequences: vec![],
                validate: Validation::None,
            })
        }
        ir::AlterTableAction::SetNotNull { column } => {
            let slot = def.column_index(column).ok_or_else(|| not_found(column))?;
            def.columns[slot].not_null = true;
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::NotNull(slot) })
        }
        ir::AlterTableAction::DropNotNull { column } => {
            let slot = def.column_index(column).ok_or_else(|| not_found(column))?;
            if def.primary_key().is_some_and(|pk| pk.contains(&slot)) {
                return Err(SqlError::new(code::INVALID_TABLE_DEFINITION, format!("column \"{column}\" is in a primary key")));
            }
            if def.columns[slot].identity.is_some() {
                return Err(SqlError::syntax(format!("column \"{column}\" of relation \"{tname}\" is an identity column")));
            }
            def.columns[slot].not_null = false;
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
        ir::AlterTableAction::SetDefault { column, expr } => {
            let slot = def.column_index(column).ok_or_else(|| not_found(column))?;
            check_default_binds(b, &def.columns[slot], expr)?;
            def.columns[slot].default = Some(expr.clone());
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
        ir::AlterTableAction::DropDefault { column } => {
            let slot = def.column_index(column).ok_or_else(|| not_found(column))?;
            def.columns[slot].default = None;
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
        ir::AlterTableAction::AlterType { column, ty, using } => {
            let slot = def.column_index(column).ok_or_else(|| not_found(column))?;
            let (t, typmod) = resolve_type_name(b.catalog, ty)?;
            let old = &def.columns[slot].ty;
            if using.is_none() && matches!(crate::cast::cast_context(old, &t), None | Some(crate::cast::CoercionContext::Explicit)) {
                return Err(SqlError::datatype_mismatch(format!("column \"{column}\" cannot be cast automatically to type {}", t.name()))
                    .with_hint(format!("You might need to specify \"USING {column}::{}\".", t.name())));
            }
            def.columns[slot].ty = t;
            def.columns[slot].typmod = typmod;
            Ok(AlterOutcome::Replace {
                def: Box::new(def),
                rename_from: None,
                sequences: vec![],
                validate: Validation::ConvertColumn { slot, using: using.clone() },
            })
        }
        ir::AlterTableAction::AddConstraint(con) => {
            if con.timing.not_valid {
                let what = match con.kind {
                    ir::TableConstraintKind::PrimaryKey(_) => Some("PRIMARY KEY"),
                    ir::TableConstraintKind::Unique { .. } => Some("UNIQUE"),
                    _ => None,
                };
                if let Some(what) = what {
                    return Err(SqlError::feature_not_supported(format!("{what} constraints cannot be marked NOT VALID")));
                }
            }
            let mut taken = Vec::new();
            let before = def.constraints.len();
            add_constraint(b, &mut def, con.name.clone(), &con.kind, false, &mut taken, con.timing)?;
            // NOT VALID skips the existing rows (new ones are checked).
            let validate = match def.constraints.get(before) {
                Some(c) if !con.timing.not_valid => Validation::Constraint(c.name.clone()),
                _ => Validation::None,
            };
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate })
        }
        ir::AlterTableAction::ValidateConstraint { name } => {
            let con = def.constraints.iter_mut().find(|c| &c.name == name).ok_or_else(|| {
                SqlError::new(code::UNDEFINED_OBJECT, format!("constraint \"{name}\" of relation \"{tname}\" does not exist"))
            })?;
            if con.validated {
                return Ok(AlterOutcome::Nothing);
            }
            con.validated = true;
            Ok(AlterOutcome::Replace {
                def: Box::new(def),
                rename_from: None,
                sequences: vec![],
                validate: Validation::Constraint(name.clone()),
            })
        }
        ir::AlterTableAction::AlterConstraint { name, timing } => {
            let con = def.constraints.iter_mut().find(|c| &c.name == name).ok_or_else(|| {
                SqlError::new(code::UNDEFINED_OBJECT, format!("constraint \"{name}\" of relation \"{tname}\" does not exist"))
            })?;
            if !matches!(con.kind, ConstraintKind::ForeignKey { .. }) {
                return Err(SqlError::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("constraint \"{name}\" of relation \"{tname}\" is not a foreign key constraint"),
                ));
            }
            if timing.initially_deferred && !timing.deferrable {
                return Err(SqlError::syntax("constraint declared INITIALLY DEFERRED must be DEFERRABLE"));
            }
            con.deferrable = timing.deferrable;
            con.initially_deferred = timing.initially_deferred;
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
        ir::AlterTableAction::DropConstraint { name, if_exists, .. } => {
            // A NOT NULL constraint (Postgres 18 names them).
            let not_null = def.columns.iter().position(|c| {
                c.not_null
                    && !c.dropped
                    && c.not_null_name.clone().unwrap_or_else(|| crate::catalog_views::not_null_constraint_name(&tname, &c.name)) == *name
            });
            if let Some(slot) = not_null
                && !def.has_constraint(name)
            {
                if def.primary_key().is_some_and(|pk| pk.contains(&slot)) {
                    return Err(SqlError::new(
                        code::INVALID_TABLE_DEFINITION,
                        format!("column \"{}\" is in a primary key", def.columns[slot].name),
                    ));
                }
                def.columns[slot].not_null = false;
                def.columns[slot].not_null_name = None;
                return Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None });
            }
            if !def.has_constraint(name) {
                if *if_exists {
                    return Ok(AlterOutcome::Nothing);
                }
                return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("constraint \"{name}\" of relation \"{tname}\" does not exist")));
            }
            def.constraints.retain(|c| &c.name != name);
            def.indexes.retain(|i| &i.name != name);
            Ok(AlterOutcome::Replace { def: Box::new(def), rename_from: None, sequences: vec![], validate: Validation::None })
        }
    }
}

fn constraint_uses(c: &Constraint, slot: usize, name: &str) -> bool {
    match &c.kind {
        ConstraintKind::PrimaryKey { columns } | ConstraintKind::Unique { columns, .. } | ConstraintKind::ForeignKey { columns, .. } => {
            columns.contains(&slot)
        }
        ConstraintKind::Check { .. } => false,
        ConstraintKind::Exclude { elements, predicate, .. } => {
            elements.iter().map(|(e, _)| e).chain(predicate.iter()).any(|e| expr_mentions(e, name))
        }
    }
}

/// Adds an exclusion constraint (and the index Postgres builds for it) to `table`, after
/// checking the access method has an operator class for every key and the operators exist.
#[allow(clippy::too_many_arguments)]
fn add_exclusion(
    b: &Binder,
    table: &mut TableDef,
    cname: Option<String>,
    method: &str,
    elements: Vec<(ir::Expr, String)>,
    predicate: Option<ir::Expr>,
    temporal: Option<bool>,
    taken: &mut Vec<String>,
    used: &[String],
    timing: ir::ConstraintTiming,
) -> SqlResult<()> {
    if !["btree", "hash", "gist", "spgist", "gin", "brin"].contains(&method) {
        return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("access method \"{method}\" does not exist")));
    }
    let level = Level::new(physical_scope(table, &table.name));
    let ctx = Ctx::new(&level).with_clause(Clause::Check);
    for (e, op) in &elements {
        let (_, ty) = b.bind_expr(e, &ctx)?;
        let base = ty.base();
        // gist has default operator classes for ranges and multiranges (and, with btree_gist,
        // for the scalar types).
        let btree_gist = b.catalog.extension("btree_gist").is_some();
        let has_opclass = match method {
            "gist" | "spgist" => matches!(base, Type::Range(_) | Type::Multirange(_)) || (btree_gist && method == "gist"),
            _ => !matches!(base, Type::Json),
        };
        if !has_opclass {
            return Err(SqlError::new(
                code::UNDEFINED_OBJECT,
                format!("data type {} has no default operator class for access method \"{method}\"", base.name()),
            )
            .with_hint("You must specify an operator class for the index or define a default operator class for the data type."));
        }
        crate::funcs::resolve_binary(op, &base, &base)?;
    }
    if let Some(p) = &predicate {
        check_expr_binds(b, table, p, Clause::Where)?;
    }
    let tname = table.name.clone();
    let key_names: Vec<String> = elements.iter().map(|(e, _)| super::like::index_key_name(e)).collect();
    let refs: Vec<&str> = key_names.iter().map(String::as_str).collect();
    let suffix = match temporal {
        Some(true) => "pkey",
        Some(false) => "key",
        None => "excl",
    };
    let base_name =
        if temporal == Some(true) { default_constraint_name(&tname, &[], suffix) } else { default_constraint_name(&tname, &refs, suffix) };
    let mut all_used = used.to_vec();
    all_used.extend(taken.iter().cloned());
    let name = cname.unwrap_or_else(|| choose_name(b.catalog, &table.schema, &all_used, base_name));
    if table.indexes.iter().any(|i| i.name == name) || b.catalog.relation(&table.schema, &name).is_some() {
        return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{name}\" already exists")));
    }
    taken.push(name.clone());
    let plain: Option<Vec<usize>> = elements
        .iter()
        .map(|(e, _)| match e {
            ir::Expr::Column(p) if p.len() == 1 => table.column_index(&p[0]),
            _ => None,
        })
        .collect();
    table.indexes.push(IndexDef {
        oid: 0,
        name: name.clone(),
        columns: plain.clone().unwrap_or_default(),
        exprs: if plain.is_some() { Vec::new() } else { elements.iter().map(|(e, _)| e.clone()).collect() },
        unique: false,
        primary: false,
        nulls_not_distinct: false,
        predicate: predicate.clone(),
    });
    table.constraints.push(Constraint {
        oid: 0,
        name,
        kind: ConstraintKind::Exclude { method: method.to_owned(), elements, predicate, temporal },
        deferrable: timing.deferrable,
        initially_deferred: timing.initially_deferred,
        validated: true,
    });
    Ok(())
}

/// True for literal-only expressions (no function calls), which can serve as fast defaults.
fn is_constant_expr(e: &ir::Expr) -> bool {
    match e {
        ir::Expr::Literal(_) | ir::Expr::TypedString { .. } => true,
        ir::Expr::Cast { expr, .. } | ir::Expr::Unary { expr, .. } => is_constant_expr(expr),
        ir::Expr::Binary { left, right, .. } => is_constant_expr(left) && is_constant_expr(right),
        ir::Expr::Array(items) => items.iter().all(is_constant_expr),
        _ => false,
    }
}

/// The collations pgmem knows (Postgres' builtin ones): name and OID.
pub const COLLATIONS: &[(&str, u32)] = &[("default", 100), ("C", 950), ("POSIX", 951), ("ucs_basic", 962), ("pg_c_utf8", 811)];

/// Validates `COLLATE name` on a column of type `ty`: None for the type's default collation.
pub fn column_collation(ty: &Type, collation: &str) -> SqlResult<Option<String>> {
    let base = ty.base();
    let elem = base.element().cloned().unwrap_or(base);
    if !(elem.is_string() || elem == Type::CiText || elem == Type::Name) {
        return Err(SqlError::datatype_mismatch(format!("collations are not supported by type {}", ty.name())));
    }
    if !COLLATIONS.iter().any(|(n, _)| *n == collation) {
        return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("collation \"{collation}\" for encoding \"UTF8\" does not exist")));
    }
    Ok((collation != "default").then(|| collation.to_owned()))
}
