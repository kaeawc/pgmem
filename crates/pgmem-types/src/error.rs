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
    pub const INVALID_CURSOR_NAME: &str = "34000";
    pub const INVALID_SAVEPOINT_SPECIFICATION: &str = "3B001";
    pub const SERIALIZATION_FAILURE: &str = "40001";
    pub const LOCK_NOT_AVAILABLE: &str = "55P03";
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
    pub const PROTOCOL_VIOLATION: &str = "08P01";
    pub const FEATURE_NOT_SUPPORTED: &str = "0A000";
    pub const OBJECT_NOT_IN_PREREQUISITE_STATE: &str = "55000";
    pub const QUERY_CANCELED: &str = "57014";
    pub const INTERNAL_ERROR: &str = "XX000";
}
