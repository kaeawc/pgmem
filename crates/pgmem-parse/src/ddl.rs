//! DDL lowering: CREATE TABLE / VIEW / INDEX / SEQUENCE, ALTER TABLE.

use crate::expr::{lower_expr, lower_type};
use crate::query::{lower_order_by_expr, lower_query};
use crate::{ident, object_name, unsupported};
use pgmem_ir::{
    AlterTable, AlterTableAction, ColumnConstraint, ColumnConstraintKind, ColumnDef, ConstraintTiming, CreateIndex, CreateSequence,
    CreateTable, CreateView, ForeignKeyRef, ReferentialAction, TableConstraint, TableConstraintKind,
};
use pgmem_types::{SqlError, SqlResult, code};
use sqlparser::ast as sq;

fn referential(a: Option<sq::ReferentialAction>) -> ReferentialAction {
    match a {
        None | Some(sq::ReferentialAction::NoAction) => ReferentialAction::NoAction,
        Some(sq::ReferentialAction::Restrict) => ReferentialAction::Restrict,
        Some(sq::ReferentialAction::Cascade) => ReferentialAction::Cascade,
        Some(sq::ReferentialAction::SetNull) => ReferentialAction::SetNull,
        Some(sq::ReferentialAction::SetDefault) => ReferentialAction::SetDefault,
    }
}

/// The column list of an `ON DELETE SET NULL / SET DEFAULT (cols)` the rewrite recorded (in
/// statement order).
fn delete_set_columns(action: ReferentialAction) -> Vec<String> {
    if matches!(action, ReferentialAction::SetNull | ReferentialAction::SetDefault) { crate::take_fk_set_columns() } else { Vec::new() }
}

fn index_column_names(cols: &[sq::IndexColumn]) -> SqlResult<Vec<String>> {
    cols.iter()
        .map(|c| match &c.column.expr {
            sq::Expr::Identifier(i) => Ok(ident(i)),
            other => Err(unsupported(&format!("expression `{other}` in a key constraint"))),
        })
        .collect()
}

/// `[NOT] DEFERRABLE [INITIALLY DEFERRED | IMMEDIATE]`, with Postgres' grammar checks.
fn timing(c: Option<&sq::ConstraintCharacteristics>, what: &str) -> SqlResult<ConstraintTiming> {
    let Some(c) = c else { return Ok(ConstraintTiming::default()) };
    let initially_deferred = matches!(c.initially, Some(sq::DeferrableInitial::Deferred));
    let deferrable = c.deferrable.unwrap_or(initially_deferred);
    if initially_deferred && !deferrable {
        return Err(SqlError::syntax("constraint declared INITIALLY DEFERRED must be DEFERRABLE"));
    }
    if deferrable && what == "CHECK" {
        return Err(SqlError::feature_not_supported("CHECK constraints cannot be marked DEFERRABLE"));
    }
    Ok(ConstraintTiming { deferrable, initially_deferred, not_valid: false })
}

