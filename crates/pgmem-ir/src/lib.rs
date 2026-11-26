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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
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
    Merge(Box<Merge>),
    CreateTable(Box<CreateTable>),
    CreateView(Box<CreateView>),
    CreateIndex(Box<CreateIndex>),
    CreateSequence(Box<CreateSequence>),
    AlterTable(Box<AlterTable>),
    CreateType(Box<CreateType>),
    CreateDomain(Box<CreateDomain>),
    AlterType(Box<AlterType>),
    AlterDomain(Box<AlterDomain>),
    Drop(Box<Drop>),
    Truncate(Box<Truncate>),
    CreateSchema(Box<CreateSchema>),
    /// `ALTER {TABLE|VIEW|SEQUENCE|TYPE|DOMAIN} name SET SCHEMA schema`.
    SetSchema {
        object_type: ObjectType,
        name: ObjectName,
        if_exists: bool,
        schema: String,
    },
    /// `ALTER SCHEMA name RENAME TO new` (`OWNER TO` is accepted as a no-op).
    AlterSchema {
        name: String,
        rename_to: Option<String>,
    },
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
    /// `PREPARE name [(type, ...)] AS statement`. `text` is the query string it came from
    /// (`pg_prepared_statements.statement`).
    Prepare(Box<Prepare>),
    /// `EXECUTE name [(arg, ...)]`.
    Execute {
        name: String,
        args: Vec<Expr>,
    },
    /// `DEALLOCATE [PREPARE] name | ALL` (`None` = ALL).
    Deallocate {
        name: Option<String>,
    },
    /// `DECLARE name [BINARY] [NO] [SCROLL] CURSOR [WITH[OUT] HOLD] FOR query`.
    DeclareCursor(Box<DeclareCursor>),
    /// `FETCH` / `MOVE` (`is_move`).
    Fetch {
        name: String,
        direction: FetchDirection,
        is_move: bool,
    },
    /// `CLOSE name | ALL` (`None` = ALL).
    CloseCursor {
        name: Option<String>,
    },
    /// `LISTEN channel`.
    Listen {
        channel: String,
    },
    /// `UNLISTEN channel | *` (`None` = *).
    Unlisten {
        channel: Option<String>,
    },
    /// `NOTIFY channel [, 'payload']`.
    Notify {
        channel: String,
        payload: Option<String>,
    },
    /// `DISCARD ALL | PLANS | SEQUENCES | TEMP`.
    Discard(DiscardKind),
    /// `EXPLAIN [ANALYZE] [(options)] statement`.
    Explain(Box<Explain>),
    /// `VACUUM` / `ANALYZE` / `CLUSTER` / `REINDEX TABLE` naming tables (checked to exist).
    /// `in_block_ok` is false for the commands that refuse to run inside a transaction block.
    Maintenance {
        tag: String,
        tables: Vec<ObjectName>,
        in_block_ok: bool,
    },
    /// `COPY table [(columns)] | (query) {FROM | TO} {STDIN | STDOUT | 'file' | PROGRAM 'cmd'}
    /// [WITH] (options) [WHERE condition]`.
    Copy(Box<Copy>),
    /// `REFRESH MATERIALIZED VIEW [CONCURRENTLY] name [WITH [NO] DATA]`.
    RefreshMatview {
        name: ObjectName,
        concurrently: bool,
        with_data: bool,
    },
    /// `ALTER [MATERIALIZED] VIEW name RENAME TO new`.
    RenameView {
        name: ObjectName,
        new_name: String,
        materialized: bool,
        if_exists: bool,
    },
    CreateRole {
        name: String,
        options: Box<RoleOptions>,
        if_not_exists: bool,
    },
    AlterRole {
        name: String,
        action: AlterRoleAction,
    },
    DropRole {
        names: Vec<String>,
        if_exists: bool,
    },
    /// GRANT / REVOKE of privileges on objects.
    Grant(Box<GrantStmt>),
    /// `GRANT role TO member` / `REVOKE role FROM member`.
    GrantRole {
        roles: Vec<String>,
        members: Vec<String>,
        admin: bool,
        revoke: bool,
    },
    /// `ALTER DEFAULT PRIVILEGES [IN SCHEMA s] {GRANT | REVOKE} ... ON {TABLES | ...}`.
    AlterDefaultPrivileges {
        schemas: Vec<String>,
        kind: char,
        grant: Box<GrantStmt>,
    },
    DropOwned {
        roles: Vec<String>,
        cascade: bool,
    },
    ReassignOwned {
        roles: Vec<String>,
        to: String,
    },
    /// `ALTER {SCHEMA | SEQUENCE | VIEW | FUNCTION | TYPE | ...} name OWNER TO role`.
    AlterOwner {
        object: OwnedObject,
        name: ObjectName,
        owner: String,
    },
    CreateExtension {
        name: String,
        if_not_exists: bool,
        schema: Option<String>,
        version: Option<String>,
    },
    DropExtension {
        names: Vec<String>,
        if_exists: bool,
        cascade: bool,
    },
    CreateFunction(Box<CreateFunction>),
    /// `ALTER {FUNCTION | PROCEDURE | ROUTINE} name [(types)] action ...` (not `OWNER TO`).
    AlterFunction {
        name: ObjectName,
        types: Option<Vec<TypeName>>,
        kind: RoutineKind,
        actions: Vec<AlterFunctionAction>,
    },
    /// `DROP {FUNCTION | PROCEDURE | ROUTINE} [IF EXISTS] name [(types)], ... [CASCADE]`.
    DropFunction {
        items: Vec<(ObjectName, Option<Vec<TypeName>>)>,
        if_exists: bool,
        cascade: bool,
        kind: RoutineKind,
    },
    /// `CALL name(args)`.
    Call {
        name: ObjectName,
        args: Vec<Expr>,
    },
    /// `DO [LANGUAGE lang] body`.
    Do {
        language: String,
        body: String,
    },
    CreateTrigger(Box<CreateTrigger>),
    DropTrigger {
        name: String,
        table: ObjectName,
        if_exists: bool,
        cascade: bool,
    },
    /// `ALTER TRIGGER name ON table RENAME TO new_name`.
    RenameTrigger {
        name: String,
        table: ObjectName,
        new_name: String,
    },
    /// `ALTER SEQUENCE [IF EXISTS] name options | RENAME TO new_name`.
    AlterSequence {
        name: ObjectName,
        if_exists: bool,
        options: Box<SequenceOptions>,
        rename_to: Option<String>,
    },
    /// `ALTER {TABLE | INDEX | VIEW | MATERIALIZED VIEW} [IF EXISTS] name {SET | RESET} (options)`:
    /// `object` is the keyword used.
    AlterRelOptions {
        object: ObjectType,
        name: ObjectName,
        if_exists: bool,
        reset: bool,
        options: Vec<RelOption>,
    },
    /// `ALTER INDEX [IF EXISTS] name RENAME TO new_name`.
    RenameIndex {
        name: ObjectName,
        new_name: String,
        if_exists: bool,
    },
    /// `SET CONSTRAINTS {ALL | name, ...} {DEFERRED | IMMEDIATE}` (`names` empty = ALL).
    SetConstraints {
        names: Vec<ObjectName>,
        deferred: bool,
    },
    /// `LOCK [TABLE] name, ... [IN mode MODE] [NOWAIT]`: `mode` from 1 (ACCESS SHARE) to 8
    /// (ACCESS EXCLUSIVE, the default).
    Lock {
        tables: Vec<ObjectName>,
        mode: u8,
        nowait: bool,
    },
}

