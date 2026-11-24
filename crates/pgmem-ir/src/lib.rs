//! The dialect-neutral statement tree. Parsers (today `sqlparser`; an opt-in `pg_query`
//! front end later) lower their own ASTs into these types, and the binder in `pgmem-exec`
//! resolves names and types against the catalog. Nothing here knows about the catalog.
//!
//! Identifiers are already case-normalized: unquoted names are lower-cased, quoted names keep
//! their case, exactly as Postgres folds them.
//!
//! Postgres' grammar rewrites several special forms into plain function calls, and so do the
//! lowerings: `EXTRACT(f FROM x)` is `extract('f', x)`, `POSITION(a IN b)` is `position(a, b)`,
//! `SUBSTRING(x FROM a FOR b)` is `substring(x, a, b)`, `TRIM(BOTH c FROM x)` is `btrim(x, c)`.

/// A possibly schema-qualified name (`public.users` → `["public", "users"]`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjectName(pub Vec<String>);

impl ObjectName {
    pub fn simple(name: impl Into<String>) -> ObjectName {
        ObjectName(vec![name.into()])
    }

    /// The unqualified (last) part.
    pub fn base(&self) -> &str {
        self.0.last().map_or("", String::as_str)
    }

    /// The schema part, when qualified.
    pub fn schema(&self) -> Option<&str> {
        if self.0.len() >= 2 { Some(self.0[self.0.len() - 2].as_str()) } else { None }
    }
}

impl std::fmt::Display for ObjectName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join("."))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    Query(Box<Query>),
    Insert(Box<Insert>),
    Update(Box<Update>),
    Delete(Box<Delete>),
    CreateTable(Box<CreateTable>),
    CreateView(Box<CreateView>),
    CreateIndex(Box<CreateIndex>),
    CreateSequence(Box<CreateSequence>),
    AlterTable(Box<AlterTable>),
    Drop(Box<Drop>),
    Truncate(Box<Truncate>),
    Transaction(TransactionStmt),
    /// `SET [SESSION|LOCAL] name = value` / `SET name TO value` / `SET TIME ZONE ...`.
    Set {
        name: String,
        value: Vec<String>,
        local: bool,
    },
    /// `RESET name` / `RESET ALL` (`name = None`).
    Reset {
        name: Option<String>,
    },
    /// `SHOW name`.
    Show {
        name: String,
    },
    /// `COMMENT ON object name IS 'text' | NULL`.
    Comment {
        target: CommentTarget,
        /// The object's name; for a column, the table name followed by the column name.
        name: ObjectName,
        text: Option<String>,
    },
    /// Statements accepted for client and migration-tool compatibility that have no effect in
    /// pgmem (`COMMENT ON`, `CREATE EXTENSION`, `ANALYZE`, `VACUUM`, `DISCARD ALL`, ...).
    /// `tag` is the CommandComplete tag Postgres sends.
    NoOp {
        tag: String,
    },
    /// Statements with no SQL content (`;` alone): Postgres answers EmptyQueryResponse.
    Empty,
}

/// What a `COMMENT ON` names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommentTarget {
    Table,
    View,
    MaterializedView,
    Sequence,
    Index,
    Column,
    Schema,
    /// Object kinds pgmem has no catalog entry for (functions, types, extensions, roles,
    /// databases, ...): accepted without effect.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionStmt {
    Begin { isolation: Option<IsolationLevel>, read_only: Option<bool> },
    Commit { chain: bool },
    Rollback { chain: bool },
    Savepoint(String),
    Release(String),
    RollbackTo(String),
    SetTransaction { isolation: Option<IsolationLevel>, read_only: Option<bool> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

// ---------------------------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub with: Option<With>,
    pub body: SetExpr,
    pub order_by: Vec<OrderByExpr>,
    /// `None` = no LIMIT (or `LIMIT ALL`).
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    /// `FOR UPDATE` / `FOR SHARE` ... (accepted; pgmem's snapshot model makes them no-ops).
    pub locking: Vec<LockClause>,
}