fn lower_column_def(c: sq::ColumnDef) -> SqlResult<ColumnDef> {
    let mut constraints = Vec::new();
    for opt in c.options {
        let name = opt.name.as_ref().map(ident);
        let kind = match opt.option {
            sq::ColumnOption::Null => ColumnConstraintKind::Null,
            sq::ColumnOption::NotNull => ColumnConstraintKind::NotNull,
            sq::ColumnOption::Default(e) => ColumnConstraintKind::Default(lower_expr(e)?),
            sq::ColumnOption::PrimaryKey(pk) => {
                let name = name.clone().or_else(|| pk.name.as_ref().map(ident));
                let timing = timing(pk.characteristics.as_ref(), "PRIMARY KEY")?;
                constraints.push(ColumnConstraint { name, kind: ColumnConstraintKind::PrimaryKey, timing });
                continue;
            }
            sq::ColumnOption::Unique(u) => {
                let name = name.clone().or_else(|| u.name.as_ref().map(ident));
                let nulls_not_distinct = matches!(u.nulls_distinct, sq::NullsDistinctOption::NotDistinct);
                let timing = timing(u.characteristics.as_ref(), "UNIQUE")?;
                constraints.push(ColumnConstraint { name, kind: ColumnConstraintKind::Unique { nulls_not_distinct }, timing });
                continue;
            }
            sq::ColumnOption::ForeignKey(fk) => {
                let name = name.clone().or_else(|| fk.name.as_ref().map(ident));
                let on_delete = referential(fk.on_delete);
                let r = ForeignKeyRef {
                    table: object_name(&fk.foreign_table)?,
                    columns: fk.referred_columns.iter().map(ident).collect(),
                    on_delete,
                    on_update: referential(fk.on_update),
                    match_full: matches!(fk.match_kind, Some(sq::ConstraintReferenceMatchKind::Full)),
                    delete_set_columns: delete_set_columns(on_delete),
                };
                let timing = timing(fk.characteristics.as_ref(), "FOREIGN KEY")?;
                constraints.push(ColumnConstraint { name, kind: ColumnConstraintKind::References(r), timing });
                continue;
            }
            sq::ColumnOption::Check(chk) => {
                let name = name.clone().or_else(|| chk.name.as_ref().map(ident));
                constraints.push(ColumnConstraint {
                    name,
                    kind: ColumnConstraintKind::Check(lower_expr(*chk.expr)?),
                    timing: ConstraintTiming::default(),
                });
                continue;
            }
            sq::ColumnOption::Generated { generated_as, generation_expr, sequence_options, .. } => match generation_expr {
                Some(e) => ColumnConstraintKind::Generated(lower_expr(e)?),
                None => ColumnConstraintKind::Identity {
                    always: matches!(generated_as, sq::GeneratedAs::Always),
                    options: lower_sequence_options(sequence_options.as_deref().unwrap_or_default())?,
                },
            },
            sq::ColumnOption::Identity(_) => ColumnConstraintKind::Identity { always: false, options: Default::default() },
            sq::ColumnOption::Collation(_) | sq::ColumnOption::Comment(_) => continue,
            other => return Err(unsupported(&format!("column option {other}"))),
        };
        constraints.push(ColumnConstraint { name, kind, timing: ConstraintTiming::default() });
    }
    Ok(ColumnDef { name: ident(&c.name), ty: lower_type(&c.data_type)?, constraints })
}

fn lower_table_constraint(c: sq::TableConstraint) -> SqlResult<TableConstraint> {
    Ok(match c {
        sq::TableConstraint::PrimaryKey(pk) => TableConstraint {
            name: pk.name.as_ref().map(ident),
            kind: {
                let columns = index_column_names(&pk.columns)?;
                if crate::take_temporal_key(&columns) {
                    TableConstraintKind::Temporal { primary: true, columns }
                } else {
                    TableConstraintKind::PrimaryKey(columns)
                }
            },
            timing: timing(pk.characteristics.as_ref(), "PRIMARY KEY")?,
        },
        sq::TableConstraint::Unique(u) => TableConstraint {
            name: u.name.as_ref().map(ident),
            kind: {
                let columns = index_column_names(&u.columns)?;
                if crate::take_temporal_key(&columns) {
                    TableConstraintKind::Temporal { primary: false, columns }
                } else {
                    TableConstraintKind::Unique {
                        columns,
                        nulls_not_distinct: matches!(u.nulls_distinct, sq::NullsDistinctOption::NotDistinct),
                    }
                }
            },
            timing: timing(u.characteristics.as_ref(), "UNIQUE")?,
        },
        sq::TableConstraint::PrimaryKeyUsingIndex(u) => TableConstraint {
            name: u.name.as_ref().map(ident),
            kind: TableConstraintKind::UsingIndex { primary: true, index: ident(&u.index_name) },
            timing: timing(u.characteristics.as_ref(), "PRIMARY KEY")?,
        },
        sq::TableConstraint::UniqueUsingIndex(u) => TableConstraint {
            name: u.name.as_ref().map(ident),
            kind: TableConstraintKind::UsingIndex { primary: false, index: ident(&u.index_name) },
            timing: timing(u.characteristics.as_ref(), "UNIQUE")?,
        },
        sq::TableConstraint::Exclude(x) => TableConstraint {
            name: x.name.as_ref().map(ident),
            kind: TableConstraintKind::Exclude {
                method: x.index_method.as_ref().map(ident),
                elements: x
                    .elements
                    .into_iter()
                    .map(|e| Ok((lower_expr(e.column.column.expr)?, e.operator.to_string())))
                    .collect::<SqlResult<Vec<_>>>()?,
                predicate: x.where_clause.map(|w| lower_expr(*w)).transpose()?,
            },
            timing: timing(x.characteristics.as_ref(), "EXCLUDE")?,
        },
        sq::TableConstraint::Check(chk) => TableConstraint {
            name: chk.name.as_ref().map(ident),
            kind: TableConstraintKind::Check(lower_expr(*chk.expr)?),
            timing: ConstraintTiming::default(),
        },
        sq::TableConstraint::ForeignKey(fk) => TableConstraint {
            name: fk.name.as_ref().map(ident),
            kind: TableConstraintKind::ForeignKey {
                columns: fk.columns.iter().map(ident).collect(),
                references: ForeignKeyRef {
                    table: object_name(&fk.foreign_table)?,
                    columns: fk.referred_columns.iter().map(ident).collect(),
                    on_delete: referential(fk.on_delete),
                    on_update: referential(fk.on_update),
                    match_full: matches!(fk.match_kind, Some(sq::ConstraintReferenceMatchKind::Full)),
                    delete_set_columns: delete_set_columns(referential(fk.on_delete)),
                },
            },
            timing: timing(fk.characteristics.as_ref(), "FOREIGN KEY")?,
        },
        _ => return Err(unsupported("this table constraint")),
    })
}

