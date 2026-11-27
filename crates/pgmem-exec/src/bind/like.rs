//! `CREATE TABLE t (..., LIKE source [INCLUDING ...], ...)`: copies the source's columns
//! (names, types and NOT NULL always; defaults, generation expressions, identity, CHECKs,
//! indexes and comments on request), as Postgres' `transformTableLikeClause` does.

use super::{Binder, Ctx, Level, Scope};
use crate::catalog_views::{CLASS_CLASS_OID, not_null_constraint_name};
use pgmem_catalog::{ColumnDef, ConstraintKind, IndexDef, Relation, SequenceDef, TableDef, default_constraint_name, truncate_identifier};
use pgmem_ir as ir;
use pgmem_types::{SqlError, SqlResult, Value, code};

/// A constraint to add once every column is known: (name, kind, timing).
pub type PendingConstraint = (Option<String>, ir::TableConstraintKind, ir::ConstraintTiming);

/// What the LIKE clauses add besides columns.
#[derive(Default)]
pub struct LikeExtras {
    pub constraints: Vec<PendingConstraint>,
    /// Plain (non-constraint) indexes, keyed by column name, named once the table is built.
    pub indexes: Vec<LikeIndex>,
    /// Column comments: (column name, text).
    pub comments: Vec<(String, String)>,
}

pub struct LikeIndex {
    pub columns: Vec<String>,
    pub exprs: Vec<ir::Expr>,
    pub unique: bool,
    pub nulls_not_distinct: bool,
    pub predicate: Option<ir::Expr>,
    pub method: String,
    pub opclasses: Vec<pgmem_catalog::IndexOpclass>,
    pub include: Vec<String>,
    pub reloptions: Vec<String>,
}

/// Inserts the LIKE sources' columns into `table` (whose columns are the statement's own, in
/// order) and returns the rest of what they copy.
pub fn expand(
    b: &Binder,
    ct: &ir::CreateTable,
    table: &mut TableDef,
    sequences: &mut Vec<SequenceDef>,
    taken: &mut Vec<String>,
) -> SqlResult<LikeExtras> {
    let mut extras = LikeExtras::default();
    let mut inserted = 0;
    for like in &ct.options.likes {
        let columns = copy_columns(b, like, table, sequences, taken, &mut extras)?;
        let at = like.position + inserted;
        inserted += columns.len();
        table.columns.splice(at..at, columns);
    }
    Ok(extras)
}

fn source(b: &Binder, name: &ir::ObjectName) -> SqlResult<TableDef> {
    match b.catalog.lookup_relation(name)? {
        Some(Relation::Table(t)) => Ok((*t).clone()),
        Some(Relation::View(v)) => {
            // A view's columns, typed by binding its query.
            let top = Level::new(Scope::default());
            let bound = pgmem_types::context::with_search_path(&v.search_path, || b.bind_query(&v.query, &Ctx::new(&top)))?;
            let columns = bound
                .columns
                .iter()
                .zip(&v.columns)
                .map(|(c, n)| {
                    let mut col = ColumnDef::new(n.clone(), c.ty.clone());
                    col.typmod = c.typmod;
                    col
                })
                .collect();
            let def = TableDef {
                oid: v.oid,
                schema: v.schema.clone(),
                name: v.name.clone(),
                columns,
                constraints: Vec::new(),
                indexes: Vec::new(),
                matview: None,
                unlogged: false,
                replident: 'd',
                reloptions: Vec::new(),
            };
            Ok(def)
        }
        Some(other) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("relation \"{}\" is invalid in LIKE clause", other.name()))
            .with_detail(format!("This operation is not supported for {}.", relation_kind(&other)))),
        None => Err(SqlError::undefined_table(&name.to_string())),
    }
}

fn relation_kind(r: &Relation) -> &'static str {
    match r {
        Relation::Sequence(_) => "sequences",
        Relation::Index { .. } => "indexes",
        _ => "this relation",
    }
}

