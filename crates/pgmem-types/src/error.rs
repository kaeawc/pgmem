//! SQL-visible errors. Every failure that reaches a client carries a five-character SQLSTATE,
//! because drivers (pgx, tokio-postgres, JDBC) pattern-match on the code, not the message.
//! Messages follow real Postgres wording wherever a test or client is known to look at them.

use std::fmt;

/// An error with a Postgres SQLSTATE. `XX000` (internal_error) is reserved for engine bugs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlError {
    pub code: &'static str,
    pub message: String,
    /// Optional fields, boxed to keep `Result<_, SqlError>` small.
    fields: Option<Box<ErrorFields>>,
}

/// Optional ErrorResponse fields: DETAIL, HINT, and the object fields Postgres attaches to
/// integrity errors (`s`chema, `t`able, `c`olumn, constraint `n`ame).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorFields {
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub constraint: Option<String>,
    /// The 1-based character offset into the query string the error points at (`P`).
    pub position: Option<u32>,
    /// What the error is about, for [`SourceRefs::resolve`] to turn into a `position` once the
    /// statement's source locations are at hand (the binder only sees the IR).
    pub subject: Option<ErrorRef>,
}

/// A reference in the statement text an error can point at, keyed by its folded name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ErrorRef {
    /// A column reference as written, parts joined with `.` (`a`, `t.a`).
    Column(String),
    /// A column reference as written that is not inside an aggregate call (what Postgres'
    /// grouping check complains about), parts joined with `.`.
    Ungrouped(String),
    /// A column an `INSERT` column list or `UPDATE SET` names.
    Target(String),
    /// A relation named in FROM or as a DML target, parts joined with `.`.
    Relation(String),
    /// A function call by its name as written, parts joined with `.`.
    Function(String),
    /// A binary operator by its Postgres name.
    Operator(String),
    /// A prefix operator by its Postgres name.
    PrefixOperator(String),
    /// A string literal by its value.
    Literal(String),
    /// A type name as written, parts joined with `.`.
    Type(String),
    /// A relation named in a `FOR UPDATE OF` / `FOR SHARE OF` clause.
    LockTarget(String),
    /// The `.1`th (0-based) expression of the `.0`th row of a `VALUES` list.
    ValuesItem(usize, usize),
    /// An expression, by its [`node_key`]: where it starts in the text (an expression's
    /// position is where it starts, whatever its operator or function).
    Expr(String),
    /// The reference as seen in the clause `.0` of the statement (`WHERE`, `HAVING`, ...),
    /// including inside its subqueries.
    InClause(&'static str, Box<ErrorRef>),
    /// One particular node: `.0` as written, with `.1` the node's [`node_key`]. Falls back to
    /// `.0` alone when the statement holds no node with that key.
    Keyed(Box<ErrorRef>, String),
    /// The `.1`th (0-based) occurrence of `.0`.
    Nth(Box<ErrorRef>, usize),
    /// The `.1`th (0-based) operand of the AND / OR expression `.0`.
    Operand(Box<ErrorRef>, usize),
    /// `.1` as found inside `.0` (the first `.1` at or after where `.0` starts).
    Within(Box<ErrorRef>, Box<ErrorRef>),
    /// Whichever of the alternatives comes first in the text (one thing spelled several ways).
    AnyOf(Vec<ErrorRef>),
}

impl ErrorRef {
    /// Whether `self` is `old` pinned down: the same reference, told apart from look-alikes.
    fn refines(&self, old: &ErrorRef) -> bool {
        match self {
            ErrorRef::Keyed(inner, _) | ErrorRef::InClause(_, inner) => inner.as_ref() == old,
            ErrorRef::Within(_, child) => child.as_ref() == old,
            ErrorRef::Operand(..) => matches!(old, ErrorRef::Expr(_)),
            _ => false,
        }
    }

    /// Whether the statement text can hold several different references of this kind that
    /// the name alone does not tell apart (an operator used twice, a literal repeated in
    /// contexts of different types): resolving by name needs exactly one match.
    fn needs_unique(&self) -> bool {
        match self {
            ErrorRef::Function(_) | ErrorRef::Operator(_) | ErrorRef::PrefixOperator(_) | ErrorRef::Literal(_) => true,
            ErrorRef::Column(_)
            | ErrorRef::Ungrouped(_)
            | ErrorRef::Target(_)
            | ErrorRef::Relation(_)
            | ErrorRef::Type(_)
            | ErrorRef::LockTarget(_)
            | ErrorRef::ValuesItem(..)
            | ErrorRef::Expr(_) => false,
            ErrorRef::InClause(_, inner) | ErrorRef::Keyed(inner, _) | ErrorRef::Nth(inner, _) => inner.needs_unique(),
            ErrorRef::Within(_, child) => child.needs_unique(),
            ErrorRef::Operand(..) => false,
            ErrorRef::AnyOf(any) => any.iter().any(ErrorRef::needs_unique),
        }
    }
}