pub(crate) fn lower_create_table(ct: sq::CreateTable) -> SqlResult<CreateTable> {
    if ct.clone.is_some() || ct.partition_of.is_some() || ct.inherits.is_some() {
        return Err(unsupported("CREATE TABLE ... PARTITION OF / INHERITS"));
    }
    let name = object_name(&ct.name)?;
    let mut options = pgmem_ir::CreateTableOptions {
        with_data: ct.with_data.is_none_or(|w| w.data),
        unlogged: ct.unlogged,
        on_commit: match ct.on_commit {
            Some(sq::OnCommit::DeleteRows) => pgmem_ir::OnCommit::DeleteRows,
            Some(sq::OnCommit::Drop) => pgmem_ir::OnCommit::Drop,
            _ => pgmem_ir::OnCommit::PreserveRows,
        },
        likes: Vec::new(),
        column_names: crate::take_ctas_columns(name.base()),
    };
    if let Some(sq::CreateTableLikeKind::Parenthesized(l) | sq::CreateTableLikeKind::Plain(l)) = &ct.like {
        let defaults = matches!(l.defaults, Some(sq::CreateTableLikeDefaults::Including));
        options.likes.push(pgmem_ir::LikeClause { position: 0, table: object_name(&l.name)?, defaults, ..Default::default() });
    }
    // `LIKE` elements among columns arrive as marker columns (see `rewrite_like`).
    let mut columns = Vec::new();
    for c in ct.columns {
        if let Some(spec) = c.name.value.strip_prefix(crate::LIKE_MARKER) {
            options.likes.push(like_clause(spec, columns.len())?);
        } else {
            columns.push(lower_column_def(c)?);
        }
    }
    if ct.on_commit.is_some() && !ct.temporary {
        return Err(SqlError::new(code::INVALID_TABLE_DEFINITION, "ON COMMIT can only be used on temporary tables"));
    }
    Ok(CreateTable {
        name,
        if_not_exists: ct.if_not_exists,
        temporary: ct.temporary,
        columns,
        constraints: ct.constraints.into_iter().map(lower_table_constraint).collect::<SqlResult<Vec<_>>>()?,
        as_query: ct.query.map(|q| lower_query(*q).map(Box::new)).transpose()?,
        options,
    })
}

