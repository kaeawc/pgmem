//! DDL lowering: CREATE TABLE / VIEW / INDEX / SEQUENCE, ALTER TABLE.

use crate::expr::{lower_expr, lower_type};
use crate::query::{lower_order_by_expr, lower_query};
use crate::{ident, object_name, unsupported};
use pgmem_ir::{
    AlterTable, AlterTableAction, ColumnConstraint, ColumnConstraintKind, ColumnDef, CreateIndex, CreateSequence, CreateTable, CreateView,
    ForeignKeyRef, ReferentialAction, TableConstraint, TableConstraintKind,
};
use pgmem_types::{SqlError, SqlResult};
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

fn index_column_names(cols: &[sq::IndexColumn]) -> SqlResult<Vec<String>> {
    cols.iter()
        .map(|c| match &c.column.expr {
            sq::Expr::Identifier(i) => Ok(ident(i)),
            other => Err(unsupported(&format!("expression `{other}` in a key constraint"))),
        })
        .collect()
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
                constraints.push(ColumnConstraint { name, kind: ColumnConstraintKind::PrimaryKey });
                continue;
            }
            sq::ColumnOption::Unique(u) => {
                let name = name.clone().or_else(|| u.name.as_ref().map(ident));
                let nulls_not_distinct = matches!(u.nulls_distinct, sq::NullsDistinctOption::NotDistinct);
                constraints.push(ColumnConstraint { name, kind: ColumnConstraintKind::Unique { nulls_not_distinct } });
                continue;
            }
            sq::ColumnOption::ForeignKey(fk) => {
                let name = name.clone().or_else(|| fk.name.as_ref().map(ident));
                let r = ForeignKeyRef {
                    table: object_name(&fk.foreign_table)?,
                    columns: fk.referred_columns.iter().map(ident).collect(),
                    on_delete: referential(fk.on_delete),
                    on_update: referential(fk.on_update),
                };
                constraints.push(ColumnConstraint { name, kind: ColumnConstraintKind::References(r) });
                continue;
            }
            sq::ColumnOption::Check(chk) => {
                let name = name.clone().or_else(|| chk.name.as_ref().map(ident));
                constraints.push(ColumnConstraint { name, kind: ColumnConstraintKind::Check(lower_expr(*chk.expr)?) });
                continue;
            }
            sq::ColumnOption::Generated { generated_as, generation_expr, .. } => match generation_expr {
                Some(e) => ColumnConstraintKind::Generated(lower_expr(e)?),
                None => ColumnConstraintKind::Identity { always: matches!(generated_as, sq::GeneratedAs::Always) },
            },
            sq::ColumnOption::Identity(_) => ColumnConstraintKind::Identity { always: false },
            sq::ColumnOption::Collation(_) | sq::ColumnOption::Comment(_) => continue,
            other => return Err(unsupported(&format!("column option {other}"))),
        };
        constraints.push(ColumnConstraint { name, kind });
    }
    Ok(ColumnDef { name: ident(&c.name), ty: lower_type(&c.data_type)?, constraints })
}

fn lower_table_constraint(c: sq::TableConstraint) -> SqlResult<TableConstraint> {
    Ok(match c {
        sq::TableConstraint::PrimaryKey(pk) => {
            TableConstraint { name: pk.name.as_ref().map(ident), kind: TableConstraintKind::PrimaryKey(index_column_names(&pk.columns)?) }
        }
        sq::TableConstraint::Unique(u) => TableConstraint {
            name: u.name.as_ref().map(ident),
            kind: TableConstraintKind::Unique {
                columns: index_column_names(&u.columns)?,
                nulls_not_distinct: matches!(u.nulls_distinct, sq::NullsDistinctOption::NotDistinct),
            },
        },
        sq::TableConstraint::Check(chk) => {
            TableConstraint { name: chk.name.as_ref().map(ident), kind: TableConstraintKind::Check(lower_expr(*chk.expr)?) }
        }
        sq::TableConstraint::ForeignKey(fk) => TableConstraint {
            name: fk.name.as_ref().map(ident),
            kind: TableConstraintKind::ForeignKey {
                columns: fk.columns.iter().map(ident).collect(),
                references: ForeignKeyRef {
                    table: object_name(&fk.foreign_table)?,
                    columns: fk.referred_columns.iter().map(ident).collect(),
                    on_delete: referential(fk.on_delete),
                    on_update: referential(fk.on_update),
                },
            },
        },
        _ => return Err(unsupported("this table constraint")),
    })
}

pub(crate) fn lower_create_table(ct: sq::CreateTable) -> SqlResult<CreateTable> {
    if ct.like.is_some() || ct.clone.is_some() || ct.partition_of.is_some() || ct.inherits.is_some() {
        return Err(unsupported("CREATE TABLE ... LIKE / PARTITION OF / INHERITS"));
    }
    Ok(CreateTable {
        name: object_name(&ct.name)?,
        if_not_exists: ct.if_not_exists,
        temporary: ct.temporary,
        columns: ct.columns.into_iter().map(lower_column_def).collect::<SqlResult<Vec<_>>>()?,
        constraints: ct.constraints.into_iter().map(lower_table_constraint).collect::<SqlResult<Vec<_>>>()?,
        as_query: ct.query.map(|q| lower_query(*q).map(Box::new)).transpose()?,
    })
}

pub(crate) fn lower_create_view(cv: sq::CreateView) -> SqlResult<CreateView> {
    Ok(CreateView {
        name: object_name(&cv.name)?,
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
    let mut seq = CreateSequence { name: object_name(name)?, if_not_exists, start: None, increment: None };
    for o in opts {
        match o {
            sq::SequenceOptions::StartWith(e, _) => seq.start = Some(const_i64(e)?),
            sq::SequenceOptions::IncrementBy(e, _) => seq.increment = Some(const_i64(e)?),
            _ => {}
        }
    }
    Ok(seq)
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
                    _ => return Err(unsupported("this ALTER COLUMN action")),
                });
            }
            O::AddConstraint { constraint, .. } => actions.push(AlterTableAction::AddConstraint(lower_table_constraint(constraint)?)),
            O::DropConstraint { if_exists, name, drop_behavior } => actions.push(AlterTableAction::DropConstraint {
                name: ident(&name),
                if_exists,
                cascade: matches!(drop_behavior, Some(sq::DropBehavior::Cascade)),
            }),
            O::OwnerTo { .. } | O::EnableRowLevelSecurity | O::DisableRowLevelSecurity | O::SetLogged | O::SetUnlogged => {}
            other => return Err(unsupported(&format!("ALTER TABLE {other}"))),
        }
    }
    Ok(AlterTable { name: object_name(&at.name)?, if_exists: at.if_exists, actions })
}
