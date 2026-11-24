//! Schema metadata. A [`Catalog`] is an immutable-by-convention value: transactions clone it
//! (cheaply — table definitions are behind `Arc`s), modify their copy, and the storage layer
//! swaps the committed version in at COMMIT. That makes DDL transactional, as in Postgres.
//!
//! Default, CHECK and generated-column expressions are kept as unbound IR and bound by the
//! executor when used, so they always see the current schema.

use pgmem_ir::Expr;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use std::collections::BTreeMap;
use std::sync::Arc;

pub type Oid = u32;

/// The schema every user object lives in. pgmem models a single schema; `public.x` resolves
/// to `x`.
pub const PUBLIC_SCHEMA: &str = "public";
pub const PUBLIC_NAMESPACE_OID: Oid = 2200;
pub const PG_CATALOG_NAMESPACE_OID: Oid = 11;
/// First OID handed to user objects (Postgres' `FirstNormalObjectId`).
pub const FIRST_USER_OID: Oid = 16384;

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: Type,
    /// Declared type modifier (`varchar(20)` → 20); -1 when absent.
    pub typmod: i32,
    pub not_null: bool,
    pub default: Option<Expr>,
    /// `GENERATED ALWAYS AS (expr) STORED`.
    pub generated: Option<Expr>,
    pub identity: Option<Identity>,
    /// Dropped columns keep their physical slot (Postgres' `attisdropped`), so existing rows
    /// never need rewriting.
    pub dropped: bool,
    /// Value rows created before this column was added read back as (Postgres'
    /// `attmissingval`): NULL, or the ADD COLUMN ... DEFAULT constant.
    pub missing: Value,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, ty: Type) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            ty,
            typmod: -1,
            not_null: false,
            default: None,
            generated: None,
            identity: None,
            dropped: false,
            missing: Value::Null,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    Always,
    ByDefault,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FkAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl FkAction {
    pub fn code(self) -> char {
        match self {
            FkAction::NoAction => 'a',
            FkAction::Restrict => 'r',
            FkAction::Cascade => 'c',
            FkAction::SetNull => 'n',
            FkAction::SetDefault => 'd',
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConstraintKind {
    PrimaryKey { columns: Vec<usize> },
    Unique { columns: Vec<usize>, nulls_not_distinct: bool },
    Check { expr: Expr },
    ForeignKey { columns: Vec<usize>, ref_table: String, ref_columns: Vec<usize>, on_delete: FkAction, on_update: FkAction },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Constraint {
    pub oid: Oid,
    pub name: String,
    pub kind: ConstraintKind,
}

impl Constraint {
    /// `pg_constraint.contype`.
    pub fn contype(&self) -> char {
        match self.kind {
            ConstraintKind::PrimaryKey { .. } => 'p',
            ConstraintKind::Unique { .. } => 'u',
            ConstraintKind::Check { .. } => 'c',
            ConstraintKind::ForeignKey { .. } => 'f',
        }
    }
}

/// A btree index. Unique indexes (including the ones backing PRIMARY KEY and UNIQUE
/// constraints) are enforced; non-unique ones exist for catalog fidelity only.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexDef {
    pub oid: Oid,
    pub name: String,
    /// Physical column positions. Empty when the index is on expressions only.
    pub columns: Vec<usize>,
    /// Expression keys (`lower(email)`), kept unbound; enforced for unique indexes.
    pub exprs: Vec<Expr>,
    pub unique: bool,
    pub primary: bool,
    pub nulls_not_distinct: bool,
    /// Partial index predicate.
    pub predicate: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableDef {
    pub oid: Oid,
    pub name: String,
    /// Physical column slots in attnum order (attnum = index + 1), including dropped ones.
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<Constraint>,
    pub indexes: Vec<IndexDef>,
}

impl TableDef {
    /// Live (not dropped) columns as (physical index, def), in attnum order.
    pub fn live_columns(&self) -> impl Iterator<Item = (usize, &ColumnDef)> {
        self.columns.iter().enumerate().filter(|(_, c)| !c.dropped)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| !c.dropped && c.name == name)
    }

    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.column_index(name).map(|i| &self.columns[i])
    }

    pub fn primary_key(&self) -> Option<&[usize]> {
        self.constraints.iter().find_map(|c| match &c.kind {
            ConstraintKind::PrimaryKey { columns } => Some(columns.as_slice()),
            _ => None,
        })
    }

    pub fn has_constraint(&self, name: &str) -> bool {
        self.constraints.iter().any(|c| c.name == name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ViewDef {
    pub oid: Oid,
    pub name: String,
    /// Output column names (from the column list or the query's select list).
    pub columns: Vec<String>,
    pub query: pgmem_ir::Query,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SequenceDef {
    pub oid: Oid,
    pub name: String,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    /// `table.column` this sequence is OWNED BY (serial / identity columns).
    pub owned_by: Option<(String, String)>,
}

/// What a relation name refers to.
#[derive(Clone, Debug)]
pub enum Relation {
    Table(Arc<TableDef>),
    View(Arc<ViewDef>),
    Sequence(Arc<SequenceDef>),
    Index { table: String, index: IndexDef },
}

impl Relation {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Relation::Table(_) => "table",
            Relation::View(_) => "view",
            Relation::Sequence(_) => "sequence",
            Relation::Index { .. } => "index",
        }
    }
}

/// The set of user objects visible to a transaction.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    tables: BTreeMap<String, Arc<TableDef>>,
    views: BTreeMap<String, Arc<ViewDef>>,
    sequences: BTreeMap<String, Arc<SequenceDef>>,
    next_oid: Oid,
}

impl Catalog {
    pub fn new() -> Catalog {
        Catalog { next_oid: FIRST_USER_OID, ..Default::default() }
    }

    pub fn alloc_oid(&mut self) -> Oid {
        if self.next_oid < FIRST_USER_OID {
            self.next_oid = FIRST_USER_OID;
        }
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }

    /// Strips a `public.` qualifier; other schemas are reported as missing relations.
    pub fn normalize<'a>(&self, name: &'a pgmem_ir::ObjectName) -> SqlResult<&'a str> {
        match name.0.as_slice() {
            [n] => Ok(n),
            [schema, n] if schema == PUBLIC_SCHEMA => Ok(n),
            [_, _] | [_, _, _] => Err(SqlError::undefined_table(&name.to_string())),
            _ => Err(SqlError::syntax(format!("improper qualified name (too many dotted names): {name}"))),
        }
    }

    pub fn table(&self, name: &str) -> Option<&Arc<TableDef>> {
        self.tables.get(name)
    }

    pub fn table_by_oid(&self, oid: Oid) -> Option<&Arc<TableDef>> {
        self.tables.values().find(|t| t.oid == oid)
    }

    pub fn view(&self, name: &str) -> Option<&Arc<ViewDef>> {
        self.views.get(name)
    }

    pub fn sequence(&self, name: &str) -> Option<&Arc<SequenceDef>> {
        self.sequences.get(name)
    }

    pub fn tables(&self) -> impl Iterator<Item = &Arc<TableDef>> {
        self.tables.values()
    }

    pub fn views(&self) -> impl Iterator<Item = &Arc<ViewDef>> {
        self.views.values()
    }

    pub fn sequences(&self) -> impl Iterator<Item = &Arc<SequenceDef>> {
        self.sequences.values()
    }

    /// Looks up any relation (table, view, sequence or index) by name; they share one
    /// namespace in Postgres (`pg_class`).
    pub fn relation(&self, name: &str) -> Option<Relation> {
        if let Some(t) = self.tables.get(name) {
            return Some(Relation::Table(Arc::clone(t)));
        }
        if let Some(v) = self.views.get(name) {
            return Some(Relation::View(Arc::clone(v)));
        }
        if let Some(s) = self.sequences.get(name) {
            return Some(Relation::Sequence(Arc::clone(s)));
        }
        self.tables
            .values()
            .find_map(|t| t.indexes.iter().find(|i| i.name == name).map(|i| Relation::Index { table: t.name.clone(), index: i.clone() }))
    }

    fn ensure_free(&self, name: &str) -> SqlResult<()> {
        if self.relation(name).is_some() {
            return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{name}\" already exists")));
        }
        Ok(())
    }

    pub fn create_table(&mut self, def: TableDef) -> SqlResult<()> {
        self.ensure_free(&def.name)?;
        for idx in &def.indexes {
            self.ensure_free(&idx.name)?;
        }
        self.tables.insert(def.name.clone(), Arc::new(def));
        Ok(())
    }

    /// Replaces an existing table definition (ALTER TABLE). The name may change (RENAME).
    pub fn replace_table(&mut self, old_name: &str, def: TableDef) {
        self.tables.remove(old_name);
        self.tables.insert(def.name.clone(), Arc::new(def));
    }

    pub fn drop_table(&mut self, name: &str) -> Option<Arc<TableDef>> {
        let dropped = self.tables.remove(name)?;
        // Sequences owned by the table's columns go with it.
        self.sequences.retain(|_, s| s.owned_by.as_ref().is_none_or(|(t, _)| t != name));
        Some(dropped)
    }

    pub fn create_view(&mut self, def: ViewDef, or_replace: bool) -> SqlResult<()> {
        if or_replace && self.views.contains_key(&def.name) {
            self.views.insert(def.name.clone(), Arc::new(def));
            return Ok(());
        }
        self.ensure_free(&def.name)?;
        self.views.insert(def.name.clone(), Arc::new(def));
        Ok(())
    }

    pub fn drop_view(&mut self, name: &str) -> Option<Arc<ViewDef>> {
        self.views.remove(name)
    }

    pub fn create_sequence(&mut self, def: SequenceDef) -> SqlResult<()> {
        self.ensure_free(&def.name)?;
        self.sequences.insert(def.name.clone(), Arc::new(def));
        Ok(())
    }

    pub fn drop_sequence(&mut self, name: &str) -> Option<Arc<SequenceDef>> {
        self.sequences.remove(name)
    }

    /// Picks an unused relation name the way Postgres' `ChooseRelationName` does:
    /// `base`, then `base1`, `base2`, ...
    pub fn choose_name(&self, base: &str, taken: &[String]) -> String {
        let free = |n: &str| self.relation(n).is_none() && !taken.iter().any(|t| t == n);
        if free(base) {
            return base.to_owned();
        }
        (1..).map(|i| format!("{base}{i}")).find(|n| free(n)).unwrap_or_else(|| base.to_owned())
    }
}