/// A `LIKE` element: "source [INCLUDING | EXCLUDING option] ..." as the rewrite recorded it.
fn like_clause(spec: &str, position: usize) -> SqlResult<pgmem_ir::LikeClause> {
    let mut words = spec.split_whitespace();
    let table = words.next().unwrap_or_default();
    let name = pgmem_ir::ObjectName(table.split('.').map(str::to_owned).collect());
    let mut like = pgmem_ir::LikeClause { position, table: name, ..Default::default() };
    let words: Vec<String> = words.map(str::to_ascii_uppercase).collect();
    for pair in words.chunks(2) {
        let [mode, what] = pair else { return Err(SqlError::syntax("syntax error at end of input")) };
        let on = mode == "INCLUDING";
        match what.as_str() {
            "DEFAULTS" => like.defaults = on,
            "CONSTRAINTS" => like.constraints = on,
            "INDEXES" => like.indexes = on,
            "IDENTITY" => like.identity = on,
            "GENERATED" => like.generated = on,
            "COMMENTS" => like.comments = on,
            "ALL" => {
                like.defaults = on;
                like.constraints = on;
                like.indexes = on;
                like.identity = on;
                like.generated = on;
                like.comments = on;
            }
            "STORAGE" | "STATISTICS" | "COMPRESSION" => {}
            other => return Err(SqlError::syntax(format!("syntax error at or near \"{other}\""))),
        }
    }
    Ok(like)
}

pub(crate) fn lower_create_view(cv: sq::CreateView) -> SqlResult<CreateView> {
    let name = object_name(&cv.name)?;
    let (check_option, with_data) = crate::take_view_options(name.base());
    Ok(CreateView {
        check_option,
        with_data,
        name,
        or_replace: cv.or_replace,
        columns: cv.columns.iter().map(|c| ident(&c.name)).collect(),
        query: Box::new(lower_query(*cv.query)?),
        materialized: cv.materialized,
    })
}

pub(crate) fn lower_create_index(ci: sq::CreateIndex) -> SqlResult<CreateIndex> {
    Ok(CreateIndex {
        name: ci.name.as_ref().map(|n| object_name(n).map(|o| o.base().to_owned())).transpose()?,
        table: object_name(&ci.table_name)?,
        columns: ci.columns.into_iter().map(|c| lower_order_by_expr(c.column)).collect::<SqlResult<Vec<_>>>()?,
        unique: ci.unique,
        if_not_exists: ci.if_not_exists,
        concurrently: ci.concurrently,
        predicate: ci.predicate.map(lower_expr).transpose()?,
        nulls_not_distinct: ci.nulls_distinct == Some(false),
        method: ci.using.map(|u| match u {
            sq::IndexType::Custom(i) => ident(&i),
            other => other.to_string().to_lowercase(),
        }),
    })
}

fn const_i64(e: &sq::Expr) -> SqlResult<i64> {
    match e {
        sq::Expr::Value(v) => match &v.value {
            sq::Value::Number(n, _) => n.parse().map_err(|_| SqlError::syntax(format!("invalid sequence option {n}"))),
            _ => Err(SqlError::syntax("sequence options must be integers")),
        },
        sq::Expr::UnaryOp { op: sq::UnaryOperator::Minus, expr } => const_i64(expr).map(|n| -n),
        _ => Err(SqlError::syntax("sequence options must be integers")),
    }
}

pub(crate) fn lower_create_sequence(name: &sq::ObjectName, if_not_exists: bool, opts: &[sq::SequenceOptions]) -> SqlResult<CreateSequence> {
    Ok(CreateSequence { name: object_name(name)?, if_not_exists, options: lower_sequence_options(opts)? })
}

pub(crate) fn lower_sequence_options(opts: &[sq::SequenceOptions]) -> SqlResult<pgmem_ir::SequenceOptions> {
    let mut out = pgmem_ir::SequenceOptions::default();
    for o in opts {
        match o {
            sq::SequenceOptions::DataType(t) => out.data_type = Some(lower_type(t)?),
            sq::SequenceOptions::StartWith(e, _) => out.start = Some(const_i64(e)?),
            sq::SequenceOptions::IncrementBy(e, _) => out.increment = Some(const_i64(e)?),
            sq::SequenceOptions::MinValue(e) => out.min = Some(e.as_ref().map(const_i64).transpose()?),
            sq::SequenceOptions::MaxValue(e) => out.max = Some(e.as_ref().map(const_i64).transpose()?),
            sq::SequenceOptions::Cache(e) => out.cache = Some(const_i64(e)?),
            sq::SequenceOptions::Cycle(no) => out.cycle = Some(!no),
            sq::SequenceOptions::OwnedBy(name) => out.owned_by = Some(name.as_ref().map(object_name).transpose()?),
        }
    }
    Ok(out)
}