/// A structural key of an IR node: its `Debug` text, or `None` when that is too long to be
/// worth comparing. Parser and binder both derive it from the same IR, so the binder can find
/// which of several equal-looking nodes in the text an error is about.
pub fn node_key(node: &dyn fmt::Debug) -> Option<String> {
    use fmt::Write;
    /// Longer nodes are not keyed (comparing them costs more than the position is worth).
    const CAP: usize = 240;
    struct Capped(String);
    impl Write for Capped {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if self.0.len() + s.len() > CAP {
                return Err(fmt::Error);
            }
            self.0.push_str(s);
            Ok(())
        }
    }
    let mut out = Capped(String::new());
    write!(out, "{node:?}").ok()?;
    Some(out.0)
}

/// The source positions of the references in one statement: `(ref, 1-based character offset
/// into the whole query string)`, in position order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceRefs(pub Vec<(ErrorRef, u32)>);

impl SourceRefs {
    /// Gives `e` the position of its subject (see [`SourceRefs::locate`]).
    pub fn resolve(&self, mut e: SqlError) -> SqlError {
        let Some(f) = e.fields.as_deref_mut() else { return e };
        let Some(subject) = f.subject.take() else { return e };
        if f.position.is_none() {
            f.position = self.locate(&subject);
        }
        e
    }

    /// Where `subject` is in the statement. A name several references share resolves to the
    /// first (statements are analyzed in text order) unless the kind of reference is one the
    /// name alone does not pin down: those need exactly one match, or a [`ErrorRef::Keyed`]
    /// subject, and otherwise have no position rather than a wrong one.
    pub fn locate(&self, subject: &ErrorRef) -> Option<u32> {
        match subject {
            ErrorRef::Keyed(inner, _) => self.nth(subject, 0).or_else(|| self.locate(inner)),
            ErrorRef::InClause(_, inner) => self.nth(subject, 0).or_else(|| self.locate(inner)),
            ErrorRef::Nth(inner, n) => self.nth(inner, *n),
            ErrorRef::Within(parent, child) => {
                let from = self.locate(parent)?;
                self.0.iter().find(|(r, p)| *p >= from && r == child.as_ref()).map(|(_, p)| *p)
            }
            ErrorRef::AnyOf(any) => any.iter().filter_map(|r| self.locate(r)).min(),
            other if other.needs_unique() => {
                let mut found = self.matching(other);
                match (found.next(), found.next()) {
                    (Some(p), None) => Some(p),
                    _ => None,
                }
            }
            other => self.nth(other, 0),
        }
    }

    fn matching<'a>(&'a self, subject: &'a ErrorRef) -> impl Iterator<Item = u32> + 'a {
        self.0.iter().filter(move |(r, _)| r == subject).map(|(_, p)| *p)
    }

    fn nth(&self, subject: &ErrorRef, n: usize) -> Option<u32> {
        self.matching(subject).nth(n)
    }
}

pub type SqlResult<T> = Result<T, SqlError>;

