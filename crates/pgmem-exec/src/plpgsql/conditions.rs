//! PL/pgSQL exception condition names (`plerrcodes.h`) and SQLSTATE matching.

/// Condition name → SQLSTATE, for the conditions applications commonly catch.
const CONDITIONS: &[(&str, &str)] = &[
    ("successful_completion", "00000"),
    ("warning", "01000"),
    ("no_data", "02000"),
    ("sql_statement_not_yet_complete", "03000"),
    ("connection_exception", "08000"),
    ("feature_not_supported", "0A000"),
    ("invalid_transaction_initiation", "0B000"),
    ("cardinality_violation", "21000"),
    ("data_exception", "22000"),
    ("array_subscript_error", "2202E"),
    ("character_not_in_repertoire", "22021"),
    ("datetime_field_overflow", "22008"),
    ("division_by_zero", "22012"),
    ("invalid_datetime_format", "22007"),
    ("invalid_parameter_value", "22023"),
    ("invalid_text_representation", "22P02"),
    ("invalid_binary_representation", "22P03"),
    ("null_value_not_allowed", "22004"),
    ("numeric_value_out_of_range", "22003"),
    ("string_data_right_truncation", "22001"),
    ("invalid_regular_expression", "2201B"),
    ("invalid_json_text", "22032"),
    ("integrity_constraint_violation", "23000"),
    ("restrict_violation", "23001"),
    ("not_null_violation", "23502"),
    ("foreign_key_violation", "23503"),
    ("unique_violation", "23505"),
    ("check_violation", "23514"),
    ("exclusion_violation", "23P01"),
    ("invalid_cursor_state", "24000"),
    ("invalid_transaction_state", "25000"),
    ("active_sql_transaction", "25001"),
    ("read_only_sql_transaction", "25006"),
    ("in_failed_sql_transaction", "25P02"),
    ("invalid_authorization_specification", "28000"),
    ("invalid_transaction_termination", "2D000"),
    ("invalid_cursor_name", "34000"),
    ("invalid_schema_name", "3F000"),
    ("transaction_rollback", "40000"),
    ("serialization_failure", "40001"),
    ("deadlock_detected", "40P01"),
    ("syntax_error_or_access_rule_violation", "42000"),
    ("syntax_error", "42601"),
    ("insufficient_privilege", "42501"),
    ("undefined_column", "42703"),
    ("undefined_function", "42883"),
    ("undefined_table", "42P01"),
    ("undefined_parameter", "42P02"),
    ("undefined_object", "42704"),
    ("duplicate_column", "42701"),
    ("duplicate_cursor", "42P03"),
    ("duplicate_database", "42P04"),
    ("duplicate_function", "42723"),
    ("duplicate_schema", "42P06"),
    ("duplicate_table", "42P07"),
    ("duplicate_alias", "42712"),
    ("duplicate_object", "42710"),
    ("ambiguous_column", "42702"),
    ("ambiguous_function", "42725"),
    ("datatype_mismatch", "42804"),
    ("wrong_object_type", "42809"),
    ("invalid_column_reference", "42P10"),
    ("invalid_table_definition", "42P16"),
    ("invalid_object_definition", "42P17"),
    ("insufficient_resources", "53000"),
    ("program_limit_exceeded", "54000"),
    ("statement_too_complex", "54001"),
    ("object_not_in_prerequisite_state", "55000"),
    ("lock_not_available", "55P03"),
    ("operator_intervention", "57000"),
    ("query_canceled", "57014"),
    ("system_error", "58000"),
    ("plpgsql_error", "P0000"),
    ("raise_exception", "P0001"),
    ("no_data_found", "P0002"),
    ("too_many_rows", "P0003"),
    ("assert_failure", "P0004"),
    ("internal_error", "XX000"),
];

/// SQLSTATE codes not known statically (`RAISE ... USING ERRCODE = '22023'`), leaked once.
pub fn intern_code(c: &str) -> &'static str {
    static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if let Some((_, k)) = CONDITIONS.iter().find(|(_, k)| *k == c) {
        return k;
    }
    let mut seen = SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(k) = seen.iter().find(|k| **k == c) {
        return k;
    }
    let k: &'static str = Box::leak(c.to_owned().into_boxed_str());
    seen.push(k);
    k
}

/// The SQLSTATE of a condition name (or `sqlstate:XXXXX`).
pub fn condition_code(name: &str) -> Option<&'static str> {
    if let Some(s) = name.strip_prefix("sqlstate:") {
        return Some(intern_code(&s.to_uppercase()));
    }
    CONDITIONS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// Whether handler condition `cond` catches an error with SQLSTATE `sqlstate`: `others`
/// catches everything but cancellation and assertion failures; a class code (`22000`)
/// catches its class.
pub fn condition_matches(cond: &str, sqlstate: &str) -> bool {
    if cond == "others" {
        return sqlstate != "57014" && sqlstate != "P0004";
    }
    let Some(c) = condition_code(cond) else { return false };
    if c == sqlstate {
        return true;
    }
    c.ends_with("000") && sqlstate.starts_with(&c[..2])
}