/// `ENABLE / DISABLE TRIGGER {name | ALL | USER}`.
fn trigger_toggle(name: &sq::Ident, enabled: bool) -> AlterTableAction {
    let n = ident(name);
    let all = name.quote_style.is_none() && (n == "all" || n == "user");
    AlterTableAction::EnableTrigger { name: (!all).then_some(n), enabled }
}

pub(crate) fn lower_alter_table(at: sq::AlterTable) -> SqlResult<AlterTable> {
    let mut actions = Vec::new();
    for op in at.operations {
        use sq::AlterTableOperation as O;
        match op {
            O::AddColumn { if_not_exists, column_def, .. } => {
                actions.push(AlterTableAction::AddColumn { if_not_exists, column: lower_column_def(column_def)? });
            }
            O::DropColumn { column_names, if_exists, drop_behavior, .. } => {
                for name in column_names {
                    actions.push(AlterTableAction::DropColumn {
                        name: ident(&name),
                        if_exists,
                        cascade: matches!(drop_behavior, Some(sq::DropBehavior::Cascade)),
                    });
                }
            }
            O::RenameColumn { old_column_name, new_column_name } => {
                actions.push(AlterTableAction::RenameColumn { old: ident(&old_column_name), new: ident(&new_column_name) });
            }
            O::RenameConstraint { old_name, new_name } => {
                actions.push(AlterTableAction::RenameConstraint { old: ident(&old_name), new: ident(&new_name) });
            }
            O::RenameTable { table_name } => {
                let n = match table_name {
                    sq::RenameTableNameKind::As(n) | sq::RenameTableNameKind::To(n) => object_name(&n)?,
                };
                actions.push(AlterTableAction::RenameTable { new: n.base().to_owned() });
            }
            O::AlterColumn { column_name, op } => {
                let column = ident(&column_name);
                actions.push(match op {
                    sq::AlterColumnOperation::SetNotNull => AlterTableAction::SetNotNull { column },
                    sq::AlterColumnOperation::DropNotNull => AlterTableAction::DropNotNull { column },
                    sq::AlterColumnOperation::SetDefault { value } => AlterTableAction::SetDefault { column, expr: lower_expr(value)? },
                    sq::AlterColumnOperation::DropDefault => AlterTableAction::DropDefault { column },
                    sq::AlterColumnOperation::SetDataType { data_type, using, .. } => {
                        AlterTableAction::AlterType { column, ty: lower_type(&data_type)?, using: using.map(lower_expr).transpose()? }
                    }
                    sq::AlterColumnOperation::AddGenerated { generated_as, sequence_options } => AlterTableAction::AddIdentity {
                        column,
                        always: !matches!(generated_as, Some(sq::GeneratedAs::ByDefault)),
                        options: lower_sequence_options(sequence_options.as_deref().unwrap_or_default())?,
                    },
                    _ => return Err(unsupported("this ALTER COLUMN action")),
                });
            }
            O::AddConstraint { constraint, not_valid } => {
                let mut c = lower_table_constraint(constraint)?;
                c.timing.not_valid = not_valid;
                actions.push(AlterTableAction::AddConstraint(c));
            }
            O::ValidateConstraint { name } => actions.push(AlterTableAction::ValidateConstraint { name: ident(&name) }),
            O::DropConstraint { if_exists, name, drop_behavior } => actions.push(AlterTableAction::DropConstraint {
                name: ident(&name),
                if_exists,
                cascade: matches!(drop_behavior, Some(sq::DropBehavior::Cascade)),
            }),
            O::EnableTrigger { name } => actions.push(trigger_toggle(&name, true)),
            O::DisableTrigger { name } => actions.push(trigger_toggle(&name, false)),
            O::OwnerTo { new_owner } => actions.push(AlterTableAction::OwnerTo(match new_owner {
                sq::Owner::Ident(i) => ident(&i),
                sq::Owner::CurrentRole => "current_role".to_owned(),
                sq::Owner::CurrentUser => "current_user".to_owned(),
                sq::Owner::SessionUser => "session_user".to_owned(),
            })),
            O::EnableRowLevelSecurity | O::DisableRowLevelSecurity | O::SetLogged | O::SetUnlogged => {}
            other => return Err(unsupported(&format!("ALTER TABLE {other}"))),
        }
    }
    Ok(AlterTable { name: object_name(&at.name)?, if_exists: at.if_exists, actions })
}