impl SqlError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        SqlError { code, message: message.into(), fields: None }
    }

    fn fields_mut(&mut self) -> &mut ErrorFields {
        self.fields.get_or_insert_with(Box::default)
    }

    pub fn fields(&self) -> Option<&ErrorFields> {
        self.fields.as_deref()
    }

    pub fn detail(&self) -> Option<&str> {
        self.fields().and_then(|f| f.detail.as_deref())
    }

    pub fn hint(&self) -> Option<&str> {
        self.fields().and_then(|f| f.hint.as_deref())
    }

    /// The 1-based character offset into the query string the error points at.
    pub fn position(&self) -> Option<u32> {
        self.fields().and_then(|f| f.position)
    }

    pub fn with_position(mut self, position: u32) -> Self {
        self.fields_mut().position = Some(position);
        self
    }

    /// Records what the error is about (see [`SourceRefs::resolve`]), unless the error already
    /// has a position or a subject: the innermost reference wins. A subject that pins down the
    /// one the error already has (see [`ErrorRef::refines`]) replaces it.
    pub fn about(mut self, subject: ErrorRef) -> Self {
        if let Some(f) = self.fields.as_deref_mut()
            && f.position.is_none()
            && let Some(old) = &f.subject
            && subject.refines(old)
        {
            f.subject = Some(subject);
            return self;
        }
        if self.fields().is_none_or(|f| f.position.is_none() && f.subject.is_none()) {
            self.fields_mut().subject = Some(subject);
        }
        self
    }

    /// The error about `subject`, when it is already about the one `subject` pins down.
    pub fn refined(self, subject: ErrorRef) -> Self {
        self.about(subject)
    }

    /// Takes over what `other` is about: for an error that replaces `other`'s message.
    pub fn subject_of(mut self, other: &SqlError) -> Self {
        if self.fields().is_none_or(|f| f.position.is_none() && f.subject.is_none())
            && let Some(f) = other.fields()
        {
            let (position, subject) = (f.position, f.subject.clone());
            let own = self.fields_mut();
            own.position = position;
            own.subject = subject;
        }
        self
    }

    /// Drops the position and subject: for errors raised while processing text other than the
    /// query string (function bodies, stored definitions), whose offsets mean nothing to it.
    pub fn without_position(mut self) -> Self {
        if let Some(f) = self.fields.as_deref_mut() {
            f.position = None;
            f.subject = None;
        }
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.fields_mut().detail = Some(detail.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.fields_mut().hint = Some(hint.into());
        self
    }

    /// Attaches the schema-qualified table an error is about.
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        let f = self.fields_mut();
        f.schema = Some("public".into());
        f.table = Some(table.into());
        self
    }

    pub fn with_column(mut self, column: impl Into<String>) -> Self {
        self.fields_mut().column = Some(column.into());
        self
    }

    pub fn with_constraint(mut self, constraint: impl Into<String>) -> Self {
        self.fields_mut().constraint = Some(constraint.into());
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        SqlError::new(code::INTERNAL_ERROR, message)
    }

    pub fn feature_not_supported(message: impl Into<String>) -> Self {
        SqlError::new(code::FEATURE_NOT_SUPPORTED, message)
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        SqlError::new(code::SYNTAX_ERROR, message)
    }

    /// `invalid input syntax for type <ty>: "<input>"` (22P02).
    pub fn invalid_text(ty: &str, input: &str) -> Self {
        SqlError::new(code::INVALID_TEXT_REPRESENTATION, format!("invalid input syntax for type {ty}: \"{input}\""))
    }

    /// `<ty> out of range` (22003).
    pub fn out_of_range(ty: &str) -> Self {
        SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, format!("{ty} out of range"))
    }

    pub fn division_by_zero() -> Self {
        SqlError::new(code::DIVISION_BY_ZERO, "division by zero")
    }

    pub fn undefined_table(name: &str) -> Self {
        SqlError::new(code::UNDEFINED_TABLE, format!("relation \"{name}\" does not exist"))
    }

    pub fn undefined_column(name: &str) -> Self {
        SqlError::new(code::UNDEFINED_COLUMN, format!("column {name} does not exist"))
    }

    pub fn undefined_function(message: impl Into<String>) -> Self {
        SqlError::new(code::UNDEFINED_FUNCTION, message)
    }

    pub fn datatype_mismatch(message: impl Into<String>) -> Self {
        SqlError::new(code::DATATYPE_MISMATCH, message)
    }

    pub fn cannot_coerce(from: &str, to: &str) -> Self {
        SqlError::new(code::CANNOT_COERCE, format!("cannot cast type {from} to {to}"))
    }
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SqlError {}

