//! Sequence parameters (Postgres' `init_params`): CREATE / ALTER SEQUENCE and identity column
//! options, with Postgres' validation messages.

use pgmem_catalog::{Catalog, SequenceDef};
use pgmem_ir as ir;
use pgmem_types::{SqlError, SqlResult, Type, code};

/// The value range of a sequence data type.
pub fn type_bounds(t: &Type) -> (i64, i64) {
    match t {
        Type::Int2 => (i64::from(i16::MIN), i64::from(i16::MAX)),
        Type::Int4 => (i64::from(i32::MIN), i64::from(i32::MAX)),
        _ => (i64::MIN, i64::MAX),
    }
}

fn invalid(msg: String) -> SqlError {
    SqlError::new(code::INVALID_PARAMETER_VALUE, msg)
}

/// Applies `opts` to `seq` (a new sequence with defaults when `creating`, else the current
/// definition). Returns where to restart, if RESTART was given (None = the start value).
pub fn apply_options(catalog: &Catalog, seq: &mut SequenceDef, opts: &ir::SequenceOptions, creating: bool) -> SqlResult<Option<i64>> {
    let ty = match &opts.data_type {
        Some(t) => {
            let (ty, _) = crate::bind::resolve_type_name(catalog, t)?;
            if !matches!(ty, Type::Int2 | Type::Int4 | Type::Int8) {
                return Err(invalid("sequence type must be smallint, integer, or bigint".into()));
            }
            Some(ty)
        }
        None => None,
    };
    apply(seq, opts, ty, creating)
}

/// The identity sequence of a column ALTER COLUMN TYPE gave type `ty` (`AS ty`, as
/// `ATExecAlterColumnType` alters it): bounds at the old type's limits move to the new type's,
/// and the current value `last_value` must fit the result.
pub fn retype(seq: &mut SequenceDef, ty: Type, last_value: i64) -> SqlResult<()> {
    apply(seq, &ir::SequenceOptions::default(), Some(ty), false)?;
    check_restart(seq, last_value)
}

/// Postgres checks the restart value (or the current value, when altering) against the bounds.
pub fn check_restart(seq: &SequenceDef, r: i64) -> SqlResult<()> {
    if r < seq.min {
        return Err(invalid(format!("RESTART value ({r}) cannot be less than MINVALUE ({})", seq.min)));
    }
    if r > seq.max {
        return Err(invalid(format!("RESTART value ({r}) cannot be greater than MAXVALUE ({})", seq.max)));
    }
    Ok(())
}

fn apply(seq: &mut SequenceDef, opts: &ir::SequenceOptions, ty: Option<Type>, creating: bool) -> SqlResult<Option<i64>> {
    let old_type = seq.data_type.clone();
    if let Some(ty) = ty {
        seq.data_type = ty;
    }
    if let Some(inc) = opts.increment {
        if inc == 0 {
            return Err(invalid("INCREMENT must not be zero".into()));
        }
        seq.increment = inc;
    }
    let (type_min, type_max) = type_bounds(&seq.data_type);
    let (old_min, old_max) = type_bounds(&old_type);
    let ascending = seq.increment > 0;
    // Bounds left at the old type's limits follow a type change.
    match opts.max {
        Some(Some(v)) => seq.max = v,
        Some(None) => seq.max = if ascending { type_max } else { -1 },
        None if creating => seq.max = if ascending { type_max } else { -1 },
        None if seq.data_type != old_type && seq.max == old_max => seq.max = type_max,
        None => {}
    }
    match opts.min {
        Some(Some(v)) => seq.min = v,
        Some(None) => seq.min = if ascending { 1 } else { type_min },
        None if creating => seq.min = if ascending { 1 } else { type_min },
        None if seq.data_type != old_type && seq.min == old_min => seq.min = type_min,
        None => {}
    }
    let type_name = seq.data_type.name();
    if seq.max < type_min || seq.max > type_max {
        return Err(invalid(format!("MAXVALUE ({}) is out of range for sequence data type {type_name}", seq.max)));
    }
    if seq.min < type_min || seq.min > type_max {
        return Err(invalid(format!("MINVALUE ({}) is out of range for sequence data type {type_name}", seq.min)));
    }
    if seq.min >= seq.max {
        return Err(invalid(format!("MINVALUE ({}) must be less than MAXVALUE ({})", seq.min, seq.max)));
    }
    match opts.start {
        Some(v) => seq.start = v,
        None if creating => seq.start = if ascending { seq.min } else { seq.max },
        None => {}
    }
    if seq.start < seq.min {
        return Err(invalid(format!("START value ({}) cannot be less than MINVALUE ({})", seq.start, seq.min)));
    }
    if seq.start > seq.max {
        return Err(invalid(format!("START value ({}) cannot be greater than MAXVALUE ({})", seq.start, seq.max)));
    }
    let restart = opts.restart.map(|r| r.unwrap_or(seq.start));
    if let Some(r) = restart {
        check_restart(seq, r)?;
    }
    if let Some(c) = opts.cache {
        if c <= 0 {
            return Err(invalid(format!("CACHE ({c}) must be greater than zero")));
        }
        seq.cache = c;
    }
    if let Some(c) = opts.cycle {
        seq.cycle = c;
    }
    Ok(restart)
}

/// `OWNED BY table.column` for a sequence in `schema`: the (table, column) it records.
pub fn resolve_owner(catalog: &Catalog, schema: &str, name: &ir::ObjectName) -> SqlResult<(String, String)> {
    let Some((column, table)) = name.0.split_last() else { return Err(SqlError::syntax("invalid OWNED BY option")) };
    if table.is_empty() {
        return Err(SqlError::syntax("invalid OWNED BY option").with_hint("Specify OWNED BY table.column or OWNED BY NONE."));
    }
    let table = catalog.lookup_table(&ir::ObjectName(table.to_vec()))?;
    if table.schema != schema {
        return Err(SqlError::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "sequence must be in same schema as table it is linked to"));
    }
    if table.column_index(column).is_none() {
        return Err(SqlError::undefined_column(&format!("\"{column}\" of relation \"{}\"", table.name)));
    }
    Ok((table.name.clone(), column.clone()))
}