/// Expanded GROUPING SETS: each set lists indexes into `Select::group_by`.
pub type GroupingSets = Vec<Vec<usize>>;

/// `PREPARE`.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepare {
    pub name: String,
    pub param_types: Vec<TypeName>,
    pub statement: Statement,
    pub text: String,
}

/// `DECLARE ... CURSOR`.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclareCursor {
    pub name: String,
    pub query: Query,
    pub binary: bool,
    /// `SCROLL` (true), `NO SCROLL` (false), or neither.
    pub scroll: Option<bool>,
    pub hold: bool,
    pub text: String,
}

/// A FETCH / MOVE direction, normalized the way Postgres' grammar does: `NEXT` is
/// `Forward(Some(1))`, `PRIOR` is `Backward(Some(1))`, `FIRST` / `LAST` are `Absolute(1)` /
/// `Absolute(-1)`; `None` counts are `ALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchDirection {
    Forward(Option<i64>),
    Backward(Option<i64>),
    Absolute(i64),
    Relative(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardKind {
    All,
    Plans,
    Sequences,
    Temp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplainFormat {
    Text,
    Json,
    Yaml,
    Xml,
}

/// `EXPLAIN`.
#[derive(Clone, Debug, PartialEq)]
pub struct Explain {
    pub statement: Statement,
    pub analyze: bool,
    pub format: ExplainFormat,
}

/// `CREATE SCHEMA [IF NOT EXISTS] [name] [AUTHORIZATION role] [element ...]`.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateSchema {
    /// None for `CREATE SCHEMA AUTHORIZATION role` (the schema is named after the role).
    pub name: Option<String>,
    pub if_not_exists: bool,
    pub authorization: Option<String>,
    /// Embedded `CREATE TABLE` / `CREATE VIEW` / ... statements, created in the new schema.
    pub elements: Vec<Statement>,
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
    Type,
    Domain,
    /// A function or procedure, with its argument types when given.
    Function(Option<Vec<TypeName>>),
    /// `CONSTRAINT name ON table`.
    Constraint(ObjectName),
    /// `TRIGGER name ON table`.
    Trigger(ObjectName),
    /// Object kinds pgmem has no catalog entry for (extensions, roles, databases, ...):
    /// accepted without effect.
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
    /// `FETCH FIRST n ROWS WITH TIES`: rows tied with the last one by ORDER BY are kept.
    pub with_ties: bool,
    /// `FOR UPDATE` / `FOR SHARE` ... (accepted; pgmem's snapshot model makes them no-ops).
    pub locking: Vec<LockClause>,
}

impl Query {
    pub fn from_body(body: SetExpr) -> Query {
        Query { with: None, body, order_by: Vec::new(), limit: None, offset: None, with_ties: false, locking: Vec::new() }
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
    /// A data-modifying statement (INSERT / UPDATE / DELETE / MERGE) as a WITH query.
    Dml(Box<Statement>),
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

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Select {
    pub distinct: Distinct,
    pub projection: Vec<SelectItem>,
    pub from: Vec<TableRef>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    /// `GROUPING SETS` / `ROLLUP` / `CUBE`, expanded: each set lists the `group_by`
    /// expressions it groups by (None for a plain GROUP BY).
    pub grouping_sets: Option<GroupingSets>,
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
    /// `(composite_expr).*`: one output column per field.
    FieldStar(Expr),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableAlias {
    pub name: String,
    pub columns: Vec<String>,
    /// A column definition list (`AS t(a int, b text)`, for functions returning `record`):
    /// one type per column, or empty.
    pub column_types: Vec<TypeName>,
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
        /// `JOIN ... USING (k) AS j`: names the merged USING columns.
        using_alias: Option<String>,
        /// `(a JOIN b) AS j(x, y)`: hides the inner table names behind one alias.
        alias: Option<TableAlias>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeName {
    /// Lower-cased base type name as written (`int4`, `varchar`, `double precision`, ...).
    pub name: String,
    /// Type modifiers (`varchar(20)` → `[20]`, `numeric(10, 2)` → `[10, 2]`).
    pub modifiers: Vec<i64>,
    /// Number of `[]` array dimensions.
    pub array_dims: usize,
    /// The schema of a qualified type name (`app.mood`, `pg_catalog.int4`).
    pub schema: Option<String>,
}

impl TypeName {
    pub fn simple(name: impl Into<String>) -> TypeName {
        TypeName { name: name.into(), modifiers: Vec::new(), array_dims: 0, schema: None }
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
    /// `(composite_expr).field`.
    FieldAccess {
        expr: Box<Expr>,
        field: String,
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
pub struct Copy {
    /// The table (with an optional column list), or None for `COPY (query) TO`.
    pub table: Option<ObjectName>,
    pub columns: Vec<String>,
    /// `COPY (SELECT ... | INSERT/UPDATE/DELETE ... RETURNING ...) TO`.
    pub query: Option<Box<Statement>>,
    /// COPY FROM (true) or COPY TO.
    pub from: bool,
    pub target: CopyTarget,
    /// Options in the order written (legacy options are mapped to their generic names).
    pub options: Vec<CopyOption>,
    pub where_clause: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyTarget {
    /// STDIN / STDOUT (the CopyIn / CopyOut sub-protocol).
    Stdio,
    File(String),
    Program(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyOption {
    pub name: String,
    pub value: CopyOptionValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyOptionValue {
    None,
    /// A string, number or word argument.
    Str(String),
    /// `*` (FORCE_QUOTE * / FORCE_NOT_NULL * / FORCE_NULL *).
    Star,
    /// A parenthesized column list.
    List(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    pub with: Option<With>,
    pub table: ObjectName,
    pub alias: Option<String>,
    pub columns: Vec<String>,
    pub source: InsertSource,
    /// `OVERRIDING SYSTEM VALUE` lets explicit values into `GENERATED ALWAYS AS IDENTITY`.
    pub overriding_system_value: bool,
    /// `OVERRIDING USER VALUE`: explicit values for identity columns are ignored.
    pub overriding_user_value: bool,
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
    /// `(col, ..., (expr), ...) [WHERE predicate]`: plain columns and expression elements.
    Columns {
        columns: Vec<String>,
        exprs: Vec<Expr>,
        selection: Option<Expr>,
    },
    Constraint(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConflictAction {
    Nothing,
    Update { assignments: Vec<Assignment>, selection: Option<Expr> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    /// One target, or several for `SET (a, b) = (x, y)`. A target is a column name followed
    /// by composite field names (`SET addr.zip = ...` is `["addr", "zip"]`).
    pub columns: Vec<Vec<String>>,
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

/// `MERGE INTO target [alias] USING source ON condition WHEN ... [RETURNING ...]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Merge {
    pub with: Option<With>,
    pub table: ObjectName,
    pub alias: Option<String>,
    pub source: TableRef,
    pub on: Expr,
    pub clauses: Vec<MergeWhen>,
    pub returning: Vec<SelectItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMatch {
    Matched,
    /// `NOT MATCHED [BY TARGET]`: a source row without a target row.
    NotMatchedByTarget,
    /// `NOT MATCHED BY SOURCE`: a target row without a source row.
    NotMatchedBySource,
}

/// One `WHEN [NOT] MATCHED [BY ...] [AND condition] THEN action`.
#[derive(Clone, Debug, PartialEq)]
pub struct MergeWhen {
    pub kind: MergeMatch,
    pub condition: Option<Expr>,
    pub action: MergeAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MergeAction {
    Update(Vec<Assignment>),
    Delete,
    DoNothing,
    /// `INSERT [(columns)] [OVERRIDING SYSTEM VALUE] VALUES (...)`; `values: None` is
    /// `DEFAULT VALUES`.
    Insert {
        columns: Vec<String>,
        values: Option<Vec<Expr>>,
        overriding_system_value: bool,
    },
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
    pub options: CreateTableOptions,
}

/// The less common parts of CREATE TABLE.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateTableOptions {
    /// `AS ... WITH [NO] DATA`.
    pub with_data: bool,
    pub on_commit: OnCommit,
    pub unlogged: bool,
    /// `LIKE source [INCLUDING ...]` elements, each before the column at `position`.
    pub likes: Vec<LikeClause>,
    /// `CREATE TABLE t (a, b) AS ...`: names for the query's columns.
    pub column_names: Vec<String>,
    /// `PARTITION BY strategy (key, ...)`.
    pub partition_by: Option<PartitionSpec>,
    /// `PARTITION OF parent FOR VALUES ... | DEFAULT`.
    pub partition_of: Option<(ObjectName, PartitionBoundSpec)>,
    /// `WITH (storage_parameter [= value], ...)`.
    pub reloptions: Vec<RelOption>,
}

/// One storage parameter of a `WITH (...)` / `SET (...)` / `RESET (...)` list: `[namespace.]name
/// [= value]`, the name folded like an identifier and the value as written (text).
#[derive(Clone, Debug, PartialEq)]
pub struct RelOption {
    pub namespace: Option<String>,
    pub name: String,
    pub value: Option<String>,
}

/// `PARTITION BY RANGE | LIST | HASH (key, ...)`: each key a column or an expression.
#[derive(Clone, Debug, PartialEq)]
pub struct PartitionSpec {
    /// `range`, `list` or `hash` (lower case).
    pub strategy: String,
    pub keys: Vec<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PartitionBoundSpec {
    Default,
    In(Vec<Expr>),
    Range { from: Vec<RangeBoundSpec>, to: Vec<RangeBoundSpec> },
    Hash { modulus: u64, remainder: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum RangeBoundSpec {
    MinValue,
    MaxValue,
    Expr(Expr),
}

impl Default for CreateTableOptions {
    fn default() -> Self {
        CreateTableOptions {
            with_data: true,
            on_commit: OnCommit::PreserveRows,
            unlogged: false,
            likes: Vec::new(),
            column_names: Vec::new(),
            partition_by: None,
            partition_of: None,
            reloptions: Vec::new(),
        }
    }
}

/// A temporary table's `ON COMMIT` action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnCommit {
    PreserveRows,
    DeleteRows,
    Drop,
}

/// `LIKE source [INCLUDING | EXCLUDING {DEFAULTS | CONSTRAINTS | INDEXES | IDENTITY |
/// GENERATED | COMMENTS | STORAGE | STATISTICS | COMPRESSION | ALL}] ...`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct LikeClause {
    pub position: usize,
    pub table: ObjectName,
    pub defaults: bool,
    pub constraints: bool,
    pub indexes: bool,
    pub identity: bool,
    pub generated: bool,
    pub comments: bool,
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
    pub timing: ConstraintTiming,
}

/// `[NOT] DEFERRABLE [INITIALLY DEFERRED | IMMEDIATE]` and `NOT VALID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ConstraintTiming {
    pub deferrable: bool,
    pub initially_deferred: bool,
    pub not_valid: bool,
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
    /// `COLLATE name`.
    Collate(String),
    /// `GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY`.
    Identity {
        always: bool,
        options: SequenceOptions,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKeyRef {
    pub table: ObjectName,
    /// Empty = the referenced table's primary key.
    pub columns: Vec<String>,
    pub on_delete: ReferentialAction,
    pub on_update: ReferentialAction,
    /// `MATCH FULL` (else MATCH SIMPLE).
    pub match_full: bool,
    /// `ON DELETE SET NULL (cols)` / `SET DEFAULT (cols)`: the columns set (empty = all).
    pub delete_set_columns: Vec<String>,
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
    pub timing: ConstraintTiming,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TableConstraintKind {
    PrimaryKey(Vec<String>),
    Unique {
        columns: Vec<String>,
        nulls_not_distinct: bool,
    },
    Check(Expr),
    ForeignKey {
        columns: Vec<String>,
        references: ForeignKeyRef,
    },
    /// `{PRIMARY KEY | UNIQUE} USING INDEX index`: adopts an existing unique index.
    UsingIndex {
        primary: bool,
        index: String,
    },
    /// `EXCLUDE [USING method] (element WITH operator, ...) [WHERE (predicate)]`.
    Exclude {
        method: Option<String>,
        elements: Vec<(Expr, String)>,
        predicate: Option<Expr>,
    },
    /// `{PRIMARY KEY | UNIQUE} (a, ..., b WITHOUT OVERLAPS)`: equal on the leading columns,
    /// overlapping on the last.
    Temporal {
        primary: bool,
        columns: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateView {
    pub name: ObjectName,
    pub or_replace: bool,
    pub columns: Vec<String>,
    pub query: Box<Query>,
    pub materialized: bool,
    /// `WITH [CASCADED | LOCAL] CHECK OPTION`.
    pub check_option: bool,
    /// The check option was written `WITH LOCAL CHECK OPTION`.
    pub check_local: bool,
    /// `WITH (security_barrier, ...)`.
    pub reloptions: Vec<RelOption>,
    /// A materialized view's `WITH [NO] DATA` (default WITH DATA).
    pub with_data: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateIndex {
    pub name: Option<String>,
    pub table: ObjectName,
    pub columns: Vec<OrderByExpr>,
    /// `INCLUDE (...)` column names.
    pub include: Vec<String>,
    pub unique: bool,
    pub if_not_exists: bool,
    pub concurrently: bool,
    pub predicate: Option<Expr>,
    pub nulls_not_distinct: bool,
    /// `USING method` (lowercase), if given.
    pub method: Option<String>,
    /// The explicit operator class of each key (parallel to `columns`; the last name part).
    pub opclasses: Vec<Option<String>>,
    /// `WITH (fillfactor = 70, ...)`.
    pub reloptions: Vec<RelOption>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateSequence {
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub options: SequenceOptions,
}

/// CREATE / ALTER ROLE options; None where not given.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RoleOptions {
    pub superuser: Option<bool>,
    pub createdb: Option<bool>,
    pub createrole: Option<bool>,
    pub inherit: Option<bool>,
    pub login: Option<bool>,
    pub replication: Option<bool>,
    pub bypassrls: Option<bool>,
    pub connlimit: Option<i32>,
    /// `PASSWORD 'x'` (Some(Some)) / `PASSWORD NULL` (Some(None)).
    pub password: Option<Option<String>>,
    pub valid_until: Option<String>,
    /// `IN ROLE a, b`: roles the new role joins.
    pub in_roles: Vec<String>,
    /// `ROLE a, b` / `USER a, b`: members of the new role.
    pub members: Vec<String>,
    pub admins: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AlterRoleAction {
    Options(RoleOptions),
    Rename(String),
    /// `SET name {TO | =} value` (None value: `SET name TO DEFAULT`).
    Set(String, Option<String>),
    /// `RESET name` / `RESET ALL` (None).
    Reset(Option<String>),
}

/// What GRANT / REVOKE names.
#[derive(Clone, Debug, PartialEq)]
pub enum GrantTarget {
    Tables(Vec<ObjectName>),
    AllTablesInSchema(Vec<String>),
    Sequences(Vec<ObjectName>),
    AllSequencesInSchema(Vec<String>),
    Schemas(Vec<String>),
    Databases(Vec<String>),
    Functions(Vec<(ObjectName, Option<Vec<TypeName>>)>),
    AllFunctionsInSchema(Vec<String>),
    Types(Vec<ObjectName>),
    /// Object kinds pgmem keeps no privileges for (languages, foreign servers, ...).
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GrantStmt {
    /// Privilege keywords (lowercase); None for ALL [PRIVILEGES].
    pub privileges: Option<Vec<String>>,
    /// Column lists, one per entry of `privileges` (one for ALL [PRIVILEGES]); an empty list
    /// is a privilege on the whole object. Empty when no privilege has a column list.
    pub columns: Vec<Vec<String>>,
    pub target: GrantTarget,
    /// Role names; "public" for PUBLIC.
    pub grantees: Vec<String>,
    /// GRANT ... WITH GRANT OPTION / REVOKE GRANT OPTION FOR.
    pub grant_option: bool,
    pub revoke: bool,
    pub cascade: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnedObject {
    Schema,
    Sequence,
    View,
    MaterializedView,
    Function,
    Procedure,
    Type,
    Domain,
    Database,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutineKind {
    Function,
    Procedure,
    Routine,
}

/// `[IN | OUT | INOUT | VARIADIC] [name] type [DEFAULT expr]`.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionArg {
    pub mode: FunctionArgMode,
    pub name: Option<String>,
    pub ty: TypeName,
    pub default: Option<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionArgMode {
    In,
    Out,
    InOut,
    Variadic,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FunctionReturns {
    Type(TypeName),
    SetOf(TypeName),
    Table(Vec<(String, TypeName)>),
}

/// `CREATE [OR REPLACE] {FUNCTION | PROCEDURE} ...`.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateFunction {
    pub name: ObjectName,
    pub or_replace: bool,
    pub procedure: bool,
    pub args: Vec<FunctionArg>,
    pub returns: Option<FunctionReturns>,
    pub language: Option<String>,
    /// The body text: `AS '...'` / `AS $$...$$`, or a SQL-standard `RETURN expr` as
    /// `SELECT expr` / `BEGIN ATOMIC ... END` as its statements.
    pub body: Option<String>,
    pub volatility: Option<char>,
    pub strict: bool,
    pub security_definer: bool,
    pub leakproof: bool,
    /// `PARALLEL {UNSAFE | RESTRICTED | SAFE}` as `u` / `r` / `s`.
    pub parallel: Option<char>,
    pub cost: Option<f64>,
    pub rows: Option<f64>,
    /// `SET` / `RESET` options in the order written.
    pub config: Vec<FunctionConfig>,
}

/// A `SET name {TO | =} value | FROM CURRENT | DEFAULT` / `RESET name | ALL` function option.
#[derive(Clone, Debug, PartialEq)]
pub enum FunctionConfig {
    /// `SET name TO v1, v2`: the values as written (strings, identifiers, numbers).
    Set(String, Vec<String>),
    /// `SET name FROM CURRENT`.
    FromCurrent(String),
    /// `SET name TO DEFAULT` / `RESET name`.
    Reset(String),
    /// `RESET ALL`.
    ResetAll,
}

/// One action of `ALTER {FUNCTION | PROCEDURE | ROUTINE} name [(types)] action ...`.
#[derive(Clone, Debug, PartialEq)]
pub enum AlterFunctionAction {
    Volatility(char),
    Strict(bool),
    SecurityDefiner(bool),
    Leakproof(bool),
    Parallel(char),
    Cost(f64),
    Rows(f64),
    Config(FunctionConfig),
    RenameTo(String),
    SetSchema(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
    InsteadOf,
}

/// `CREATE [OR REPLACE] TRIGGER name {BEFORE | AFTER | INSTEAD OF} events ON table ...`.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateTrigger {
    pub name: String,
    pub or_replace: bool,
    pub timing: TriggerTiming,
    pub insert: bool,
    pub delete: bool,
    pub truncate: bool,
    /// `UPDATE [OF cols]`.
    pub update: Option<Vec<String>>,
    pub table: ObjectName,
    pub for_each_row: bool,
    /// The WHEN condition's SQL text (it reads `NEW` / `OLD`).
    pub when: Option<String>,
    pub function: ObjectName,
    pub args: Vec<String>,
}

/// CREATE / ALTER SEQUENCE and identity column options; None where not given.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SequenceOptions {
    pub data_type: Option<TypeName>,
    pub increment: Option<i64>,
    /// `MINVALUE n` / `NO MINVALUE` (Some(None)).
    pub min: Option<Option<i64>>,
    pub max: Option<Option<i64>>,
    pub start: Option<i64>,
    /// `RESTART [WITH n]` (ALTER only).
    pub restart: Option<Option<i64>>,
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
    /// `OWNED BY table.column` / `OWNED BY NONE` (Some(None)).
    pub owned_by: Option<Option<ObjectName>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlterTable {
    pub name: ObjectName,
    pub if_exists: bool,
    pub actions: Vec<AlterTableAction>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AlterTableAction {
    /// `ATTACH PARTITION name FOR VALUES ... | DEFAULT`.
    AttachPartition {
        partition: ObjectName,
        bound: PartitionBoundSpec,
    },
    /// `DETACH PARTITION name [CONCURRENTLY | FINALIZE]`.
    DetachPartition {
        partition: ObjectName,
    },
    AddColumn {
        if_not_exists: bool,
        column: ColumnDef,
    },
    DropColumn {
        name: String,
        if_exists: bool,
        cascade: bool,
    },
    RenameColumn {
        old: String,
        new: String,
    },
    RenameTable {
        new: String,
    },
    RenameConstraint {
        old: String,
        new: String,
    },
    /// `ALTER COLUMN c ADD GENERATED {ALWAYS | BY DEFAULT} AS IDENTITY [(options)]`.
    AddIdentity {
        column: String,
        always: bool,
        options: SequenceOptions,
    },
    /// `ALTER COLUMN c {SET GENERATED ... | SET option | RESTART ...} ...`.
    SetIdentity {
        column: String,
        always: Option<bool>,
        options: SequenceOptions,
    },
    DropIdentity {
        column: String,
        if_exists: bool,
    },
    /// `ALTER COLUMN c SET EXPRESSION AS (expr)`.
    SetExpression {
        column: String,
        expr: Expr,
    },
    DropExpression {
        column: String,
        if_exists: bool,
    },
    /// `OWNER TO role`.
    OwnerTo(String),
    /// `{ENABLE [REPLICA | ALWAYS] | DISABLE} TRIGGER {name | ALL | USER}` (None: all);
    /// `mode` is the `pg_trigger.tgenabled` letter: 'O' origin, 'D' disabled, 'R' replica,
    /// 'A' always.
    EnableTrigger {
        name: Option<String>,
        mode: char,
    },
    SetNotNull {
        column: String,
    },
    DropNotNull {
        column: String,
    },
    SetDefault {
        column: String,
        expr: Expr,
    },
    DropDefault {
        column: String,
    },
    AlterType {
        column: String,
        ty: TypeName,
        using: Option<Expr>,
    },
    AddConstraint(TableConstraint),
    DropConstraint {
        name: String,
        if_exists: bool,
        cascade: bool,
    },
    /// `VALIDATE CONSTRAINT name`.
    ValidateConstraint {
        name: String,
    },
    /// `ALTER CONSTRAINT name [NOT] DEFERRABLE [INITIALLY ...]`.
    AlterConstraint {
        name: String,
        timing: ConstraintTiming,
    },
    /// `ALTER COLUMN c SET STATISTICS n`.
    SetStatistics {
        column: String,
        target: i64,
    },
    /// `ALTER COLUMN c SET STORAGE {PLAIN | EXTERNAL | EXTENDED | MAIN | DEFAULT}` (the name
    /// as written, lower case).
    SetStorage {
        column: String,
        storage: String,
    },
    /// `ALTER COLUMN c SET COMPRESSION {method | DEFAULT}`.
    SetCompression {
        column: String,
        method: String,
    },
    /// `REPLICA IDENTITY {DEFAULT | FULL | NOTHING | USING INDEX i}`: `kind` is the
    /// `relreplident` letter (`i` with `index`).
    ReplicaIdentity {
        kind: char,
        index: Option<String>,
    },
    /// `CLUSTER ON index` / `SET WITHOUT CLUSTER` (None).
    ClusterOn(Option<String>),
    /// `SET LOGGED` (false) / `SET UNLOGGED` (true).
    SetUnlogged(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateType {
    pub name: ObjectName,
    pub kind: CreateTypeKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CreateTypeKind {
    /// `AS ENUM ('a', 'b')`.
    Enum(Vec<String>),
    /// `AS (field type, ...)`.
    Composite(Vec<(String, TypeName)>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreateDomain {
    pub name: ObjectName,
    pub ty: TypeName,
    pub default: Option<Expr>,
    pub not_null: bool,
    /// CHECK constraints (optional name, expression over `VALUE`), in declaration order.
    pub checks: Vec<(Option<String>, Expr)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlterType {
    pub name: ObjectName,
    pub action: AlterTypeAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AlterTypeAction {
    /// `ADD VALUE [IF NOT EXISTS] 'v' [BEFORE | AFTER 'x']` (`before` is true for BEFORE).
    AddValue {
        value: String,
        if_not_exists: bool,
        neighbor: Option<(bool, String)>,
    },
    RenameValue {
        from: String,
        to: String,
    },
    Rename(String),
    AddAttribute {
        name: String,
        ty: TypeName,
    },
    DropAttribute {
        name: String,
        if_exists: bool,
    },
    RenameAttribute {
        from: String,
        to: String,
    },
    /// `OWNER TO`, `SET SCHEMA public`: accepted without effect.
    NoOp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AlterDomain {
    pub name: ObjectName,
    pub action: AlterDomainAction,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AlterDomainAction {
    SetDefault(Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    AddCheck { name: Option<String>, expr: Expr, not_valid: bool },
    DropConstraint { name: String, if_exists: bool },
    RenameConstraint { from: String, to: String },
    ValidateConstraint(String),
    Rename(String),
    NoOp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectType {
    Table,
    View,
    MaterializedView,
    Index,
    Sequence,
    Schema,
    Extension,
    Type,
    Domain,
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
            Expr::FieldAccess { expr, .. } => expr.walk(f),
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