/// SQLSTATE codes used by pgmem. Names follow Appendix A of the Postgres manual.
pub mod code {
    pub const SUCCESSFUL_COMPLETION: &str = "00000";
    pub const WARNING: &str = "01000";
    pub const CARDINALITY_VIOLATION: &str = "21000";
    pub const DATA_EXCEPTION: &str = "22000";
    pub const STRING_DATA_RIGHT_TRUNCATION: &str = "22001";
    pub const NUMERIC_VALUE_OUT_OF_RANGE: &str = "22003";
    pub const NULL_VALUE_NOT_ALLOWED: &str = "22004";
    pub const INVALID_DATETIME_FORMAT: &str = "22007";
    pub const DATETIME_FIELD_OVERFLOW: &str = "22008";
    pub const INVALID_TIME_ZONE_DISPLACEMENT_VALUE: &str = "22009";
    pub const DIVISION_BY_ZERO: &str = "22012";
    pub const INVALID_PARAMETER_VALUE: &str = "22023";
    pub const CANT_CHANGE_RUNTIME_PARAM: &str = "55P02";
    pub const INVALID_REGULAR_EXPRESSION: &str = "2201B";
    pub const ARRAY_SUBSCRIPT_ERROR: &str = "2202E";
    pub const INVALID_TEXT_REPRESENTATION: &str = "22P02";
    pub const INVALID_BINARY_REPRESENTATION: &str = "22P03";
    pub const INVALID_JSON_TEXT: &str = "22P02";
    pub const INTEGRITY_CONSTRAINT_VIOLATION: &str = "23000";
    pub const RESTRICT_VIOLATION: &str = "23001";
    pub const NOT_NULL_VIOLATION: &str = "23502";
    pub const FOREIGN_KEY_VIOLATION: &str = "23503";
    pub const UNIQUE_VIOLATION: &str = "23505";
    pub const CHECK_VIOLATION: &str = "23514";
    pub const INVALID_CURSOR_STATE: &str = "24000";
    pub const ACTIVE_SQL_TRANSACTION: &str = "25001";
    pub const NO_ACTIVE_SQL_TRANSACTION: &str = "25P01";
    pub const IN_FAILED_SQL_TRANSACTION: &str = "25P02";
    pub const INVALID_SQL_STATEMENT_NAME: &str = "26000";
    pub const INVALID_CATALOG_NAME: &str = "3D000";
    pub const INVALID_CURSOR_NAME: &str = "34000";
    pub const INVALID_SAVEPOINT_SPECIFICATION: &str = "3B001";
    pub const SERIALIZATION_FAILURE: &str = "40001";
    pub const LOCK_NOT_AVAILABLE: &str = "55P03";
    pub const UNSAFE_NEW_ENUM_VALUE_USAGE: &str = "55P04";
    pub const READ_ONLY_SQL_TRANSACTION: &str = "25006";
    pub const DEADLOCK_DETECTED: &str = "40P01";
    pub const SYNTAX_ERROR: &str = "42601";
    pub const INSUFFICIENT_PRIVILEGE: &str = "42501";
    pub const CANNOT_COERCE: &str = "42846";
    pub const GROUPING_ERROR: &str = "42803";
    pub const WINDOWING_ERROR: &str = "42P20";
    pub const INVALID_RECURSION: &str = "42P19";
    pub const INVALID_FOREIGN_KEY: &str = "42830";
    pub const INVALID_NAME: &str = "42602";
    pub const INVALID_SCHEMA_NAME: &str = "3F000";
    pub const NAME_TOO_LONG: &str = "42622";
    pub const RESERVED_NAME: &str = "42939";
    pub const DATATYPE_MISMATCH: &str = "42804";
    pub const INDETERMINATE_DATATYPE: &str = "42P18";
    pub const COLLATION_MISMATCH: &str = "42P21";
    pub const WRONG_OBJECT_TYPE: &str = "42809";
    pub const GENERATED_ALWAYS: &str = "428C9";
    pub const UNDEFINED_COLUMN: &str = "42703";
    pub const UNDEFINED_FUNCTION: &str = "42883";
    pub const UNDEFINED_TABLE: &str = "42P01";
    pub const UNDEFINED_PARAMETER: &str = "42P02";
    pub const UNDEFINED_OBJECT: &str = "42704";
    pub const DUPLICATE_COLUMN: &str = "42701";
    pub const DUPLICATE_CURSOR: &str = "42P03";
    pub const DUPLICATE_DATABASE: &str = "42P04";
    pub const DUPLICATE_PREPARED_STATEMENT: &str = "42P05";
    pub const DUPLICATE_SCHEMA: &str = "42P06";
    pub const UNTRANSLATABLE_CHARACTER: &str = "22P05";
    pub const PROGRAM_LIMIT_EXCEEDED: &str = "54000";
    pub const DUPLICATE_TABLE: &str = "42P07";
    pub const DUPLICATE_ALIAS: &str = "42712";
    pub const DUPLICATE_OBJECT: &str = "42710";
    pub const AMBIGUOUS_COLUMN: &str = "42702";
    pub const AMBIGUOUS_FUNCTION: &str = "42725";
    pub const AMBIGUOUS_PARAMETER: &str = "42P08";
    pub const INVALID_COLUMN_REFERENCE: &str = "42P10";
    pub const INVALID_COLUMN_DEFINITION: &str = "42611";
    pub const INVALID_TABLE_DEFINITION: &str = "42P16";
    pub const DEPENDENT_OBJECTS_STILL_EXIST: &str = "2BP01";
    pub const INVALID_OBJECT_DEFINITION: &str = "42P17";
    pub const EXCLUSION_VIOLATION: &str = "23P01";
    pub const STRING_DATA_LENGTH_MISMATCH: &str = "22026";
    pub const STATEMENT_TOO_COMPLEX: &str = "54001";
    pub const INVALID_FUNCTION_DEFINITION: &str = "42P13";
    pub const DUPLICATE_FUNCTION: &str = "42723";
    pub const INVALID_TRANSACTION_TERMINATION: &str = "2D000";
    pub const CASE_NOT_FOUND: &str = "20000";
    pub const ASSERT_FAILURE: &str = "P0004";
    pub const NO_DATA_FOUND: &str = "P0002";
    pub const TOO_MANY_ROWS: &str = "P0003";
    pub const STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER: &str = "0Z002";
    pub const RAISE_EXCEPTION: &str = "P0001";
    pub const FUNCTION_EXECUTED_NO_RETURN_STATEMENT: &str = "2F005";
    pub const TRIGGERED_DATA_CHANGE_VIOLATION: &str = "27000";
    pub const INVALID_GRANT_OPERATION: &str = "0LP01";
    pub const OBJECT_IN_USE: &str = "55006";
    pub const PROTOCOL_VIOLATION: &str = "08P01";
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "55000";
    pub const QUERY_CANCELED: &str = "57014";
    pub const BAD_COPY_FILE_FORMAT: &str = "22P04";
    pub const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";
    pub const UNDEFINED_FILE: &str = "58P01";
    pub const IO_ERROR: &str = "58030";
    pub const INTERNAL_ERROR: &str = "XX000";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(items: &[(ErrorRef, u32)]) -> SourceRefs {
        SourceRefs(items.to_vec())
    }