/// Postgres' default constraint/index name: `table_col1_col2_suffix`, truncated to 63 bytes.
pub fn default_constraint_name(table: &str, columns: &[&str], suffix: &str) -> String {
    let mut name = String::from(table);
    for c in columns {
        name.push('_');
        name.push_str(c);
    }
    name.push('_');
    name.push_str(suffix);
    truncate_identifier(name)
}

/// Identifiers longer than NAMEDATALEN-1 (63) bytes are truncated, on a char boundary.
pub fn truncate_identifier(mut s: String) -> String {
    if s.len() > 63 {
        let mut end = 63;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str) -> TableDef {
        TableDef { oid: 0, name: name.into(), columns: vec![ColumnDef::new("id", Type::Int4)], constraints: vec![], indexes: vec![] }
    }

    #[test]
    fn relations_share_a_namespace() {
        let mut cat = Catalog::new();
        cat.create_table(table("users")).expect("create");
        let err = cat.create_table(table("users")).expect_err("dup");
        assert_eq!(err.code, "42P07");
        assert_eq!(err.message, "relation \"users\" already exists");
        assert!(matches!(cat.relation("users"), Some(Relation::Table(_))));
    }

    #[test]
    fn default_names() {
        assert_eq!(default_constraint_name("users", &["email"], "key"), "users_email_key");
        assert_eq!(default_constraint_name("t", &[], "pkey"), "t_pkey");
        assert_eq!(truncate_identifier("x".repeat(70)).len(), 63);
    }

    #[test]
    fn choose_name_avoids_collisions() {
        let mut cat = Catalog::new();
        cat.create_table(table("t_pkey")).expect("create");
        assert_eq!(cat.choose_name("t_pkey", &[]), "t_pkey1");
    }
}