impl Query {
    pub fn from_body(body: SetExpr) -> Query {
        Query { with: None, body, order_by: Vec::new(), limit: None, offset: None, locking: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockClause {
    pub strength: LockStrength,
    pub of: Vec<ObjectName>,
    pub nowait: bool,
    pub skip_locked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockStrength {
    Update,
    NoKeyUpdate,
    Share,
    KeyShare,
}

#[derive(Clone, Debug, PartialEq)]
pub struct With {
    pub recursive: bool,
    pub ctes: Vec<Cte>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub query: Box<Query>,
    pub materialized: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SetExpr {
    Select(Box<Select>),
    Values(Vec<Vec<Expr>>),
    SetOp {
        op: SetOperator,
        all: bool,
        left: Box<SetExpr>,
        right: Box<SetExpr>,
    },
    /// A parenthesized query used as a set operand (may carry its own ORDER BY / LIMIT).
    Query(Box<Query>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOperator {
    Union,
    Intersect,
    Except,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub distinct: Distinct,
    pub projection: Vec<SelectItem>,
    pub from: Vec<TableRef>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub windows: Vec<NamedWindow>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum Distinct {
    #[default]
    All,
    Distinct,
    On(Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SelectItem {
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
    /// `*`
    Wildcard,
    /// `t.*`
    QualifiedWildcard(ObjectName),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableAlias {
    pub name: String,
    pub columns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TableRef {
    Table {
        name: ObjectName,
        alias: Option<TableAlias>,
    },
    Subquery {
        query: Box<Query>,
        alias: Option<TableAlias>,
        lateral: bool,
    },
    /// A set-returning function in FROM (`generate_series(1, 3) AS g(n)`, `unnest($1)`).
    Function {
        call: FunctionCall,
        alias: Option<TableAlias>,
        lateral: bool,
        with_ordinality: bool,
    },
    Join {
        left: Box<TableRef>,
        right: Box<TableRef>,
        kind: JoinKind,
        constraint: JoinConstraint,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Clone, Debug, PartialEq)]
pub enum JoinConstraint {
    On(Expr),
    Using(Vec<String>),
    Natural,
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderByExpr {
    pub expr: Expr,
    pub desc: bool,
    /// `None` = default (NULLS LAST for ASC, NULLS FIRST for DESC).
    pub nulls_first: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NamedWindow {
    pub name: String,
    pub spec: WindowSpec,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct WindowSpec {
    /// `OVER (w ...)` refers to a named window it extends.
    pub base: Option<String>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderByExpr>,
    pub frame: Option<WindowFrame>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowFrame {
    pub units: FrameUnits,
    pub start: FrameBound,
    pub end: FrameBound,
    pub exclude: FrameExclude,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUnits {
    Rows,
    Range,
    Groups,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Box<Expr>),
    CurrentRow,
    Following(Box<Expr>),
    UnboundedFollowing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FrameExclude {
    #[default]
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

// ---------------------------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    /// An integer literal's text (typed int4 / int8 / numeric by magnitude in the binder).
    Integer(String),
    /// A decimal or exponent literal's text (Postgres types these as numeric).
    Decimal(String),
    /// A string constant: type `unknown` until context resolves it.
    String(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypeName {
    /// Lower-cased base type name as written (`int4`, `varchar`, `double precision`, ...).
    pub name: String,
    /// Type modifiers (`varchar(20)` → `[20]`, `numeric(10, 2)` → `[10, 2]`).
    pub modifiers: Vec<i64>,
    /// Number of `[]` array dimensions.
    pub array_dims: usize,
}

impl TypeName {
    pub fn simple(name: impl Into<String>) -> TypeName {
        TypeName { name: name.into(), modifiers: Vec::new(), array_dims: 0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Literal),
    /// A column reference, possibly qualified (`id`, `u.id`, `public.users.id`).
    Column(Vec<String>),
    /// `$n` (1-based, as written).
    Param(usize),
    /// A typed constant: `interval '1 day'`, `timestamptz '...'`, `'...'::type` folded early
    /// is NOT done here — those stay `Cast`. This is only the `type 'literal'` syntax.
    TypedString {
        ty: TypeName,
        value: String,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    /// A binary operator by its Postgres name (`=`, `<>`, `+`, `||`, `->>`, `@>`, `~~`, ...).
    Binary {
        op: String,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `IS [NOT] TRUE / FALSE / UNKNOWN` (`value = None` is UNKNOWN).
    IsBool {
        expr: Box<Expr>,
        value: Option<bool>,
        negated: bool,
    },
    IsDistinctFrom {
        left: Box<Expr>,
        right: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
        symmetric: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        query: Box<Query>,
        negated: bool,
    },
    Exists {
        query: Box<Query>,
        negated: bool,
    },
    /// A scalar subquery `(SELECT ...)`.
    Subquery(Box<Query>),
    /// `left op ANY|SOME|ALL (array_expr)` or `(subquery)`.
    AnyAll {
        left: Box<Expr>,
        op: String,
        right: AnyAllRight,
        all: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        escape: Option<Box<Expr>>,
        negated: bool,
        case_insensitive: bool,
    },
    SimilarTo {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        escape: Option<Box<Expr>>,
        negated: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        ty: TypeName,
    },
    Function(Box<FunctionCall>),
    /// `ARRAY[a, b, c]`.
    Array(Vec<Expr>),
    /// `ARRAY(SELECT ...)`.
    ArraySubquery(Box<Query>),
    /// `expr[index]` (1-based) or `expr[lower:upper]`.
    Subscript {
        expr: Box<Expr>,
        index: Subscript,
    },
    /// `ROW(a, b)` / `(a, b)`.
    Row(Vec<Expr>),
    /// `expr COLLATE "C"` (collations are accepted and ignored: pgmem compares bytewise).
    Collate {
        expr: Box<Expr>,
        collation: String,
    },
    /// `expr AT TIME ZONE zone`.
    AtTimeZone {
        expr: Box<Expr>,
        zone: Box<Expr>,
    },
    /// The bare `DEFAULT` keyword in an INSERT VALUES row or an UPDATE SET.
    Default,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AnyAllRight {
    Array(Box<Expr>),
    Subquery(Box<Query>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Subscript {
    Index(Box<Expr>),
    Slice { lower: Option<Box<Expr>>, upper: Option<Box<Expr>> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Minus,
    Plus,
    /// `~` bitwise not.
    BitNot,
    /// `|/` square root, `||/` cube root, `@` absolute value.
    SquareRoot,
    CubeRoot,
    Abs,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionCall {
    pub name: ObjectName,
    pub args: Vec<Expr>,
    /// `count(*)`.
    pub star: bool,
    pub distinct: bool,
    /// `agg(x ORDER BY y)`.
    pub order_by: Vec<OrderByExpr>,
    pub filter: Option<Box<Expr>>,
    pub over: Option<WindowSpec>,
    /// `percentile_cont(0.5) WITHIN GROUP (ORDER BY x)`.
    pub within_group: Vec<OrderByExpr>,
    /// `VARIADIC` on the last argument.
    pub variadic: bool,
}

impl FunctionCall {
    pub fn new(name: &str, args: Vec<Expr>) -> FunctionCall {
        FunctionCall {
            name: ObjectName::simple(name),
            args,
            star: false,
            distinct: false,
            order_by: Vec::new(),
            filter: None,
            over: None,
            within_group: Vec::new(),
            variadic: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// DML
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    pub with: Option<With>,
    pub table: ObjectName,
    pub alias: Option<String>,
    pub columns: Vec<String>,
    pub source: InsertSource,
    /// `OVERRIDING SYSTEM VALUE` lets explicit values into `GENERATED ALWAYS AS IDENTITY`.
    pub overriding_system_value: bool,
    pub on_conflict: Option<OnConflict>,
    pub returning: Vec<SelectItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InsertSource {
    /// `VALUES (...), (...)` — rows may contain `Expr::Default`.
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
    DefaultValues,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OnConflict {
    pub target: ConflictTarget,
    pub action: ConflictAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConflictTarget {
    None,
    Columns { columns: Vec<String>, selection: Option<Expr> },
    Constraint(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConflictAction {
    Nothing,
    Update { assignments: Vec<Assignment>, selection: Option<Expr> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    /// One column, or several for `SET (a, b) = (x, y)`.
    pub columns: Vec<String>,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub with: Option<With>,
    pub table: ObjectName,
    pub alias: Option<String>,
    pub assignments: Vec<Assignment>,
    pub from: Vec<TableRef>,
    pub selection: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    pub with: Option<With>,
    pub table: ObjectName,
    pub alias: Option<String>,
    pub using: Vec<TableRef>,
    pub selection: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

// ---------------------------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct CreateTable {
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub temporary: bool,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
    /// `CREATE TABLE t AS SELECT ...`.
    pub as_query: Option<Box<Query>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: TypeName,
    pub constraints: Vec<ColumnConstraint>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnConstraint {
    pub name: Option<String>,
    pub kind: ColumnConstraintKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ColumnConstraintKind {
    NotNull,
    Null,
    PrimaryKey,
    Unique {
        nulls_not_distinct: bool,
    },
    Check(Expr),
    References(ForeignKeyRef),
    Default(Expr),
    /// `GENERATED ALWAYS AS (expr) STORED`.
    Generated(Expr),
    /// `GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY`.
    Identity {
        always: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKeyRef {
    pub table: ObjectName,
    /// Empty = the referenced table's primary key.
    pub columns: Vec<String>,
    pub on_delete: ReferentialAction,
    pub on_update: ReferentialAction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReferentialAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableConstraint {
    pub name: Option<String>,
    pub kind: TableConstraintKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TableConstraintKind {
    PrimaryKey(Vec<String>),
    Unique { columns: Vec<String>, nulls_not_distinct: bool },
    Check(Expr),
    ForeignKey { columns: Vec<String>, references: ForeignKeyRef },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateView {
    pub name: ObjectName,
    pub or_replace: bool,
    pub columns: Vec<String>,
    pub query: Box<Query>,
    pub materialized: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateIndex {
    pub name: Option<String>,
    pub table: ObjectName,
    pub columns: Vec<OrderByExpr>,
    pub unique: bool,
    pub if_not_exists: bool,
    pub concurrently: bool,
    pub predicate: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateSequence {
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub start: Option<i64>,
    pub increment: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlterTable {
    pub name: ObjectName,
    pub if_exists: bool,
    pub actions: Vec<AlterTableAction>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AlterTableAction {
    AddColumn { if_not_exists: bool, column: ColumnDef },
    DropColumn { name: String, if_exists: bool, cascade: bool },
    RenameColumn { old: String, new: String },
    RenameTable { new: String },
    SetNotNull { column: String },
    DropNotNull { column: String },
    SetDefault { column: String, expr: Expr },
    DropDefault { column: String },
    AlterType { column: String, ty: TypeName, using: Option<Expr> },
    AddConstraint(TableConstraint),
    DropConstraint { name: String, if_exists: bool, cascade: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectType {
    Table,
    View,
    Index,
    Sequence,
    Schema,
    Extension,
    Type,
    Function,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Drop {
    pub object_type: ObjectType,
    pub names: Vec<ObjectName>,
    pub if_exists: bool,
    pub cascade: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Truncate {
    pub tables: Vec<ObjectName>,
    pub restart_identity: bool,
    pub cascade: bool,
}

impl Expr {
    /// Pre-order traversal of this expression and its sub-expressions (not into subqueries).
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        let each = |items: &[Expr], f: &mut dyn FnMut(&Expr)| items.iter().for_each(|e| e.walk(f));
        match self {
            Expr::Literal(_) | Expr::Column(_) | Expr::Param(_) | Expr::TypedString { .. } | Expr::Default => {}
            Expr::Exists { .. } | Expr::Subquery(_) | Expr::ArraySubquery(_) => {}
            Expr::Unary { expr, .. }
            | Expr::Not(expr)
            | Expr::IsNull { expr, .. }
            | Expr::IsBool { expr, .. }
            | Expr::Cast { expr, .. }
            | Expr::Collate { expr, .. }
            | Expr::InSubquery { expr, .. } => expr.walk(f),
            Expr::Binary { left, right, .. }
            | Expr::And(left, right)
            | Expr::Or(left, right)
            | Expr::IsDistinctFrom { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            Expr::AtTimeZone { expr, zone } => {
                expr.walk(f);
                zone.walk(f);
            }
            Expr::Between { expr, low, high, .. } => {
                expr.walk(f);
                low.walk(f);
                high.walk(f);
            }
            Expr::InList { expr, list, .. } => {
                expr.walk(f);
                each(list, f);
            }
            Expr::AnyAll { left, right, .. } => {
                left.walk(f);
                if let AnyAllRight::Array(a) = right {
                    a.walk(f);
                }
            }
            Expr::Like { expr, pattern, escape, .. } | Expr::SimilarTo { expr, pattern, escape, .. } => {
                expr.walk(f);
                pattern.walk(f);
                if let Some(e) = escape {
                    e.walk(f);
                }
            }
            Expr::Case { operand, whens, else_result } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (w, t) in whens {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(e) = else_result {
                    e.walk(f);
                }
            }
            Expr::Function(call) => {
                each(&call.args, f);
                if let Some(fl) = &call.filter {
                    fl.walk(f);
                }
            }
            Expr::Array(items) | Expr::Row(items) => each(items, f),
            Expr::Subscript { expr, index } => {
                expr.walk(f);
                match index {
                    Subscript::Index(i) => i.walk(f),
                    Subscript::Slice { lower, upper } => {
                        if let Some(l) = lower {
                            l.walk(f);
                        }
                        if let Some(u) = upper {
                            u.walk(f);
                        }
                    }
                }
            }
        }
    }

    /// Distinct column names referenced (last name part), in order of first appearance.
    pub fn referenced_columns(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        self.walk(&mut |e| {
            if let Expr::Column(parts) = e
                && let Some(last) = parts.last()
                && !out.contains(last)
            {
                out.push(last.clone());
            }
        });
        out
    }
}