    fn column(name: &str) -> ErrorRef {
        ErrorRef::Column(name.to_owned())
    }

    fn operator(name: &str) -> ErrorRef {
        ErrorRef::Operator(name.to_owned())
    }

    #[test]
    fn a_repeated_name_resolves_to_its_first_mention() {
        let r = refs(&[(column("a"), 8), (column("a"), 20)]);
        assert_eq!(r.locate(&column("a")), Some(8));
        assert_eq!(r.locate(&column("b")), None);
    }

    #[test]
    fn kinds_a_name_does_not_pin_down_need_exactly_one_match() {
        let r = refs(&[(operator("+"), 10), (operator("+"), 17), (operator("-"), 30)]);
        assert_eq!(r.locate(&operator("+")), None);
        assert_eq!(r.locate(&operator("-")), Some(30));
    }

    #[test]
    fn a_keyed_subject_picks_its_node_and_falls_back_to_the_unique_name() {
        let keyed = |key: &str| ErrorRef::Keyed(Box::new(operator("+")), key.to_owned());
        let r = refs(&[(keyed("x"), 10), (operator("+"), 10), (keyed("y"), 17), (operator("+"), 17)]);
        assert_eq!(r.locate(&keyed("y")), Some(17));
        // No node with that key: the name is ambiguous, so there is no position.
        assert_eq!(r.locate(&keyed("z")), None);
        let one = refs(&[(operator("+"), 10)]);
        assert_eq!(one.locate(&keyed("z")), Some(10));
    }

    #[test]
    fn nth_within_and_any_of() {
        let r = refs(&[(column("a"), 5), (ErrorRef::Expr("p".into()), 12), (ErrorRef::Expr("c".into()), 20), (column("a"), 30)]);
        assert_eq!(r.locate(&ErrorRef::Nth(Box::new(column("a")), 1)), Some(30));
        let within = ErrorRef::Within(Box::new(ErrorRef::Expr("p".into())), Box::new(ErrorRef::Expr("c".into())));
        assert_eq!(r.locate(&within), Some(20));
        assert_eq!(r.locate(&ErrorRef::AnyOf(vec![column("zz"), column("a")])), Some(5));
    }

    #[test]
    fn a_pinned_down_subject_replaces_the_plain_one_but_the_innermost_wins() {
        let plain = SqlError::new("42883", "x").about(operator("+"));
        let keyed = ErrorRef::Keyed(Box::new(operator("+")), "k".into());
        let refined = plain.about(keyed.clone());
        assert_eq!(refined.fields().and_then(|f| f.subject.clone()), Some(keyed));
        let first = SqlError::new("42883", "x").about(column("a")).about(column("b"));
        assert_eq!(first.fields().and_then(|f| f.subject.clone()), Some(column("a")));
    }

    #[test]
    fn node_keys_are_capped() {
        assert!(node_key(&"short").is_some());
        assert!(node_key(&"x".repeat(1000)).is_none());
    }
}