fn copy_columns(
    b: &Binder,
    like: &ir::LikeClause,
    table: &TableDef,
    sequences: &mut Vec<SequenceDef>,
    taken: &mut Vec<String>,
    extras: &mut LikeExtras,
) -> SqlResult<Vec<ColumnDef>> {
    let src = source(b, &like.table)?;
    let mut out = Vec::new();
    for (_, c) in src.live_columns() {
        let mut col = ColumnDef::new(c.name.clone(), c.ty.clone());
        col.typmod = c.typmod;
        if c.not_null {
            col.not_null = true;
            col.not_null_name = Some(c.not_null_name.clone().unwrap_or_else(|| not_null_constraint_name(&src.name, &c.name)));
        }
        if like.generated && c.generated.is_some() {
            col.generated.clone_from(&c.generated);
        } else if c.identity.is_some() {
            if like.identity {
                let seq = identity_sequence(b, &src, &c.name, table, taken)?;
                col.default = Some(super::ddl::nextval_default(&table.schema, &seq.name));
                col.identity = c.identity;
                sequences.push(seq);
            }
        } else if like.defaults && c.generated.is_none() {
            col.default.clone_from(&c.default);
        }
        col.missing = Value::Null;
        out.push(col);
    }
    if like.constraints {
        for con in &src.constraints {
            if let ConstraintKind::Check { expr } = &con.kind {
                let timing =
                    ir::ConstraintTiming { deferrable: con.deferrable, initially_deferred: con.initially_deferred, not_valid: false };
                extras.constraints.push((Some(con.name.clone()), ir::TableConstraintKind::Check(expr.clone()), timing));
            }
        }
    }
    let name_of = |i: &usize| src.columns[*i].name.clone();
    if like.indexes {
        for idx in &src.indexes {
            let backing = src.constraints.iter().find(|c| c.name == idx.name);
            let columns: Vec<String> = idx.columns.iter().map(name_of).collect();
            match backing.map(|c| (&c.kind, c)) {
                Some((ConstraintKind::PrimaryKey { .. }, con)) => {
                    extras.constraints.push((None, ir::TableConstraintKind::PrimaryKey(columns), timing_of(con)));
                }
                Some((ConstraintKind::Unique { nulls_not_distinct, .. }, con)) => extras.constraints.push((
                    None,
                    ir::TableConstraintKind::Unique { columns, nulls_not_distinct: *nulls_not_distinct },
                    timing_of(con),
                )),
                _ => extras.indexes.push(LikeIndex {
                    columns,
                    exprs: idx.exprs.clone(),
                    unique: idx.unique,
                    nulls_not_distinct: idx.nulls_not_distinct,
                    predicate: idx.predicate.clone(),
                    method: idx.method.clone(),
                    opclasses: idx.opclasses.clone(),
                    include: idx.include.iter().map(name_of).collect(),
                    reloptions: idx.reloptions.clone(),
                }),
            }
        }
    }
    if like.comments {
        for (i, c) in src.live_columns() {
            let subid = i32::try_from(i + 1).unwrap_or(i32::MAX);
            if let Some(text) = b.catalog.comment(CLASS_CLASS_OID, src.oid, subid) {
                extras.comments.push((c.name.clone(), text.to_owned()));
            }
        }
    }
    Ok(out)
}

fn timing_of(c: &pgmem_catalog::Constraint) -> ir::ConstraintTiming {
    ir::ConstraintTiming { deferrable: c.deferrable, initially_deferred: c.initially_deferred, not_valid: false }
}

/// A new sequence for a copied identity column, with the source sequence's parameters.
fn identity_sequence(b: &Binder, src: &TableDef, column: &str, table: &TableDef, taken: &mut Vec<String>) -> SqlResult<SequenceDef> {
    let owner = Some((src.name.clone(), column.to_owned()));
    let from = b
        .catalog
        .sequences()
        .find(|s| s.schema == src.schema && s.owned_by == owner)
        .ok_or_else(|| SqlError::internal("identity column without a sequence"))?;
    let name = b.catalog.choose_name(&table.schema, &truncate_identifier(default_constraint_name(&table.name, &[column], "seq")), taken);
    taken.push(name.clone());
    Ok(SequenceDef {
        oid: 0,
        schema: table.schema.clone(),
        name,
        owned_by: Some((table.name.clone(), column.to_owned())),
        ..(**from).clone()
    })
}

/// The name part an index key contributes to a generated index name (Postgres'
/// `FigureIndexColname`): the column, the function's name, or "expr".
pub fn index_key_name(e: &ir::Expr) -> String {
    match e {
        ir::Expr::Column(parts) => parts.last().cloned().unwrap_or_default(),
        ir::Expr::Function(f) => f.name.base().to_owned(),
        ir::Expr::Cast { expr, .. } => index_key_name(expr),
        _ => "expr".to_owned(),
    }
}

/// Builds a copied plain index on `table`, choosing its name.
pub fn build_index(b: &Binder, table: &TableDef, li: &LikeIndex, taken: &mut Vec<String>) -> SqlResult<IndexDef> {
    let columns = li
        .columns
        .iter()
        .map(|n| table.column_index(n).ok_or_else(|| SqlError::undefined_column(&format!("\"{n}\""))))
        .collect::<SqlResult<Vec<_>>>()?;
    let names: Vec<String> = if li.exprs.is_empty() { li.columns.clone() } else { li.exprs.iter().map(index_key_name).collect() };
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut used = taken.clone();
    used.extend(table.indexes.iter().map(|i| i.name.clone()));
    let name = b.catalog.choose_name(
        &table.schema,
        &truncate_identifier(default_constraint_name(&table.name, &refs, if li.unique { "key" } else { "idx" })),
        &used,
    );
    taken.push(name.clone());
    let include = li
        .include
        .iter()
        .map(|n| table.column_index(n).ok_or_else(|| SqlError::undefined_column(&format!("\"{n}\""))))
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(IndexDef {
        method: li.method.clone(),
        opclasses: li.opclasses.clone(),
        oid: 0,
        name,
        columns,
        exprs: li.exprs.clone(),
        include,
        unique: li.unique,
        primary: false,
        nulls_not_distinct: li.nulls_not_distinct,
        predicate: li.predicate.clone(),
        clustered: false,
        replica: false,
        reloptions: li.reloptions.clone(),
    })
}
