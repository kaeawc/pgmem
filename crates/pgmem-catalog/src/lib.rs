//! Schema metadata. A [`Catalog`] is an immutable-by-convention value: transactions clone it
//! (cheaply — table definitions are behind `Arc`s), modify their copy, and the storage layer
//! swaps the committed version in at COMMIT. That makes DDL transactional, as in Postgres.
//!
//! Default, CHECK and generated-column expressions are kept as unbound IR and bound by the
//! executor when used, so they always see the current schema.

use pgmem_ir::Expr;
use pgmem_types::usertype::{EnumLabel, Field, UserKind, UserType};
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use std::collections::BTreeMap;
use std::sync::Arc;

mod partitions;
mod roles;
pub use partitions::{PartitionBound, PartitionKey, PartitionKeyPart, PartitionOf, PartitionStrategy, RangeDatum};
pub use roles::{DefaultPrivilege, ExtensionDef, Grant, GrantObject, RoleDef};

pub type Oid = u32;

/// The schema every database starts with, first on the default `search_path` after `$user`.
pub const PUBLIC_SCHEMA: &str = "public";
pub const PUBLIC_NAMESPACE_OID: Oid = 2200;
pub const PG_CATALOG: &str = "pg_catalog";
pub const PG_CATALOG_NAMESPACE_OID: Oid = 11;
pub const INFORMATION_SCHEMA: &str = "information_schema";
/// `pg_namespace.oid` of information_schema in a fresh PostgreSQL 18 cluster.
pub const INFORMATION_SCHEMA_NAMESPACE_OID: Oid = 13_336;
pub const PG_TOAST_NAMESPACE_OID: Oid = 99;
/// First OID handed to user objects (Postgres' `FirstNormalObjectId`).
pub const FIRST_USER_OID: Oid = 16384;

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: Type,
    /// Declared type modifier (`varchar(20)` → 20); -1 when absent.
    pub typmod: i32,
    pub not_null: bool,
    /// The name given to the NOT NULL constraint (`CONSTRAINT a_required NOT NULL`); None
    /// for the default `<table>_<column>_not_null`.
    pub not_null_name: Option<String>,
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
    /// `COLLATE name` when not the type's default collation.
    pub collation: Option<String>,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, ty: Type) -> ColumnDef {
        ColumnDef {
            name: name.into(),
            ty,
            typmod: -1,
            not_null: false,
            not_null_name: None,
            default: None,
            generated: None,
            identity: None,
            dropped: false,
            missing: Value::Null,
            collation: None,
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
    PrimaryKey {
        columns: Vec<usize>,
    },
    Unique {
        columns: Vec<usize>,
        nulls_not_distinct: bool,
    },
    Check {
        expr: Expr,
    },
    ForeignKey {
        columns: Vec<usize>,
        ref_table: QualName,
        ref_columns: Vec<usize>,
        on_delete: FkAction,
        on_update: FkAction,
        /// `MATCH FULL`: either every key column is NULL or none is.
        match_full: bool,
        /// `ON DELETE SET NULL / SET DEFAULT (cols)`: the referencing columns set (empty = all).
        delete_set_columns: Vec<usize>,
    },
    /// An exclusion constraint: no two rows (where `predicate` holds) for which every
    /// `key op key` is true. Temporal keys (`WITHOUT OVERLAPS`) are exclusions too, reported
    /// as primary key / unique constraints.
    Exclude {
        method: String,
        /// Key expressions (over column names) with their operators.
        elements: Vec<(Expr, String)>,
        predicate: Option<Expr>,
        /// `Some(true)` for `PRIMARY KEY (... WITHOUT OVERLAPS)`, `Some(false)` for UNIQUE.
        temporal: Option<bool>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Constraint {
    pub oid: Oid,
    pub name: String,
    pub kind: ConstraintKind,
    /// `DEFERRABLE` (`pg_constraint.condeferrable`).
    pub deferrable: bool,
    /// `INITIALLY DEFERRED` (`condeferred`).
    pub initially_deferred: bool,
    /// False for a constraint added `NOT VALID` and not validated since (`convalidated`).
    pub validated: bool,
}

impl Constraint {
    /// `pg_constraint.contype`.
    pub fn contype(&self) -> char {
        match self.kind {
            ConstraintKind::PrimaryKey { .. } => 'p',
            ConstraintKind::Unique { .. } => 'u',
            ConstraintKind::Check { .. } => 'c',
            ConstraintKind::ForeignKey { .. } => 'f',
            ConstraintKind::Exclude { temporal: Some(true), .. } => 'p',
            ConstraintKind::Exclude { temporal: Some(false), .. } => 'u',
            ConstraintKind::Exclude { temporal: None, .. } => 'x',
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
    /// `INCLUDE` (covering) column positions: stored in the index but not part of its key.
    pub include: Vec<usize>,
    pub unique: bool,
    pub primary: bool,
    pub nulls_not_distinct: bool,
    /// Partial index predicate.
    pub predicate: Option<Expr>,
    /// `pg_class.reloptions` (`name=value`, in Postgres' order); empty means NULL.
    pub reloptions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableDef {
    pub oid: Oid,
    /// The schema (namespace) the table lives in.
    pub schema: String,
    pub name: String,
    /// Physical column slots in attnum order (attnum = index + 1), including dropped ones.
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<Constraint>,
    pub indexes: Vec<IndexDef>,
    /// Set for a materialized view (stored like a table, refreshed from its query).
    pub matview: Option<Box<MatView>>,
    /// `CREATE UNLOGGED TABLE` (`relpersistence = 'u'`).
    pub unlogged: bool,
    /// `pg_class.reloptions` (`name=value`, in Postgres' order); empty means NULL.
    pub reloptions: Vec<String>,
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

    pub fn qualified(&self) -> QualName {
        QualName::new(&self.schema, &self.name)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ViewDef {
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    /// Output column names (from the column list or the query's select list).
    pub columns: Vec<String>,
    pub query: pgmem_ir::Query,
    /// The effective search path when the view was created: its names resolve against it,
    /// as Postgres binds a view's relations at creation.
    pub search_path: Vec<String>,
    /// `WITH CHECK OPTION`: rows written through the view must stay visible in it.
    pub check_option: bool,
    /// `pg_class.reloptions` (`name=value`, in Postgres' order); empty means NULL.
    pub reloptions: Vec<String>,
}

/// What makes a table a materialized view: its defining query and whether it holds data.
#[derive(Clone, Debug, PartialEq)]
pub struct MatView {
    pub query: pgmem_ir::Query,
    pub search_path: Vec<String>,
    pub populated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SequenceDef {
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    /// The table (in the sequence's schema) and column this sequence is OWNED BY (serial /
    /// identity columns).
    pub owned_by: Option<(String, String)>,
    pub cycle: bool,
    /// `AS smallint | integer | bigint`.
    pub data_type: Type,
    pub cache: i64,
}

impl SequenceDef {
    /// A sequence with Postgres' defaults for `data_type` (ascending from 1).
    pub fn new(schema: impl Into<String>, name: impl Into<String>, data_type: Type) -> SequenceDef {
        let max = match data_type {
            Type::Int2 => i64::from(i16::MAX),
            Type::Int4 => i64::from(i32::MAX),
            _ => i64::MAX,
        };
        SequenceDef {
            oid: 0,
            schema: schema.into(),
            name: name.into(),
            start: 1,
            increment: 1,
            min: 1,
            max,
            owned_by: None,
            cycle: false,
            data_type,
            cache: 1,
        }
    }
}

/// A function argument (`pg_proc.proargnames` / `proargmodes` / `proargtypes`).
#[derive(Clone, Debug, PartialEq)]
pub struct FuncArg {
    pub name: Option<String>,
    pub ty: Type,
    pub mode: ArgMode,
    pub default: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgMode {
    In,
    Out,
    InOut,
    Variadic,
}

impl ArgMode {
    /// `pg_proc.proargmodes` letter.
    pub fn code(self) -> char {
        match self {
            ArgMode::In => 'i',
            ArgMode::Out => 'o',
            ArgMode::InOut => 'b',
            ArgMode::Variadic => 'v',
        }
    }

    /// Whether callers pass this argument.
    pub fn is_input(self) -> bool {
        !matches!(self, ArgMode::Out)
    }
}

/// What a function returns.
#[derive(Clone, Debug, PartialEq)]
pub enum FuncReturn {
    Scalar(Type),
    /// `RETURNS SETOF type`.
    SetOf(Type),
    /// `RETURNS TABLE (...)`, or OUT parameters of a set-returning function.
    Table(Vec<(String, Type)>),
    /// A procedure.
    None,
}

/// A user function or procedure (`pg_proc`).
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    pub procedure: bool,
    pub args: Vec<FuncArg>,
    pub ret: FuncReturn,
    /// `sql` or `plpgsql`.
    pub language: String,
    /// The function body as written (`prosrc`).
    pub body: String,
    /// `i`, `s` or `v`.
    pub volatility: char,
    pub strict: bool,
    pub security_definer: bool,
    pub leakproof: bool,
    /// `s` (safe), `r` (restricted) or `u` (unsafe, the default).
    pub parallel: char,
    pub cost: f64,
    /// Estimated result rows (`prorows`): 0 for a function that returns no set.
    pub rows: f64,
    /// `proconfig`: `name=value` entries of the function's `SET` options.
    pub config: Vec<String>,
    /// Extensions the function depends on (`ALTER FUNCTION ... DEPENDS ON EXTENSION`):
    /// dropping one drops the function.
    pub extensions: Vec<String>,
}

impl FunctionDef {
    /// The input argument types (what overload resolution matches).
    pub fn input_types(&self) -> Vec<Type> {
        self.args.iter().filter(|a| a.mode.is_input()).map(|a| a.ty.clone()).collect()
    }

    pub fn returns_set(&self) -> bool {
        matches!(self.ret, FuncReturn::SetOf(_) | FuncReturn::Table(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
    InsteadOf,
}

/// A trigger (`pg_trigger`).
#[derive(Clone, Debug, PartialEq)]
pub struct TriggerDef {
    pub oid: Oid,
    pub name: String,
    /// The table or view the trigger is on.
    pub table: Oid,
    pub timing: TriggerTiming,
    pub insert: bool,
    pub delete: bool,
    pub truncate: bool,
    /// `UPDATE [OF columns]`: Some(columns) when the trigger fires on update.
    pub update: Option<Vec<String>>,
    pub for_each_row: bool,
    /// The WHEN condition's SQL text.
    pub when: Option<String>,
    pub function: Oid,
    pub args: Vec<String>,
    /// `pg_trigger.tgenabled`: 'O' fires in origin / local role, 'D' disabled, 'R' fires in
    /// replica role, 'A' always.
    pub enabled: char,
}

/// A named CHECK constraint of a domain; `expr` refers to the value as `VALUE`.
#[derive(Clone, Debug, PartialEq)]
pub struct DomainCheck {
    pub oid: Oid,
    pub name: String,
    pub expr: Expr,
    /// False after `ADD ... NOT VALID` until `VALIDATE CONSTRAINT` (`convalidated`).
    pub validated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypeDefKind {
    /// Labels in sort order.
    Enum {
        labels: Vec<EnumLabel>,
    },
    /// `relid` is the composite type's own `pg_class` entry (relkind `c`), which clients read
    /// the fields from through `typrelid`.
    Composite {
        fields: Vec<Field>,
        relid: Oid,
    },
    Domain {
        base: Type,
        typmod: i32,
        not_null: bool,
        default: Option<Expr>,
        checks: Vec<DomainCheck>,
    },
}

/// A user-defined type (`CREATE TYPE`, `CREATE DOMAIN`).
#[derive(Clone, Debug, PartialEq)]
pub struct TypeDef {
    pub oid: Oid,
    pub array_oid: Oid,
    pub schema: String,
    pub name: String,
    pub kind: TypeDefKind,
}

impl TypeDef {
    /// The I/O-level description installed in the session context.
    pub fn user_type(&self) -> UserType {
        let kind = match &self.kind {
            TypeDefKind::Enum { labels } => UserKind::Enum { labels: labels.clone() },
            TypeDefKind::Composite { fields, relid } => UserKind::Composite { fields: fields.clone(), relid: *relid },
            TypeDefKind::Domain { base, typmod, not_null, .. } => {
                UserKind::Domain { base: base.clone(), typmod: *typmod, not_null: *not_null }
            }
        };
        UserType { oid: self.oid, array_oid: self.array_oid, name: self.name.clone(), schema: self.schema.clone(), kind }
    }

    pub fn as_type(&self) -> Type {
        match self.kind {
            TypeDefKind::Enum { .. } => Type::Enum(self.oid),
            TypeDefKind::Composite { .. } => Type::Composite(self.oid),
            TypeDefKind::Domain { .. } => Type::Domain(self.oid),
        }
    }

    /// `typtype`: e(num), c(omposite), d(omain).
    pub fn typtype(&self) -> char {
        match self.kind {
            TypeDefKind::Enum { .. } => 'e',
            TypeDefKind::Composite { .. } => 'c',
            TypeDefKind::Domain { .. } => 'd',
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            TypeDefKind::Domain { .. } => "domain",
            _ => "type",
        }
    }
}

/// True if `ty` is, or is an array of, the user type `oid`.
pub fn type_mentions(ty: &Type, oid: Oid) -> bool {
    match ty {
        Type::Enum(o) | Type::Domain(o) | Type::Composite(o) => *o == oid,
        Type::Array(e) => type_mentions(e, oid),
        _ => false,
    }
}

/// What a relation name refers to.
#[derive(Clone, Debug)]
pub enum Relation {
    Table(Arc<TableDef>),
    View(Arc<ViewDef>),
    Sequence(Arc<SequenceDef>),
    Index { table: QualName, index: Box<IndexDef> },
}

impl Relation {
    pub fn oid(&self) -> Oid {
        match self {
            Relation::Table(t) => t.oid,
            Relation::View(v) => v.oid,
            Relation::Sequence(s) => s.oid,
            Relation::Index { index, .. } => index.oid,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Relation::Table(t) => &t.name,
            Relation::View(v) => &v.name,
            Relation::Sequence(s) => &s.name,
            Relation::Index { index, .. } => &index.name,
        }
    }

    pub fn schema(&self) -> &str {
        match self {
            Relation::Table(t) => &t.schema,
            Relation::View(v) => &v.schema,
            Relation::Sequence(s) => &s.schema,
            Relation::Index { table, .. } => &table.schema,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Relation::Table(_) => "table",
            Relation::View(_) => "view",
            Relation::Sequence(_) => "sequence",
            Relation::Index { .. } => "index",
        }
    }
}

/// A schema-qualified object name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QualName {
    pub schema: String,
    pub name: String,
}

impl QualName {
    pub fn new(schema: &str, name: &str) -> QualName {
        QualName { schema: schema.to_owned(), name: name.to_owned() }
    }
}

impl std::fmt::Display for QualName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

/// A schema (`pg_namespace` row).
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaDef {
    pub oid: Oid,
    pub name: String,
    pub owner: String,
}

/// Where an unqualified name resolves: user schemas from the catalog, or the virtual
/// `pg_catalog` / `information_schema` relations the executor provides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathEntry {
    /// pg_catalog: system relations, types and functions.
    System,
    Schema(String),
}

/// The set of user objects visible to a transaction.
#[derive(Clone, Debug)]
pub struct Catalog {
    schemas: BTreeMap<String, SchemaDef>,
    tables: BTreeMap<QualName, Arc<TableDef>>,
    views: BTreeMap<QualName, Arc<ViewDef>>,
    sequences: BTreeMap<QualName, Arc<SequenceDef>>,
    types: BTreeMap<QualName, Arc<TypeDef>>,
    /// User functions and procedures by OID (names are overloaded).
    functions: BTreeMap<Oid, Arc<FunctionDef>>,
    /// Triggers by OID.
    triggers: BTreeMap<Oid, Arc<TriggerDef>>,
    /// Roles created with CREATE ROLE (the session user and predefined roles are implicit).
    roles: BTreeMap<String, Arc<RoleDef>>,
    /// (role, member, admin option).
    members: Vec<(String, String, bool)>,
    grants: Vec<Grant>,
    default_privileges: Vec<DefaultPrivilege>,
    /// Object owners other than the bootstrap superuser, by OID.
    owners: BTreeMap<Oid, String>,
    /// Installed extensions (plpgsql is implicit).
    extensions: BTreeMap<String, ExtensionDef>,
    /// `COMMENT ON` text by (classoid, objoid, objsubid), as in `pg_description`.
    comments: BTreeMap<(Oid, Oid, i32), String>,
    /// Partition keys of partitioned tables, by table OID.
    partition_keys: BTreeMap<Oid, PartitionKey>,
    /// Partitions' parents and bounds, by partition OID.
    partition_bounds: BTreeMap<Oid, PartitionOf>,
    next_oid: Oid,
}

impl Default for Catalog {
    fn default() -> Catalog {
        Catalog::new()
    }
}

/// Schemas the catalog knows without them being created: their objects are virtual.
pub fn is_system_schema(name: &str) -> bool {
    name == PG_CATALOG || name == INFORMATION_SCHEMA
}

impl Catalog {
    pub fn new() -> Catalog {
        let mut schemas = BTreeMap::new();
        schemas.insert(
            PUBLIC_SCHEMA.to_owned(),
            SchemaDef { oid: PUBLIC_NAMESPACE_OID, name: PUBLIC_SCHEMA.to_owned(), owner: "pg_database_owner".to_owned() },
        );
        Catalog {
            schemas,
            tables: BTreeMap::new(),
            views: BTreeMap::new(),
            sequences: BTreeMap::new(),
            types: BTreeMap::new(),
            functions: BTreeMap::new(),
            triggers: BTreeMap::new(),
            roles: BTreeMap::new(),
            members: Vec::new(),
            grants: Vec::new(),
            default_privileges: Vec::new(),
            owners: BTreeMap::new(),
            extensions: BTreeMap::new(),
            comments: BTreeMap::new(),
            partition_keys: BTreeMap::new(),
            partition_bounds: BTreeMap::new(),
            next_oid: FIRST_USER_OID,
        }
    }

    pub fn alloc_oid(&mut self) -> Oid {
        if self.next_oid < FIRST_USER_OID {
            self.next_oid = FIRST_USER_OID;
        }
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }

    // -----------------------------------------------------------------------------------------
    // Schemas and name resolution
    // -----------------------------------------------------------------------------------------

    /// User schemas (public and created ones), by name.
    pub fn schemas(&self) -> impl Iterator<Item = &SchemaDef> {
        self.schemas.values()
    }

    pub fn schema(&self, name: &str) -> Option<&SchemaDef> {
        self.schemas.get(name)
    }

    pub fn schema_by_oid(&self, oid: Oid) -> Option<&SchemaDef> {
        self.schemas.values().find(|s| s.oid == oid)
    }

    /// The namespace OID of a schema name (system schemas included).
    pub fn namespace_oid(&self, name: &str) -> Option<Oid> {
        match name {
            PG_CATALOG => Some(PG_CATALOG_NAMESPACE_OID),
            INFORMATION_SCHEMA => Some(INFORMATION_SCHEMA_NAMESPACE_OID),
            "pg_toast" => Some(PG_TOAST_NAMESPACE_OID),
            _ => self.schemas.get(name).map(|s| s.oid),
        }
    }

    /// True if `name` is a schema: a user schema or a system one.
    pub fn schema_exists(&self, name: &str) -> bool {
        self.namespace_oid(name).is_some()
    }

    pub fn create_schema(&mut self, def: SchemaDef) -> SqlResult<()> {
        if self.schema_exists(&def.name) {
            return Err(SqlError::new(code::DUPLICATE_SCHEMA, format!("schema \"{}\" already exists", def.name)));
        }
        self.schemas.insert(def.name.clone(), def);
        Ok(())
    }

    /// Removes a schema (its objects must already be gone).
    pub fn drop_schema(&mut self, name: &str) -> Option<SchemaDef> {
        self.schemas.remove(name)
    }

    /// Renames a schema and moves every object in it.
    pub fn rename_schema(&mut self, old: &str, new: &str) -> SqlResult<()> {
        if self.schema_exists(new) {
            return Err(SqlError::new(code::DUPLICATE_SCHEMA, format!("schema \"{new}\" already exists")));
        }
        let mut def = self.schemas.remove(old).ok_or_else(|| undefined_schema(old))?;
        def.name = new.to_owned();
        self.schemas.insert(new.to_owned(), def);
        fn moved<T: Clone>(map: &mut BTreeMap<QualName, Arc<T>>, old: &str, new: &str, set: impl Fn(&mut T)) {
            let keys: Vec<QualName> = map.keys().filter(|k| k.schema == old).cloned().collect();
            for k in keys {
                if let Some(v) = map.remove(&k) {
                    let mut v = (*v).clone();
                    set(&mut v);
                    map.insert(QualName::new(new, &k.name), Arc::new(v));
                }
            }
        }
        moved(&mut self.tables, old, new, |t: &mut TableDef| t.schema = new.to_owned());
        moved(&mut self.views, old, new, |t: &mut ViewDef| t.schema = new.to_owned());
        moved(&mut self.sequences, old, new, |t: &mut SequenceDef| t.schema = new.to_owned());
        moved(&mut self.types, old, new, |t: &mut TypeDef| t.schema = new.to_owned());
        // Foreign keys name their referenced table by schema.
        for t in self.tables.values_mut() {
            if t.constraints.iter().any(|c| matches!(&c.kind, ConstraintKind::ForeignKey { ref_table, .. } if ref_table.schema == old)) {
                let def = Arc::make_mut(t);
                for c in &mut def.constraints {
                    if let ConstraintKind::ForeignKey { ref_table, .. } = &mut c.kind
                        && ref_table.schema == old
                    {
                        ref_table.schema = new.to_owned();
                    }
                }
            }
        }
        Ok(())
    }

    /// True if anything lives in the schema.
    pub fn schema_is_empty(&self, name: &str) -> bool {
        !self.tables.keys().chain(self.views.keys()).chain(self.sequences.keys()).chain(self.types.keys()).any(|k| k.schema == name)
    }

    /// The effective search path (`recomputeNamespacePath`): the session's temporary schema
    /// first (for relations and types), pg_catalog first unless listed, `$user` and
    /// `pg_temp` substituted, schemas that don't exist skipped.
    pub fn search_path(&self) -> Vec<PathEntry> {
        let ctx = pgmem_types::context::search_path();
        let mut out = Vec::new();
        for entry in &ctx.entries {
            let name = match entry.as_str() {
                "$user" => ctx.user.clone(),
                "pg_temp" => match &ctx.temp_schema {
                    Some(t) => t.clone(),
                    None => continue,
                },
                other => other.to_owned(),
            };
            let e = if name == PG_CATALOG {
                PathEntry::System
            } else if self.schemas.contains_key(&name) {
                PathEntry::Schema(name)
            } else {
                continue;
            };
            if !out.contains(&e) {
                out.push(e);
            }
        }
        if !out.contains(&PathEntry::System) {
            out.insert(0, PathEntry::System);
        }
        if let Some(t) = &ctx.temp_schema
            && self.schemas.contains_key(t)
        {
            let e = PathEntry::Schema(t.clone());
            out.retain(|x| *x != e);
            out.insert(0, e);
        }
        out
    }

    /// The effective search path as schema names (pg_catalog included), to resolve names
    /// later exactly as now ([`ViewDef::search_path`]).
    pub fn resolved_search_path(&self) -> Vec<String> {
        self.search_path()
            .into_iter()
            .map(|e| match e {
                PathEntry::System => PG_CATALOG.to_owned(),
                PathEntry::Schema(s) => s,
            })
            .collect()
    }

    /// The explicit path entries that exist (`current_schemas(false)`): no implicit
    /// pg_catalog or temporary schema.
    pub fn explicit_search_path(&self) -> Vec<String> {
        let ctx = pgmem_types::context::search_path();
        let mut out: Vec<String> = Vec::new();
        for entry in &ctx.entries {
            let name = match entry.as_str() {
                "$user" => ctx.user.clone(),
                "pg_temp" => continue,
                other => other.to_owned(),
            };
            if self.schema_exists(&name) && !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// The schema a new object named `name` is created in: its qualifier (which must
    /// exist), else the first existing schema on the search path (3F000 if there is none).
    pub fn creation_schema(&self, name: &pgmem_ir::ObjectName) -> SqlResult<String> {
        match name.0.as_slice() {
            [_] => {
                let ctx = pgmem_types::context::search_path();
                for entry in &ctx.entries {
                    let n = match entry.as_str() {
                        "$user" => ctx.user.clone(),
                        "pg_temp" => match &ctx.temp_schema {
                            Some(t) => t.clone(),
                            None => continue,
                        },
                        other => other.to_owned(),
                    };
                    if n == PG_CATALOG {
                        return Err(system_catalog_denied(name.base()));
                    }
                    if self.schemas.contains_key(&n) {
                        return Ok(n);
                    }
                }
                Err(SqlError::new(code::INVALID_SCHEMA_NAME, "no schema has been selected to create in"))
            }
            [schema, _] | [_, schema, _] => self.target_schema(schema, name.base()),
            _ => Err(SqlError::syntax(format!("improper qualified name (too many dotted names): {name}"))),
        }
    }

    /// The schema named by an explicit qualifier for a new object (`pg_temp` is the session's
    /// temporary schema).
    pub fn target_schema(&self, schema: &str, object: &str) -> SqlResult<String> {
        let schema = self.resolve_schema_name(schema);
        if is_system_schema(&schema) {
            return Err(system_catalog_denied(object));
        }
        if !self.schemas.contains_key(&schema) {
            return Err(undefined_schema(&schema));
        }
        Ok(schema)
    }

    /// `pg_temp` stands for the session's temporary schema.
    pub fn resolve_schema_name(&self, schema: &str) -> String {
        if schema == "pg_temp" {
            pgmem_types::context::search_path().temp_schema.clone().unwrap_or_else(|| "pg_temp".to_owned())
        } else {
            schema.to_owned()
        }
    }

    /// Resolves a possibly qualified name with `find` (which looks in one schema), the way
    /// Postgres' `RangeVarGetRelid` does: a qualified name looks only in its schema (3F000 if
    /// it doesn't exist), an unqualified one walks the search path. `System` entries are
    /// passed to `find` as `pg_catalog`.
    pub fn resolve<T>(&self, name: &pgmem_ir::ObjectName, find: impl Fn(&str, &str) -> Option<T>) -> SqlResult<Option<T>> {
        match name.0.as_slice() {
            [n] => {
                for e in self.search_path() {
                    let schema = match &e {
                        PathEntry::System => PG_CATALOG,
                        PathEntry::Schema(s) => s.as_str(),
                    };
                    if let Some(found) = find(schema, n) {
                        return Ok(Some(found));
                    }
                }
                Ok(None)
            }
            [schema, n] | [_, schema, n] => {
                let schema = self.resolve_schema_name(schema);
                if !self.schema_exists(&schema) {
                    return Err(undefined_schema(&schema));
                }
                Ok(find(&schema, n))
            }
            _ => Err(SqlError::syntax(format!("improper qualified name (too many dotted names): {name}"))),
        }
    }

    /// Looks up a relation by possibly qualified name through the search path.
    pub fn lookup_relation(&self, name: &pgmem_ir::ObjectName) -> SqlResult<Option<Relation>> {
        self.resolve(name, |schema, n| self.relation(schema, n))
    }

    /// [`Catalog::lookup_relation`] as queries resolve names: a qualifier naming no schema
    /// just finds nothing (Postgres reports the relation, not the schema, as missing).
    pub fn find_relation(&self, name: &pgmem_ir::ObjectName) -> SqlResult<Option<Relation>> {
        match self.lookup_relation(name) {
            Err(e) if e.code == code::INVALID_SCHEMA_NAME => Ok(None),
            other => other,
        }
    }

    /// Like [`Catalog::lookup_relation`] for a table, with Postgres' errors.
    pub fn lookup_table(&self, name: &pgmem_ir::ObjectName) -> SqlResult<Arc<TableDef>> {
        match self.lookup_relation(name)? {
            Some(Relation::Table(t)) => Ok(t),
            Some(other) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", other.name()))),
            None => Err(SqlError::undefined_table(&name.to_string())),
        }
    }

    pub fn lookup_type(&self, name: &pgmem_ir::ObjectName) -> SqlResult<Option<Arc<TypeDef>>> {
        self.resolve(name, |schema, n| self.types.get(&QualName::new(schema, n)).cloned())
    }

    /// True if an unqualified reference to `name` in `schema` would find it (Postgres'
    /// `RelationIsVisible`): object names print qualified otherwise.
    pub fn relation_visible(&self, schema: &str, name: &str) -> bool {
        for e in self.search_path() {
            match e {
                PathEntry::System => {}
                PathEntry::Schema(s) => {
                    if self.relation(&s, name).is_some() {
                        return s == schema;
                    }
                }
            }
        }
        false
    }

    /// ruleutils' `FunctionIsVisible` for a function `f`: the first schema on the search path
    /// with a function of that name and argument types holds `f`.
    pub fn function_visible(&self, f: &FunctionDef) -> bool {
        let types = f.input_types();
        for e in self.search_path() {
            if let PathEntry::Schema(s) = e
                && self.functions.values().any(|g| g.schema == s && g.name == f.name && g.input_types() == types)
            {
                return s == f.schema;
            }
        }
        false
    }

    /// Like [`Catalog::relation_visible`] for types.
    pub fn type_visible(&self, schema: &str, name: &str) -> bool {
        for e in self.search_path() {
            if let PathEntry::Schema(s) = e
                && self.types.contains_key(&QualName::new(&s, name))
            {
                return s == schema;
            }
        }
        false
    }

    // -----------------------------------------------------------------------------------------
    // Relations
    // -----------------------------------------------------------------------------------------

    pub fn table(&self, schema: &str, name: &str) -> Option<&Arc<TableDef>> {
        self.tables.get(&QualName::new(schema, name))
    }

    pub fn table_q(&self, q: &QualName) -> Option<&Arc<TableDef>> {
        self.tables.get(q)
    }

    pub fn table_by_oid(&self, oid: Oid) -> Option<&Arc<TableDef>> {
        self.tables.values().find(|t| t.oid == oid)
    }

    pub fn view(&self, schema: &str, name: &str) -> Option<&Arc<ViewDef>> {
        self.views.get(&QualName::new(schema, name))
    }

    pub fn sequence(&self, schema: &str, name: &str) -> Option<&Arc<SequenceDef>> {
        self.sequences.get(&QualName::new(schema, name))
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

    /// Sequences OWNED BY a column of `table`.
    pub fn owned_sequences<'a>(&'a self, table: &'a TableDef) -> impl Iterator<Item = &'a Arc<SequenceDef>> {
        self.sequences.values().filter(move |s| s.schema == table.schema && s.owned_by.as_ref().is_some_and(|(t, _)| *t == table.name))
    }

    /// Looks up any relation (table, view, sequence or index) in a schema; they share one
    /// namespace in Postgres (`pg_class`).
    pub fn relation(&self, schema: &str, name: &str) -> Option<Relation> {
        let key = QualName::new(schema, name);
        if let Some(t) = self.tables.get(&key) {
            return Some(Relation::Table(Arc::clone(t)));
        }
        if let Some(v) = self.views.get(&key) {
            return Some(Relation::View(Arc::clone(v)));
        }
        if let Some(s) = self.sequences.get(&key) {
            return Some(Relation::Sequence(Arc::clone(s)));
        }
        self.tables.values().filter(|t| t.schema == schema).find_map(|t| {
            t.indexes.iter().find(|i| i.name == name).map(|i| Relation::Index { table: t.qualified(), index: Box::new(i.clone()) })
        })
    }

    /// Sets (or with `None` removes) the comment on an object.
    pub fn set_comment(&mut self, classoid: Oid, objoid: Oid, subid: i32, text: Option<String>) {
        match text {
            Some(t) => {
                self.comments.insert((classoid, objoid, subid), t);
            }
            None => {
                self.comments.remove(&(classoid, objoid, subid));
            }
        }
    }

    /// Comments on objects that still exist: `(classoid, objoid, objsubid) → text`.
    pub fn comments(&self) -> impl Iterator<Item = (&(Oid, Oid, i32), &String)> {
        self.comments.iter()
    }

    pub fn comment(&self, classoid: Oid, objoid: Oid, subid: i32) -> Option<&str> {
        self.comments.get(&(classoid, objoid, subid)).map(String::as_str)
    }

    /// Drops the comments on an object and its columns (when the object is dropped).
    pub fn drop_comments(&mut self, objoid: Oid) {
        self.comments.retain(|(_, o, _), _| *o != objoid);
    }

    /// The relation (table, view, sequence or index) with this OID.
    pub fn relation_by_oid(&self, oid: Oid) -> Option<Relation> {
        let t = self.tables.values().find(|t| t.oid == oid).map(|t| Relation::Table(Arc::clone(t)));
        t.or_else(|| self.views.values().find(|v| v.oid == oid).map(|v| Relation::View(Arc::clone(v))))
            .or_else(|| self.sequences.values().find(|s| s.oid == oid).map(|s| Relation::Sequence(Arc::clone(s))))
            .or_else(|| {
                self.tables.values().find_map(|t| {
                    t.indexes.iter().find(|i| i.oid == oid).map(|i| Relation::Index { table: t.qualified(), index: Box::new(i.clone()) })
                })
            })
    }

    fn ensure_free(&self, schema: &str, name: &str) -> SqlResult<()> {
        // A composite type has a pg_class entry, so it takes a relation name too.
        let composite = self.types.get(&QualName::new(schema, name)).is_some_and(|t| matches!(t.kind, TypeDefKind::Composite { .. }));
        if self.relation(schema, name).is_some() || composite {
            return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{name}\" already exists")));
        }
        Ok(())
    }

    pub fn type_by_name(&self, schema: &str, name: &str) -> Option<&Arc<TypeDef>> {
        self.types.get(&QualName::new(schema, name))
    }

    pub fn type_by_oid(&self, oid: Oid) -> Option<&Arc<TypeDef>> {
        self.types.values().find(|t| t.oid == oid || t.array_oid == oid)
    }

    pub fn types(&self) -> impl Iterator<Item = &Arc<TypeDef>> {
        self.types.values()
    }

    pub fn functions(&self) -> impl Iterator<Item = &Arc<FunctionDef>> {
        self.functions.values()
    }

    pub fn function_by_oid(&self, oid: Oid) -> Option<&Arc<FunctionDef>> {
        self.functions.get(&oid)
    }

    /// Functions named `name` (schema-qualified, or the first schema on the search path that
    /// has any).
    pub fn find_functions(&self, name: &pgmem_ir::ObjectName) -> Vec<Arc<FunctionDef>> {
        let base = name.base();
        let in_schema =
            |s: &str| -> Vec<Arc<FunctionDef>> { self.functions.values().filter(|f| f.schema == s && f.name == base).cloned().collect() };
        if name.0.len() > 1 {
            return in_schema(&self.resolve_schema_name(&name.0[name.0.len() - 2]));
        }
        for e in self.search_path() {
            if let PathEntry::Schema(s) = e {
                let found = in_schema(&s);
                if !found.is_empty() {
                    return found;
                }
            }
        }
        Vec::new()
    }

    /// Adds or replaces (same OID) a function.
    pub fn put_function(&mut self, def: FunctionDef) {
        self.functions.insert(def.oid, Arc::new(def));
    }

    pub fn drop_function(&mut self, oid: Oid) -> Option<Arc<FunctionDef>> {
        self.triggers.retain(|_, t| t.function != oid);
        self.grants.retain(|g| g.object != GrantObject::Function(oid));
        self.owners.remove(&oid);
        self.functions.remove(&oid)
    }

    pub fn triggers(&self) -> impl Iterator<Item = &Arc<TriggerDef>> {
        self.triggers.values()
    }

    /// Triggers of table `oid`, in firing order (by name, as Postgres fires them).
    pub fn table_triggers(&self, oid: Oid) -> Vec<Arc<TriggerDef>> {
        let mut out: Vec<Arc<TriggerDef>> = self.triggers.values().filter(|t| t.table == oid).cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn put_trigger(&mut self, def: TriggerDef) {
        self.triggers.insert(def.oid, Arc::new(def));
    }

    pub fn drop_trigger(&mut self, oid: Oid) -> Option<Arc<TriggerDef>> {
        self.triggers.remove(&oid)
    }

    /// Types share `pg_type`'s namespace with the row types of tables and views.
    fn ensure_type_name_free(&self, schema: &str, name: &str) -> SqlResult<()> {
        let key = QualName::new(schema, name);
        let taken = self.types.contains_key(&key)
            || self.tables.contains_key(&key)
            || self.views.contains_key(&key)
            || self.types.values().any(|t| t.schema == schema && format!("_{}", t.name) == name);
        if taken {
            return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("type \"{name}\" already exists")));
        }
        Ok(())
    }

    pub fn create_type(&mut self, def: TypeDef) -> SqlResult<()> {
        self.ensure_type_name_free(&def.schema, &def.name)?;
        self.types.insert(QualName::new(&def.schema, &def.name), Arc::new(def));
        Ok(())
    }

    /// Replaces a type definition (ALTER TYPE / ALTER DOMAIN); the name may change.
    pub fn replace_type(&mut self, old: &QualName, def: TypeDef) {
        self.types.remove(old);
        self.types.insert(QualName::new(&def.schema, &def.name), Arc::new(def));
    }

    pub fn drop_type(&mut self, name: &QualName) -> Option<Arc<TypeDef>> {
        self.types.remove(name)
    }

    /// Fails like Postgres when a new relation's row type would collide with a type.
    fn ensure_rowtype_free(&self, schema: &str, name: &str) -> SqlResult<()> {
        if self.types.contains_key(&QualName::new(schema, name)) {
            return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("type \"{name}\" already exists")).with_hint(
                "A relation has an associated type of the same name, so you must use a name that doesn't conflict with any existing type.",
            ));
        }
        Ok(())
    }

    pub fn create_table(&mut self, def: TableDef) -> SqlResult<()> {
        self.ensure_free(&def.schema, &def.name)?;
        self.ensure_rowtype_free(&def.schema, &def.name)?;
        for idx in &def.indexes {
            self.ensure_free(&def.schema, &idx.name)?;
        }
        self.tables.insert(def.qualified(), Arc::new(def));
        Ok(())
    }

    /// Replaces an existing table definition (ALTER TABLE). The name may change (RENAME).
    pub fn replace_table(&mut self, old: &QualName, def: TableDef) {
        self.tables.remove(old);
        self.tables.insert(def.qualified(), Arc::new(def));
    }

    pub fn drop_table(&mut self, name: &QualName) -> Option<Arc<TableDef>> {
        let dropped = self.tables.remove(name)?;
        self.partition_keys.remove(&dropped.oid);
        self.partition_bounds.remove(&dropped.oid);
        self.triggers.retain(|_, t| t.table != dropped.oid);
        self.grants.retain(|g| g.object.relation_oid() != Some(dropped.oid));
        self.owners.remove(&dropped.oid);
        // Sequences owned by the table's columns go with it.
        self.sequences.retain(|_, s| s.schema != name.schema || s.owned_by.as_ref().is_none_or(|(t, _)| *t != name.name));
        Some(dropped)
    }

    pub fn create_view(&mut self, def: ViewDef, or_replace: bool) -> SqlResult<()> {
        let key = QualName::new(&def.schema, &def.name);
        if or_replace && self.views.contains_key(&key) {
            self.views.insert(key, Arc::new(def));
            return Ok(());
        }
        self.ensure_free(&def.schema, &def.name)?;
        self.ensure_rowtype_free(&def.schema, &def.name)?;
        self.views.insert(key, Arc::new(def));
        Ok(())
    }

    pub fn drop_view(&mut self, name: &QualName) -> Option<Arc<ViewDef>> {
        let dropped = self.views.remove(name)?;
        self.triggers.retain(|_, t| t.table != dropped.oid);
        self.grants.retain(|g| g.object.relation_oid() != Some(dropped.oid));
        self.owners.remove(&dropped.oid);
        Some(dropped)
    }

    pub fn create_sequence(&mut self, def: SequenceDef) -> SqlResult<()> {
        self.ensure_free(&def.schema, &def.name)?;
        self.sequences.insert(QualName::new(&def.schema, &def.name), Arc::new(def));
        Ok(())
    }

    /// Replaces a sequence definition (ALTER SEQUENCE, OWNED BY); the name may change.
    pub fn replace_sequence(&mut self, old: &QualName, def: SequenceDef) {
        self.sequences.remove(old);
        self.sequences.insert(QualName::new(&def.schema, &def.name), Arc::new(def));
    }

    pub fn drop_sequence(&mut self, name: &QualName) -> Option<Arc<SequenceDef>> {
        self.sequences.remove(name)
    }

    /// Picks an unused relation name in `schema` the way Postgres' `ChooseRelationName` does:
    /// `base`, then `base1`, `base2`, ...
    pub fn choose_name(&self, schema: &str, base: &str, taken: &[String]) -> String {
        let free = |n: &str| self.relation(schema, n).is_none() && !taken.iter().any(|t| t == n);
        if free(base) {
            return base.to_owned();
        }
        (1..).map(|i| format!("{base}{i}")).find(|n| free(n)).unwrap_or_else(|| base.to_owned())
    }
}

pub fn undefined_schema(name: &str) -> SqlError {
    SqlError::new(code::INVALID_SCHEMA_NAME, format!("schema \"{name}\" does not exist"))
}

fn system_catalog_denied(object: &str) -> SqlError {
    SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied to create \"pg_catalog.{object}\""))
        .with_detail("System catalog modifications are currently disallowed.")
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
        TableDef {
            oid: 0,
            schema: PUBLIC_SCHEMA.into(),
            name: name.into(),
            columns: vec![ColumnDef::new("id", Type::Int4)],
            constraints: vec![],
            indexes: vec![],
            matview: None,
            unlogged: false,
            reloptions: Vec::new(),
        }
    }

    #[test]
    fn relations_share_a_namespace() {
        let mut cat = Catalog::new();
        cat.create_table(table("users")).expect("create");
        let err = cat.create_table(table("users")).expect_err("dup");
        assert_eq!(err.code, "42P07");
        assert_eq!(err.message, "relation \"users\" already exists");
        assert!(matches!(cat.relation("public", "users"), Some(Relation::Table(_))));
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
        assert_eq!(cat.choose_name("public", "t_pkey", &[]), "t_pkey1");
    }
}
