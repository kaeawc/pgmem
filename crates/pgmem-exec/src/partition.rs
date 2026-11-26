//! Declarative partitioning (`partbounds.c`, `execPartition.c`, `tablecmds.c`): partition keys
//! and bounds, row routing and partition constraints, the DDL that builds partition trees, and
//! DML on partitioned tables, which runs on their partitions (a partitioned table stores no
//! rows itself).

use crate::bind::{Binder, Ctx, Level, Scope};
use crate::cast::CoercionContext;
use crate::eval::{EvalCtx, StatementState, eval};
use crate::plan::Row;
use crate::session::{QueryResult, SessionEnv};
use pgmem_catalog::{
    Catalog, ColumnDef, ConstraintKind, IndexDef, PartitionBound, PartitionKey, PartitionKeyPart, PartitionOf, PartitionStrategy,
    RangeDatum, Relation, TableDef,
};
use pgmem_ir::{self as ir, Statement};
use pgmem_storage::{RowId, Txn};
use pgmem_types::{Format, SqlError, SqlResult, Type, Value, code, compare_values};
use std::cmp::Ordering;
use std::sync::Arc;

fn bad_definition(msg: impl Into<String>) -> SqlError {
    SqlError::new(code::INVALID_OBJECT_DEFINITION, msg)
}

fn bad_bound(msg: impl Into<String>) -> SqlError {
    SqlError::new(code::INVALID_TABLE_DEFINITION, msg)
}

fn top_ctx<R>(f: impl FnOnce(&Ctx) -> R) -> R {
    let level = Level::new(Scope::default());
    f(&Ctx::new(&level))
}

// ---------------------------------------------------------------------------------------------
// Keys

/// Binds a partition key expression against the table's row.
fn bind_key_expr(b: &Binder, table: &TableDef, e: &ir::Expr) -> SqlResult<(crate::plan::Expr, Type)> {
    let level = Level::new(crate::bind::dml::physical_scope(table, &table.name));
    b.bind_expr(e, &Ctx::new(&level))
}

/// `PARTITION BY strategy (keys)` for `table`.
pub(crate) fn bind_key(b: &Binder, table: &TableDef, spec: &ir::PartitionSpec) -> SqlResult<PartitionKey> {
    let strategy = match spec.strategy.as_str() {
        "range" => PartitionStrategy::Range,
        "list" => PartitionStrategy::List,
        _ => PartitionStrategy::Hash,
    };
    if strategy == PartitionStrategy::List && spec.keys.len() > 1 {
        return Err(bad_definition("cannot use \"list\" partition strategy with more than one column"));
    }
    let mut parts = Vec::new();
    for e in &spec.keys {
        match e {
            ir::Expr::Column(names) if names.len() == 1 => {
                let slot = table.column_index(&names[0]).ok_or_else(|| {
                    SqlError::new(code::UNDEFINED_COLUMN, format!("column \"{}\" named in partition key does not exist", names[0]))
                })?;
                parts.push(PartitionKeyPart::Column(slot));
            }
            other => {
                bind_key_expr(b, table, other)?;
                parts.push(PartitionKeyPart::Expr(other.clone()));
            }
        }
    }
    let key = PartitionKey { strategy, parts };
    if strategy == PartitionStrategy::Hash {
        for t in key_types(b, table, &key)? {
            if hash_value(&Value::Null, &t).is_none() && !hashable(&t) {
                return Err(SqlError::new(
                    code::UNDEFINED_OBJECT,
                    format!("data type {} has no default operator class for access method \"hash\"", t.name()),
                ));
            }
        }
    }
    Ok(key)
}

pub(crate) fn key_types(b: &Binder, table: &TableDef, key: &PartitionKey) -> SqlResult<Vec<Type>> {
    key.parts
        .iter()
        .map(|p| match p {
            PartitionKeyPart::Column(slot) => Ok(table.columns[*slot].ty.clone()),
            PartitionKeyPart::Expr(e) => bind_key_expr(b, table, e).map(|(_, t)| t),
        })
        .collect()
}

/// The partition key values of a row of `table` (its physical layout).
fn key_values(b: &Binder, table: &TableDef, key: &PartitionKey, row: &Row, ectx: &EvalCtx) -> SqlResult<Vec<Value>> {
    key.parts
        .iter()
        .map(|p| match p {
            PartitionKeyPart::Column(slot) => Ok(row.get(*slot).cloned().unwrap_or(Value::Null)),
            PartitionKeyPart::Expr(e) => {
                let (bound, _) = bind_key_expr(b, table, e)?;
                eval(&bound, row, ectx)
            }
        })
        .collect()
}

/// How a key part prints (`pg_get_partkeydef`, routing errors).
fn key_part_text(catalog: &Catalog, table: &TableDef, part: &PartitionKeyPart, partkeydef: bool) -> String {
    match part {
        PartitionKeyPart::Column(slot) => crate::funcs::string::quote_ident(&table.columns[*slot].name),
        PartitionKeyPart::Expr(e) => {
            let text = crate::deparse::deparse_expr(catalog, table, e).unwrap_or_default();
            // A function call prints as is; other expressions keep their parentheses.
            if partkeydef && !matches!(e, ir::Expr::Function(_)) { format!("({text})") } else { text }
        }
    }
}

/// `pg_get_partkeydef`: `RANGE (at)`.
pub fn key_definition(catalog: &Catalog, table: &TableDef) -> Option<String> {
    let key = catalog.partition_key(table.oid)?;
    let parts: Vec<String> = key.parts.iter().map(|p| key_part_text(catalog, table, p, true)).collect();
    Some(format!("{} ({})", key.strategy.keyword(), parts.join(", ")))
}

// ---------------------------------------------------------------------------------------------
// Bounds

/// A partition bound constant: bound without columns in scope, coerced to the key type.
fn bound_value(b: &Binder, e: &ir::Expr, ty: &Type, ectx: &EvalCtx) -> SqlResult<Value> {
    let (bound, from) = top_ctx(|ctx| b.bind_expr(e, ctx)).map_err(|err| {
        if err.code == code::UNDEFINED_COLUMN {
            SqlError::feature_not_supported("cannot use column reference in partition bound expression")
        } else {
            err
        }
    })?;
    let coerced = crate::bind::coerce(b, bound, &from, ty, CoercionContext::Assignment)?;
    eval(&coerced, &[], ectx)
}

fn value_text(v: &Value, ty: &Type) -> String {
    if v.is_null() {
        return "NULL".into();
    }
    let text = pgmem_types::encode(v, ty, Format::Text).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    let bare = matches!(ty, Type::Int2 | Type::Int4 | Type::Int8 | Type::Numeric | Type::Float4 | Type::Float8 | Type::Oid)
        && !text.starts_with('-');
    match ty {
        Type::Bool => text.replace('t', "true").replace('f', "false"),
        _ if bare => text,
        _ => format!("'{}'", text.replace('\'', "''")),
    }
}

fn datum_text(d: &RangeDatum, ty: &Type) -> String {
    match d {
        RangeDatum::MinValue => "MINVALUE".into(),
        RangeDatum::MaxValue => "MAXVALUE".into(),
        RangeDatum::Value(v) => value_text(v, ty),
    }
}

/// `pg_get_expr(relpartbound)`: `FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')`.
pub fn bound_text(bound: &PartitionBound, types: &[Type]) -> String {
    let ty = |i: usize| types.get(i).cloned().unwrap_or(Type::Text);
    match bound {
        PartitionBound::Default => "DEFAULT".into(),
        PartitionBound::List(values) => {
            let items: Vec<String> = values.iter().map(|v| value_text(v, &ty(0))).collect();
            format!("FOR VALUES IN ({})", items.join(", "))
        }
        PartitionBound::Range { from, to } => {
            let side = |ds: &[RangeDatum]| ds.iter().enumerate().map(|(i, d)| datum_text(d, &ty(i))).collect::<Vec<_>>().join(", ");
            format!("FOR VALUES FROM ({}) TO ({})", side(from), side(to))
        }
        PartitionBound::Hash { modulus, remainder } => format!("FOR VALUES WITH (modulus {modulus}, remainder {remainder})"),
    }
}

/// The relpartbound text of a partition (with its parent's key types).
pub fn partition_bound_text(catalog: &Catalog, table: &TableDef) -> Option<String> {
    let of = catalog.partition_of(table.oid)?;
    let parent = catalog.table_by_oid(of.parent)?;
    let b = Binder::new(catalog, &[]);
    let types = catalog.partition_key(parent.oid).map(|k| key_types(&b, parent, k).unwrap_or_default()).unwrap_or_default();
    Some(bound_text(&of.bound, &types))
}

fn strategy_word(s: PartitionStrategy) -> &'static str {
    match s {
        PartitionStrategy::Range => "range",
        PartitionStrategy::List => "list",
        PartitionStrategy::Hash => "hash",
    }
}

fn range_side(b: &Binder, items: &[ir::RangeBoundSpec], types: &[Type], which: &str, ectx: &EvalCtx) -> SqlResult<Vec<RangeDatum>> {
    if items.len() != types.len() {
        return Err(bad_bound(format!("{which} must specify exactly one value per partitioning column")));
    }
    let mut out = Vec::new();
    for (item, ty) in items.iter().zip(types) {
        out.push(match item {
            ir::RangeBoundSpec::MinValue => RangeDatum::MinValue,
            ir::RangeBoundSpec::MaxValue => RangeDatum::MaxValue,
            ir::RangeBoundSpec::Expr(e) => {
                let v = bound_value(b, e, ty, ectx)?;
                if v.is_null() {
                    return Err(bad_bound("cannot specify NULL in range bound"));
                }
                RangeDatum::Value(v)
            }
        });
    }
    // After MINVALUE / MAXVALUE every datum must be the same infinity.
    for w in out.windows(2) {
        match (&w[0], &w[1]) {
            (RangeDatum::MinValue, d) if *d != RangeDatum::MinValue => {
                return Err(bad_bound("every bound following MINVALUE must also be MINVALUE"));
            }
            (RangeDatum::MaxValue, d) if *d != RangeDatum::MaxValue => {
                return Err(bad_bound("every bound following MAXVALUE must also be MAXVALUE"));
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Compares two range bound tuples (`partition_rbound_cmp`, without the lower/upper tie-break).
fn cmp_bounds(a: &[RangeDatum], b: &[RangeDatum]) -> Ordering {
    let rank = |d: &RangeDatum| match d {
        RangeDatum::MinValue => 0,
        RangeDatum::Value(_) => 1,
        RangeDatum::MaxValue => 2,
    };
    for (x, y) in a.iter().zip(b) {
        match rank(x).cmp(&rank(y)) {
            Ordering::Equal => {}
            other => return other,
        }
        match (x, y) {
            (RangeDatum::Value(x), RangeDatum::Value(y)) => match compare_values(x, y).unwrap_or(Ordering::Equal) {
                Ordering::Equal => {}
                other => return other,
            },
            _ => return Ordering::Equal,
        }
    }
    Ordering::Equal
}

/// Compares a bound tuple with a key tuple.
fn cmp_bound_key(bound: &[RangeDatum], key: &[Value]) -> Ordering {
    for (d, v) in bound.iter().zip(key) {
        match d {
            RangeDatum::MinValue => return Ordering::Less,
            RangeDatum::MaxValue => return Ordering::Greater,
            RangeDatum::Value(x) => match compare_values(x, v).unwrap_or(Ordering::Equal) {
                Ordering::Equal => {}
                other => return other,
            },
        }
    }
    Ordering::Equal
}

/// A partition bound from its specification, checked against the key.
pub(crate) fn bind_bound(
    b: &Binder,
    parent: &TableDef,
    key: &PartitionKey,
    spec: &ir::PartitionBoundSpec,
    name: &str,
    ectx: &EvalCtx,
) -> SqlResult<PartitionBound> {
    let types = key_types(b, parent, key)?;
    let wrong = || bad_bound(format!("invalid bound specification for a {} partition", strategy_word(key.strategy)));
    Ok(match spec {
        ir::PartitionBoundSpec::Default if key.strategy == PartitionStrategy::Hash => {
            return Err(bad_bound("a hash-partitioned table may not have a default partition"));
        }
        ir::PartitionBoundSpec::Default => PartitionBound::Default,
        ir::PartitionBoundSpec::In(values) => {
            if key.strategy != PartitionStrategy::List {
                return Err(wrong());
            }
            let mut out: Vec<Value> = Vec::new();
            for e in values {
                let v = bound_value(b, e, &types[0], ectx)?;
                if !out.iter().any(|x| same_value(x, &v)) {
                    out.push(v);
                }
            }
            PartitionBound::List(out)
        }
        ir::PartitionBoundSpec::Range { from, to } => {
            if key.strategy != PartitionStrategy::Range {
                return Err(wrong());
            }
            let from = range_side(b, from, &types, "FROM", ectx)?;
            let to = range_side(b, to, &types, "TO", ectx)?;
            if cmp_bounds(&from, &to) != Ordering::Less {
                let side = |ds: &[RangeDatum]| ds.iter().enumerate().map(|(i, d)| datum_text(d, &types[i])).collect::<Vec<_>>().join(", ");
                return Err(bad_definition(format!("empty range bound specified for partition \"{name}\"")).with_detail(format!(
                    "Specified lower bound ({}) is greater than or equal to upper bound ({}).",
                    side(&from),
                    side(&to)
                )));
            }
            PartitionBound::Range { from, to }
        }
        ir::PartitionBoundSpec::Hash { modulus, remainder } => {
            if key.strategy != PartitionStrategy::Hash {
                return Err(wrong());
            }
            let modulus = u32::try_from(*modulus)
                .ok()
                .filter(|m| *m > 0)
                .ok_or_else(|| bad_bound("modulus for hash partition must be an integer value greater than zero"))?;
            let remainder = u32::try_from(*remainder).unwrap_or(u32::MAX);
            if remainder >= modulus {
                return Err(bad_bound("remainder for hash partition must be less than modulus"));
            }
            PartitionBound::Hash { modulus, remainder }
        }
    })
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a.is_null(), b.is_null()) {
        (true, true) => true,
        (false, false) => compare_values(a, b).is_ok_and(|o| o == Ordering::Equal),
        _ => false,
    }
}

/// Rejects a bound that overlaps a sibling's (`check_new_partition_bound`).
pub(crate) fn check_overlap(catalog: &Catalog, parent: &TableDef, bound: &PartitionBound, name: &str) -> SqlResult<()> {
    let overlap = |other: &str| bad_definition(format!("partition \"{name}\" would overlap partition \"{other}\""));
    for (sibling, of) in catalog.partitions(parent.oid) {
        match (bound, &of.bound) {
            (PartitionBound::Default, PartitionBound::Default) => {
                return Err(bad_definition(format!("partition \"{name}\" conflicts with existing default partition \"{}\"", sibling.name)));
            }
            (PartitionBound::List(new), PartitionBound::List(old)) if new.iter().any(|v| old.iter().any(|o| same_value(v, o))) => {
                return Err(overlap(&sibling.name));
            }
            (PartitionBound::Range { from, to }, PartitionBound::Range { from: of_from, to: of_to })
                if cmp_bounds(from, of_to) == Ordering::Less && cmp_bounds(of_from, to) == Ordering::Less =>
            {
                return Err(overlap(&sibling.name));
            }
            (PartitionBound::Hash { modulus, remainder }, PartitionBound::Hash { modulus: m, remainder: r }) => {
                let (small, large) = if modulus <= m { (*modulus, *m) } else { (*m, *modulus) };
                if large % small != 0 {
                    let detail = if modulus < m {
                        format!("The new modulus {modulus} is not a factor of {m}, the modulus of existing partition \"{}\".", sibling.name)
                    } else {
                        format!(
                            "The new modulus {modulus} is not divisible by {m}, the modulus of existing partition \"{}\".",
                            sibling.name
                        )
                    };
                    return Err(
                        bad_definition("every hash partition modulus must be a factor of the next larger modulus").with_detail(detail)
                    );
                }
                if remainder % small == r % small {
                    return Err(overlap(&sibling.name));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether a non-default bound accepts key values.
fn accepts(bound: &PartitionBound, values: &[Value]) -> bool {
    match bound {
        PartitionBound::Default => false,
        PartitionBound::List(items) => values.first().is_some_and(|v| items.iter().any(|i| same_value(i, v))),
        // A NULL key goes to the default partition only.
        PartitionBound::Range { from, to } => {
            !values.iter().any(Value::is_null)
                && cmp_bound_key(from, values) != Ordering::Greater
                && cmp_bound_key(to, values) == Ordering::Greater
        }
        PartitionBound::Hash { modulus, remainder } => row_hash(values) % u64::from(*modulus) == u64::from(*remainder),
    }
}

// ---------------------------------------------------------------------------------------------
// Hash partitioning (`compute_partition_hash_value` with the types' extended hash functions)

const HASH_PARTITION_SEED: u64 = 0x7A5B_2236_7996_DCFD;

fn hashable(t: &Type) -> bool {
    matches!(
        t.base(),
        Type::Int2
            | Type::Int4
            | Type::Int8
            | Type::Text
            | Type::Varchar
            | Type::Bpchar
            | Type::Name
            | Type::Bool
            | Type::Date
            | Type::Timestamp
            | Type::Timestamptz
            | Type::Uuid
            | Type::Bytea
            | Type::Oid
            | Type::Char
    )
}

#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn hash_value(v: &Value, _ty: &Type) -> Option<u64> {
    use crate::funcs::misc::{hash_bytes_extended, hash_uint32_extended};
    let seed = HASH_PARTITION_SEED;
    let int8 = |n: i64| {
        let lo = n as u32;
        let hi = (n >> 32) as u32;
        hash_uint32_extended(lo ^ if n >= 0 { hi } else { !hi }, seed)
    };
    Some(match v {
        Value::Int2(n) => hash_uint32_extended(i32::from(*n) as u32, seed),
        Value::Int4(n) => hash_uint32_extended(*n as u32, seed),
        Value::Int8(n) => int8(*n),
        Value::Oid(n) => hash_uint32_extended(*n, seed),
        Value::Bool(b) => hash_uint32_extended(u32::from(*b), seed),
        Value::Date(d) => hash_uint32_extended(*d as u32, seed),
        Value::Timestamp(t) | Value::Timestamptz(t) => int8(*t),
        Value::Text(s) => hash_bytes_extended(s.trim_end_matches(' ').as_bytes(), seed),
        Value::Uuid(u) => hash_bytes_extended(u, seed),
        Value::Bytea(b) => hash_bytes_extended(b, seed),
        _ => return None,
    })
}

fn row_hash(values: &[Value]) -> u64 {
    let mut h: u64 = 0;
    for v in values.iter().filter(|v| !v.is_null()) {
        let x = hash_value(v, &v.natural_type()).unwrap_or(0);
        // hash_combine64
        h ^= x.wrapping_add(0x49a0_f4dd_15e5_a8e3).wrapping_add(h << 54).wrapping_add(h >> 7);
    }
    h
}

// ---------------------------------------------------------------------------------------------
// Rows

/// A row of `from` in `to`'s physical layout (columns matched by name).
pub(crate) fn remap(row: &Row, from: &TableDef, to: &TableDef) -> Row {
    if from.columns.len() == to.columns.len()
        && from.columns.iter().zip(&to.columns).all(|(a, b)| a.name == b.name && a.dropped == b.dropped)
    {
        return row.clone();
    }
    to.columns
        .iter()
        .map(|c| {
            if c.dropped {
                return Value::Null;
            }
            from.live_columns().find(|(_, f)| f.name == c.name).and_then(|(i, _)| row.get(i).cloned()).unwrap_or(Value::Null)
        })
        .collect()
}

fn routing_error(catalog: &Catalog, table: &TableDef, key: &PartitionKey, values: &[Value], b: &Binder) -> SqlError {
    let names: Vec<String> = key.parts.iter().map(|p| key_part_text(catalog, table, p, false)).collect();
    let types = key_types(b, table, key).unwrap_or_default();
    let vals: Vec<String> = values
        .iter()
        .zip(types.iter().chain(std::iter::repeat(&Type::Text)))
        .map(|(v, t)| {
            if v.is_null() {
                "null".to_owned()
            } else {
                pgmem_types::encode(v, t, Format::Text).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
            }
        })
        .collect();
    SqlError::new(code::CHECK_VIOLATION, format!("no partition of relation \"{}\" found for row", table.name))
        .with_detail(format!("Partition key of the failing row contains ({}) = ({}).", names.join(", "), vals.join(", ")))
        .with_table(table.name.clone())
}

/// The direct partition of `parent` a row (in `parent`'s layout) belongs to.
fn choose_partition(catalog: &Catalog, b: &Binder, parent: &TableDef, row: &Row, ectx: &EvalCtx) -> SqlResult<Arc<TableDef>> {
    let Some(key) = catalog.partition_key(parent.oid) else {
        return Err(SqlError::internal("routing into a table that is not partitioned"));
    };
    let values = key_values(b, parent, key, row, ectx)?;
    let parts = catalog.partitions(parent.oid);
    if let Some((t, _)) = parts.iter().find(|(_, of)| accepts(&of.bound, &values)) {
        return Ok(Arc::clone(t));
    }
    if let Some((t, _)) = parts.iter().find(|(_, of)| of.bound == PartitionBound::Default) {
        return Ok(Arc::clone(t));
    }
    Err(routing_error(catalog, parent, key, &values, b))
}

/// The leaf partition a row of the partitioned table `root` (its layout) goes to.
pub(crate) fn route(catalog: &Catalog, b: &Binder, root: &TableDef, row: &Row, ectx: &EvalCtx) -> SqlResult<(Arc<TableDef>, Row)> {
    let mut table = Arc::new(root.clone());
    let mut row = row.clone();
    while catalog.is_partitioned(table.oid) {
        let child = choose_partition(catalog, b, &table, &row, ectx)?;
        row = remap(&row, &table, &child);
        table = child;
    }
    Ok((table, row))
}

/// Whether a row of partition `table` satisfies its partition constraint (and its
/// ancestors').
pub(crate) fn satisfies_constraint(catalog: &Catalog, table: &TableDef, row: &Row, ectx: &EvalCtx) -> SqlResult<bool> {
    let b = Binder::new(catalog, &[]);
    let mut table = table.clone();
    let mut row = row.clone();
    while let Some(of) = catalog.partition_of(table.oid) {
        let Some(parent) = catalog.table_by_oid(of.parent) else { return Ok(true) };
        let Some(key) = catalog.partition_key(parent.oid) else { return Ok(true) };
        let prow = remap(&row, &table, parent);
        let values = key_values(&b, parent, key, &prow, ectx)?;
        let ok = match &of.bound {
            PartitionBound::Default => {
                !catalog.partitions(parent.oid).iter().any(|(t, o)| t.oid != table.oid && accepts(&o.bound, &values))
            }
            bound => accepts(bound, &values),
        };
        if !ok {
            return Ok(false);
        }
        table = (**parent).clone();
        row = prow;
    }
    Ok(true)
}

pub(crate) fn constraint_error(table: &TableDef, row: &Row) -> SqlError {
    SqlError::new(code::CHECK_VIOLATION, format!("new row for relation \"{}\" violates partition constraint", table.name))
        .with_detail(crate::dml::failing_row(table, row))
        .with_table(table.name.clone())
}

/// The rows of a partitioned table, read from its leaves in its layout: (leaf, row id, row).
pub(crate) fn scan(txn: &Txn, table: pgmem_catalog::Oid) -> Option<Vec<(pgmem_catalog::Oid, RowId, Row)>> {
    let catalog = txn.catalog();
    if !catalog.is_partitioned(table) {
        return None;
    }
    let root = catalog.table_by_oid(table)?;
    let mut out = Vec::new();
    for leaf in catalog.partition_leaves(table) {
        txn.note_read(leaf.oid);
        for (id, mut row) in txn.scan(leaf.oid) {
            crate::dml::pad(&mut row, &leaf);
            out.push((leaf.oid, id, remap(&row, &leaf, root)));
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// DDL

/// Rows of a (possibly partitioned) table, in its own layout.
fn table_rows(txn: &Txn, table: &TableDef) -> Vec<Row> {
    match scan(txn, table.oid) {
        Some(rows) => rows.into_iter().map(|(_, _, r)| r).collect(),
        None => txn
            .scan(table.oid)
            .into_iter()
            .map(|(_, mut r)| {
                crate::dml::pad(&mut r, table);
                r
            })
            .collect(),
    }
}

/// A new non-default partition may not take rows the default partition holds.
fn check_default_rows(txn: &Txn, parent: &TableDef, bound: &PartitionBound, ectx: &EvalCtx) -> SqlResult<()> {
    if *bound == PartitionBound::Default {
        return Ok(());
    }
    let catalog = txn.catalog();
    let Some((default, _)) = catalog.partitions(parent.oid).into_iter().find(|(_, o)| o.bound == PartitionBound::Default) else {
        return Ok(());
    };
    let Some(key) = catalog.partition_key(parent.oid) else { return Ok(()) };
    let b = Binder::new(catalog, &[]);
    for row in table_rows(txn, &default) {
        let values = key_values(&b, parent, key, &remap(&row, &default, parent), ectx)?;
        if accepts(bound, &values) {
            return Err(SqlError::new(
                code::CHECK_VIOLATION,
                format!("updated partition constraint for default partition \"{}\" would be violated by some row", default.name),
            ));
        }
    }
    Ok(())
}

fn partitioned_parent(catalog: &Catalog, name: &ir::ObjectName) -> SqlResult<(Arc<TableDef>, PartitionKey)> {
    let parent = match catalog.lookup_relation(name)? {
        Some(Relation::Table(t)) => t,
        Some(r) => return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", r.name()))),
        None => return Err(SqlError::undefined_table(&name.to_string())),
    };
    let key =
        catalog.partition_key(parent.oid).cloned().ok_or_else(|| bad_definition(format!("\"{}\" is not partitioned", parent.name)))?;
    Ok((parent, key))
}

/// Index name for a partition's copy of a parent index (`<partition>_pkey`, `_key`, `_idx`).
fn partition_index_name(catalog: &Catalog, table: &TableDef, idx: &IndexDef, taken: &[String]) -> String {
    let cols: Vec<&str> =
        if idx.columns.is_empty() { vec!["expr"] } else { idx.columns.iter().map(|c| table.columns[*c].name.as_str()).collect() };
    let base = if idx.primary {
        format!("{}_pkey", table.name)
    } else if idx.unique {
        pgmem_catalog::default_constraint_name(&table.name, &cols, "key")
    } else {
        pgmem_catalog::default_constraint_name(&table.name, &cols, "idx")
    };
    catalog.choose_name(&table.schema, &pgmem_catalog::truncate_identifier(base), taken)
}

/// Copies the parent's indexes (and their key constraints) to a partition whose slots map
/// `slot_of(parent slot)`, except those an existing index of the partition can stand for.
/// Returns (partition index name, parent index OID) for each index to attach.
fn clone_indexes(
    catalog: &Catalog,
    parent: &TableDef,
    part: &mut TableDef,
    slot_of: &dyn Fn(usize) -> Option<usize>,
) -> Vec<(String, pgmem_catalog::Oid)> {
    let mut taken: Vec<String> = part.indexes.iter().map(|i| i.name.clone()).collect();
    taken.push(part.name.clone());
    let mut links = Vec::new();
    let plan: Vec<(IndexDef, Option<pgmem_catalog::Oid>)> =
        crate::partition_index::plan_partition_indexes(catalog, parent, part).into_iter().map(|(i, f)| (i.clone(), f)).collect();
    for (idx, existing) in plan {
        if let Some(found) = existing.and_then(|o: pgmem_catalog::Oid| part.indexes.iter().find(|i| i.oid == o)) {
            links.push((found.name.clone(), idx.oid));
            continue;
        }
        let Some(columns) = idx.columns.iter().map(|c| slot_of(*c)).collect::<Option<Vec<usize>>>() else { continue };
        let Some(include) = idx.include.iter().map(|c| slot_of(*c)).collect::<Option<Vec<usize>>>() else { continue };
        let name = partition_index_name(catalog, part, &IndexDef { columns: columns.clone(), ..idx.clone() }, &taken);
        taken.push(name.clone());
        links.push((name.clone(), idx.oid));
        let constraint = parent.constraints.iter().find(|c| c.name == idx.name).cloned();
        if let Some(mut c) = constraint {
            c.name.clone_from(&name);
            c.oid = 0;
            c.kind = match c.kind {
                ConstraintKind::PrimaryKey { .. } => ConstraintKind::PrimaryKey { columns: columns.clone() },
                ConstraintKind::Unique { nulls_not_distinct, .. } => {
                    ConstraintKind::Unique { columns: columns.clone(), nulls_not_distinct }
                }
                other => other,
            };
            part.constraints.push(c);
        }
        part.indexes.push(IndexDef { oid: 0, name, columns, include, ..idx.clone() });
    }
    links
}

/// `CREATE TABLE name PARTITION OF parent [(constraints)] FOR VALUES ... [PARTITION BY ...]`.
pub(crate) fn create_partition(
    ct: &ir::CreateTable,
    txn: &mut Txn,
    binder: &Binder,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let Some((parent_name, spec)) = &ct.options.partition_of else { return Err(SqlError::internal("not a partition")) };
    let catalog = Arc::new(txn.catalog().clone());
    let (parent, key) = partitioned_parent(&catalog, parent_name)?;
    let schema = catalog.creation_schema(&ct.name)?;
    let name = ct.name.base().to_owned();
    if catalog.relation(&schema, &name).is_some() {
        if ct.if_not_exists {
            return Ok(crate::ddl::notice("CREATE TABLE", format!("relation \"{name}\" already exists, skipping")));
        }
        return Err(SqlError::new(code::DUPLICATE_TABLE, format!("relation \"{name}\" already exists")));
    }
    let b = Binder::new(&catalog, &[]);
    let bound = {
        let ectx = EvalCtx::new(txn, &[], env, state);
        let bound = bind_bound(&b, &parent, &key, spec, &name, &ectx)?;
        check_overlap(&catalog, &parent, &bound, &name)?;
        check_default_rows(txn, &parent, &bound, &ectx)?;
        bound
    };
    // The partition has the parent's live columns (not its dropped slots, so attnums may
    // differ from the parent's), CHECK and foreign key constraints, and its own copies of the
    // parent's indexes. `slots[parent slot]` is the partition's slot for that column.
    let mut slots = vec![None; parent.columns.len()];
    for (i, (slot, _)) in parent.live_columns().enumerate() {
        slots[slot] = Some(i);
    }
    let slot_of = |s: usize| slots.get(s).copied().flatten();
    let map = |cols: &[usize]| cols.iter().filter_map(|c| slot_of(*c)).collect::<Vec<usize>>();
    let mut table = TableDef {
        oid: 0,
        schema,
        name: name.clone(),
        columns: parent.live_columns().map(|(_, c)| c.clone()).collect(),
        constraints: parent
            .constraints
            .iter()
            .filter(|c| matches!(c.kind, ConstraintKind::Check { .. } | ConstraintKind::ForeignKey { .. }))
            .cloned()
            .map(|mut c| {
                c.oid = 0;
                if let ConstraintKind::ForeignKey { columns, delete_set_columns, .. } = &mut c.kind {
                    *columns = map(columns);
                    *delete_set_columns = map(delete_set_columns);
                }
                c
            })
            .collect(),
        indexes: Vec::new(),
        matview: None,
        unlogged: ct.options.unlogged,
    };
    for c in &ct.columns {
        apply_column_options(&mut table, c)?;
    }
    let index_links = clone_indexes(&catalog, &parent, &mut table, &slot_of);
    let sub_key = ct.options.partition_by.as_ref().map(|p| bind_key(&b, &table, p)).transpose()?;
    let cat = txn.catalog_mut();
    crate::ddl::assign_oids(cat, &mut table);
    let oid = table.oid;
    cat.create_table(table)?;
    cat.set_partition_of(oid, Some(PartitionOf { parent: parent.oid, bound }));
    cat.set_partition_key(oid, sub_key);
    crate::partition_index::link_by_name(cat, oid, &index_links);
    txn.created_table(oid);
    let qualified = txn.catalog().table_by_oid(oid).map(|t| t.qualified()).ok_or_else(|| SqlError::internal("partition vanished"))?;
    for con in &ct.constraints {
        let action = ir::AlterTableAction::AddConstraint(con.clone());
        crate::ddl::alter_table_action(txn, &qualified, &action, env, state)?;
    }
    let _ = binder;
    Ok(QueryResult { tag: "CREATE TABLE".into(), ..Default::default() })
}

/// Column options in `PARTITION OF parent (col WITH OPTIONS ...)`: defaults and NOT NULL.
fn apply_column_options(table: &mut TableDef, c: &ir::ColumnDef) -> SqlResult<()> {
    let slot = table.column_index(&c.name).ok_or_else(|| SqlError::undefined_column(&format!("\"{}\"", c.name)))?;
    let col: &mut ColumnDef = &mut table.columns[slot];
    for con in &c.constraints {
        match &con.kind {
            ir::ColumnConstraintKind::NotNull => col.not_null = true,
            ir::ColumnConstraintKind::Default(e) => col.default = Some(e.clone()),
            _ => {}
        }
    }
    Ok(())
}

/// `ALTER TABLE parent ATTACH PARTITION name FOR VALUES ...`.
pub(crate) fn attach(
    parent_name: &pgmem_catalog::QualName,
    partition: &ir::ObjectName,
    spec: &ir::PartitionBoundSpec,
    txn: &mut Txn,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<()> {
    let catalog = Arc::new(txn.catalog().clone());
    let parent = catalog.table_q(parent_name).cloned().ok_or_else(|| SqlError::undefined_table(&parent_name.name))?;
    let key = catalog
        .partition_key(parent.oid)
        .cloned()
        .ok_or_else(|| SqlError::new(code::WRONG_OBJECT_TYPE, format!("table \"{}\" is not partitioned", parent.name)))?;
    let part = match catalog.lookup_relation(partition)? {
        Some(Relation::Table(t)) if t.matview.is_none() => t,
        Some(r) => return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", r.name()))),
        None => return Err(SqlError::undefined_table(&partition.to_string())),
    };
    if catalog.partition_of(part.oid).is_some() {
        return Err(bad_definition(format!("\"{}\" is already a partition", part.name)));
    }
    if part.oid == parent.oid || catalog.partition_ancestors(parent.oid).contains(&part.oid) {
        return Err(SqlError::new(code::DUPLICATE_TABLE, "circular inheritance not allowed")
            .with_detail(format!("\"{}\" is already a child of \"{}\".", parent.name, part.name)));
    }
    check_attach_columns(&parent, &part)?;
    let b = Binder::new(&catalog, &[]);
    let bound = {
        let ectx = EvalCtx::new(txn, &[], env, state);
        let bound = bind_bound(&b, &parent, &key, spec, &part.name, &ectx)?;
        check_overlap(&catalog, &parent, &bound, &part.name)?;
        check_default_rows(txn, &parent, &bound, &ectx)?;
        bound
    };
    // Existing rows must satisfy the new partition constraint.
    let mut probe = catalog.as_ref().clone();
    probe.set_partition_of(part.oid, Some(PartitionOf { parent: parent.oid, bound: bound.clone() }));
    {
        let ectx = EvalCtx::new(txn, &[], env, state);
        for row in table_rows(txn, &part) {
            if !satisfies_constraint(&probe, &part, &row, &ectx)? {
                return Err(SqlError::new(
                    code::CHECK_VIOLATION,
                    format!("partition constraint of relation \"{}\" is violated by some row", part.name),
                )
                .with_table(part.name.clone()));
            }
        }
    }
    let mut def = (*part).clone();
    let slot_of = |s: usize| def_slot(&parent, &part, s);
    let index_links = clone_indexes(&catalog, &parent, &mut def, &slot_of);
    let cat = txn.catalog_mut();
    crate::ddl::assign_oids(cat, &mut def);
    let q = def.qualified();
    cat.replace_table(&q, def);
    cat.set_partition_of(part.oid, Some(PartitionOf { parent: parent.oid, bound }));
    crate::partition_index::link_by_name(cat, part.oid, &index_links);
    Ok(())
}

fn def_slot(parent: &TableDef, part: &TableDef, slot: usize) -> Option<usize> {
    let name = &parent.columns.get(slot)?.name;
    part.live_columns().find(|(_, c)| c.name == *name).map(|(i, _)| i)
}

/// ATTACH requires the same columns (names and types) as the parent, NOT NULL where the
/// parent has it.
fn check_attach_columns(parent: &TableDef, part: &TableDef) -> SqlResult<()> {
    let mismatch = |msg: String| SqlError::new(code::DATATYPE_MISMATCH, msg);
    for (_, c) in part.live_columns() {
        if parent.live_columns().all(|(_, p)| p.name != c.name) {
            return Err(mismatch(format!(
                "table \"{}\" contains column \"{}\" not found in parent \"{}\"",
                part.name, c.name, parent.name
            ))
            .with_detail("The new partition may contain only the columns present in parent."));
        }
    }
    for (_, p) in parent.live_columns() {
        let Some((_, c)) = part.live_columns().find(|(_, c)| c.name == p.name) else {
            return Err(mismatch(format!("child table is missing column \"{}\"", p.name)));
        };
        if c.ty != p.ty || c.typmod != p.typmod {
            return Err(mismatch(format!("child table \"{}\" has different type for column \"{}\"", part.name, p.name)));
        }
        if p.not_null && !c.not_null {
            return Err(mismatch(format!("column \"{}\" in child table \"{}\" must be marked NOT NULL", p.name, part.name)));
        }
    }
    Ok(())
}

/// `ALTER TABLE parent DETACH PARTITION name`.
pub(crate) fn detach(parent_name: &pgmem_catalog::QualName, partition: &ir::ObjectName, txn: &mut Txn) -> SqlResult<()> {
    let catalog = txn.catalog();
    let parent = catalog.table_q(parent_name).cloned().ok_or_else(|| SqlError::undefined_table(&parent_name.name))?;
    let part = match catalog.lookup_relation(partition)? {
        Some(Relation::Table(t)) => t,
        _ => return Err(SqlError::undefined_table(&partition.to_string())),
    };
    if catalog.partition_of(part.oid).is_none_or(|of| of.parent != parent.oid) {
        return Err(SqlError::new(
            code::UNDEFINED_TABLE,
            format!("relation \"{}\" is not a partition of relation \"{}\"", part.name, parent.name),
        ));
    }
    txn.catalog_mut().set_partition_of(part.oid, None);
    crate::partition_index::detach_indexes(txn.catalog_mut(), &part);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// DML

/// The partitioned table a DML statement targets, if it targets one.
pub(crate) fn partitioned_target(catalog: &Catalog, name: &ir::ObjectName) -> Option<Arc<TableDef>> {
    match catalog.find_relation(name).ok()?? {
        Relation::Table(t) if catalog.is_partitioned(t.oid) => Some(t),
        _ => None,
    }
}

fn leaf_name(t: &TableDef) -> ir::ObjectName {
    ir::ObjectName(vec![t.schema.clone(), t.name.clone()])
}

/// Statement-level parameter type of a column (domains as their base type).
fn param_type(ty: &Type) -> Type {
    match ty {
        Type::Domain(oid) => pgmem_types::usertype::domain_base(*oid).map_or_else(|| ty.clone(), |(b, _)| b),
        other => other.clone(),
    }
}

/// INSERT of concrete rows (in `leaf`'s layout) into a partition, with the original
/// statement's ON CONFLICT and RETURNING.
#[allow(clippy::too_many_arguments)]
fn insert_into(
    leaf: &TableDef,
    rows: Vec<Row>,
    alias: &str,
    on_conflict: Option<ir::OnConflict>,
    returning: Vec<ir::SelectItem>,
    txn: &mut Txn,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let cols: Vec<(usize, &ColumnDef)> = leaf.live_columns().filter(|(_, c)| c.generated.is_none()).collect();
    let mut types = Vec::new();
    let mut params = Vec::new();
    let mut values = Vec::new();
    for row in rows {
        let mut exprs = Vec::new();
        for (slot, c) in &cols {
            types.push(param_type(&c.ty));
            params.push(row.get(*slot).cloned().unwrap_or(Value::Null));
            exprs.push(ir::Expr::Param(params.len()));
        }
        values.push(exprs);
    }
    let ins = ir::Insert {
        with: None,
        table: leaf_name(leaf),
        alias: Some(alias.to_owned()),
        columns: cols.iter().map(|(_, c)| c.name.clone()).collect(),
        source: if cols.is_empty() { ir::InsertSource::DefaultValues } else { ir::InsertSource::Values(values) },
        overriding_system_value: true,
        overriding_user_value: false,
        on_conflict,
        returning,
    };
    let catalog = Arc::new(txn.catalog().clone());
    let b = Binder::new(&catalog, &types);
    top_ctx(|ctx| crate::statement::exec_statement(&Statement::Insert(Box::new(ins)), &b, ctx, txn, &params, env, state))
}

fn combine(results: Vec<QueryResult>, verb: &str, columns: Vec<crate::plan::OutCol>) -> QueryResult {
    let mut count = 0u64;
    let mut rows = Vec::new();
    for r in results {
        count += r.tag.rsplit(' ').next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        rows.extend(r.rows);
    }
    let tag = if verb == "INSERT" { format!("INSERT 0 {count}") } else { format!("{verb} {count}") };
    QueryResult { tag, returns_rows: !columns.is_empty(), columns, rows, ..Default::default() }
}

/// INSERT into a partitioned table: the rows are built with the parent's defaults, then
/// inserted into the partitions they route to.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_insert(
    i: &ir::Insert,
    root: &TableDef,
    binder: &Binder,
    ctx: &Ctx,
    txn: &mut Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let mut stripped = i.clone();
    stripped.returning.clear();
    stripped.on_conflict = None;
    let plan = crate::bind::bind_insert(binder, &stripped, ctx)?;
    let columns = if i.returning.is_empty() { Vec::new() } else { crate::bind::bind_insert(binder, i, ctx)?.returning_cols };
    let rows = crate::dml::insert_preview(&plan, txn, params, env, state)?;
    let catalog = Arc::new(txn.catalog().clone());
    let b = Binder::new(&catalog, &[]);
    let mut groups: Vec<(Arc<TableDef>, Vec<Row>)> = Vec::new();
    {
        let ectx = EvalCtx::new(txn, params, env, state);
        for row in rows {
            let (leaf, leaf_row) = route(&catalog, &b, root, &row, &ectx)?;
            match groups.last_mut() {
                Some((t, rs)) if t.oid == leaf.oid => rs.push(leaf_row),
                _ => groups.push((leaf, vec![leaf_row])),
            }
        }
    }
    let alias = i.alias.clone().unwrap_or_else(|| root.name.clone());
    let mut results = Vec::new();
    for (leaf, rows) in groups {
        results.push(insert_into(&leaf, rows, &alias, i.on_conflict.clone(), i.returning.clone(), txn, env, state)?);
    }
    Ok(combine(results, "INSERT", columns))
}

/// UPDATE of a partitioned table: run on each leaf partition; rows whose new partition key
/// belongs elsewhere move there (deleted, then inserted through the root).
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_update(
    u: &ir::Update,
    root: &TableDef,
    binder: &Binder,
    ctx: &Ctx,
    txn: &mut Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let columns = if u.returning.is_empty() { Vec::new() } else { crate::bind::bind_update(binder, u, ctx)?.returning_cols };
    let alias = u.alias.clone().unwrap_or_else(|| root.name.clone());
    let owner = state.moved_rows.borrow().is_none();
    if owner {
        *state.moved_rows.borrow_mut() = Some(Vec::new());
    }
    let leaves = txn.catalog().partition_leaves(root.oid);
    let mut results = Vec::new();
    for leaf in &leaves {
        let mut lu = u.clone();
        lu.table = leaf_name(leaf);
        lu.alias = Some(alias.clone());
        let r = crate::statement::exec_statement(&Statement::Update(Box::new(lu)), binder, ctx, txn, params, env, state);
        match r {
            Ok(r) => results.push(r),
            Err(e) => {
                if owner {
                    *state.moved_rows.borrow_mut() = None;
                }
                return Err(e);
            }
        }
    }
    if owner {
        let moved = state.moved_rows.borrow_mut().take().unwrap_or_default();
        for (leaf_oid, id, row) in moved {
            let Some(leaf) = txn.catalog().table_by_oid(leaf_oid).cloned() else { continue };
            txn.delete(leaf_oid, id);
            let root_row = remap(&row, &leaf, root);
            let catalog = Arc::new(txn.catalog().clone());
            let b = Binder::new(&catalog, &[]);
            let (target, target_row) = {
                let ectx = EvalCtx::new(txn, params, env, state);
                route(&catalog, &b, root, &root_row, &ectx)?
            };
            let mut r = insert_into(&target, vec![target_row], &alias, None, u.returning.clone(), txn, env, state)?;
            r.tag = "UPDATE 1".into();
            results.push(r);
        }
    }
    Ok(combine(results, "UPDATE", columns))
}

/// DELETE from a partitioned table: run on each leaf partition.
#[allow(clippy::too_many_arguments)]
pub(crate) fn exec_delete(
    d: &ir::Delete,
    root: &TableDef,
    binder: &Binder,
    ctx: &Ctx,
    txn: &mut Txn,
    params: &[Value],
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let columns = if d.returning.is_empty() { Vec::new() } else { crate::bind::bind_delete(binder, d, ctx)?.returning_cols };
    let alias = d.alias.clone().unwrap_or_else(|| root.name.clone());
    let mut results = Vec::new();
    for leaf in txn.catalog().partition_leaves(root.oid) {
        let mut ld = d.clone();
        ld.table = leaf_name(&leaf);
        ld.alias = Some(alias.clone());
        results.push(crate::statement::exec_statement(&Statement::Delete(Box::new(ld)), binder, ctx, txn, params, env, state)?);
    }
    Ok(combine(results, "DELETE", columns))
}

/// A unique constraint or index on a partitioned table must contain every partition key
/// column (and the key may not have expressions).
pub(crate) fn check_unique_keys(table: &TableDef, key: &PartitionKey) -> SqlResult<()> {
    for idx in table.indexes.iter().filter(|i| i.unique) {
        let what = if idx.primary { "PRIMARY KEY" } else { "UNIQUE" };
        for part in &key.parts {
            match part {
                PartitionKeyPart::Expr(_) => {
                    return Err(SqlError::feature_not_supported("unsupported UNIQUE constraint with partition key definition")
                        .with_detail(format!("{what} constraints cannot be used when partition keys include expressions.")));
                }
                PartitionKeyPart::Column(slot) if !idx.columns.contains(slot) => {
                    return Err(SqlError::feature_not_supported(
                        "unique constraint on partitioned table must include all partitioning columns",
                    )
                    .with_detail(format!(
                        "{what} constraint on table \"{}\" lacks column \"{}\" which is part of the partition key.",
                        table.name, table.columns[*slot].name
                    )));
                }
                PartitionKeyPart::Column(_) => {}
            }
        }
    }
    Ok(())
}

/// ALTER TABLE actions that apply to a partitioned table's partitions too.
pub(crate) fn recurses(action: &ir::AlterTableAction) -> Option<ir::AlterTableAction> {
    use ir::AlterTableAction as A;
    Some(match action {
        A::AddColumn { .. }
        | A::DropColumn { .. }
        | A::RenameColumn { .. }
        | A::SetNotNull { .. }
        | A::DropNotNull { .. }
        | A::SetDefault { .. }
        | A::DropDefault { .. }
        | A::AlterType { .. } => action.clone(),
        // Each partition names its own copy of a key constraint.
        A::AddConstraint(c) => {
            let mut c = c.clone();
            if matches!(c.kind, ir::TableConstraintKind::PrimaryKey(_) | ir::TableConstraintKind::Unique { .. }) {
                c.name = None;
            }
            A::AddConstraint(c)
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Functions

fn reg_class(catalog: &Catalog, oid: pgmem_catalog::Oid) -> Value {
    Value::Reg(pgmem_types::RegKind::Class, oid, crate::reg::reg_name(pgmem_types::RegKind::Class, oid, catalog))
}

fn arg_oid(v: &Value) -> Option<pgmem_catalog::Oid> {
    match v {
        Value::Reg(_, o, _) | Value::Oid(o) => Some(*o),
        _ => None,
    }
}

/// Whether a relation is in a partition tree: a partition or a partitioned table, or an
/// attached or partitioned index.
fn in_tree(catalog: &Catalog, oid: pgmem_catalog::Oid) -> bool {
    match catalog.index_by_oid(oid) {
        Some((t, _)) => catalog.index_parent(oid).is_some() || catalog.is_partitioned(t.oid),
        None => catalog.partition_of(oid).is_some() || catalog.is_partitioned(oid),
    }
}

/// A relation's parent in its partition tree (a table's or an index's).
fn tree_parent(catalog: &Catalog, oid: pgmem_catalog::Oid) -> Option<pgmem_catalog::Oid> {
    catalog.partition_of(oid).map(|p| p.parent).or_else(|| catalog.index_parent(oid))
}

/// A relation's children in its partition tree, in OID order.
fn tree_children(catalog: &Catalog, oid: pgmem_catalog::Oid) -> Vec<pgmem_catalog::Oid> {
    let mut out: Vec<pgmem_catalog::Oid> = catalog.partitions(oid).into_iter().map(|(t, _)| t.oid).collect();
    out.extend(catalog.index_children(oid));
    out
}

/// Whether a tree member is a leaf: not a partitioned table, nor an index on one.
fn tree_leaf(catalog: &Catalog, oid: pgmem_catalog::Oid) -> bool {
    match catalog.index_by_oid(oid) {
        Some((t, _)) => !catalog.is_partitioned(t.oid),
        None => !catalog.is_partitioned(oid),
    }
}

fn tree_ancestors(catalog: &Catalog, oid: pgmem_catalog::Oid) -> Vec<pgmem_catalog::Oid> {
    let mut out = Vec::new();
    let mut cur = oid;
    while let Some(p) = tree_parent(catalog, cur) {
        out.push(p);
        cur = p;
    }
    out
}

/// `pg_partition_ancestors(rel)`: the relation and its ancestors (no rows for a relation that is
/// neither a partition nor partitioned).
pub(crate) fn ancestors_srf(a: &[Value], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let catalog = ctx.txn.catalog();
    let Some(oid) = arg_oid(&a[0]) else { return Ok(Vec::new()) };
    if !in_tree(catalog, oid) {
        return Ok(Vec::new());
    }
    let mut out = vec![vec![reg_class(catalog, oid)]];
    out.extend(tree_ancestors(catalog, oid).into_iter().map(|o| vec![reg_class(catalog, o)]));
    Ok(out)
}

/// `pg_partition_tree(rel)`: (relid, parentrelid, isleaf, level) for the tree under `rel`.
pub(crate) fn tree_srf(a: &[Value], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let catalog = ctx.txn.catalog();
    let Some(oid) = arg_oid(&a[0]) else { return Ok(Vec::new()) };
    if !in_tree(catalog, oid) {
        return Ok(Vec::new());
    }
    fn walk(catalog: &Catalog, oid: pgmem_catalog::Oid, parent: Option<pgmem_catalog::Oid>, level: i32, out: &mut Vec<Row>) {
        out.push(vec![
            reg_class(catalog, oid),
            parent.map_or(Value::Null, |p| reg_class(catalog, p)),
            Value::Bool(tree_leaf(catalog, oid)),
            Value::Int4(level),
        ]);
        for child in tree_children(catalog, oid) {
            walk(catalog, child, Some(oid), level + 1, out);
        }
    }
    let mut out = Vec::new();
    walk(catalog, oid, tree_parent(catalog, oid), 0, &mut out);
    Ok(out)
}

/// `pg_partition_root(rel)`.
pub(crate) fn root_fn(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let catalog = ctx.txn.catalog();
    let Some(oid) = arg_oid(&a[0]) else { return Ok(Value::Null) };
    if !in_tree(catalog, oid) {
        return Ok(Value::Null);
    }
    Ok(reg_class(catalog, tree_ancestors(catalog, oid).last().copied().unwrap_or(oid)))
}

/// The latest rows of a table (a partitioned one's from its leaves, in its layout), as
/// foreign key checks see them.
pub(crate) fn latest_rows(txn: &Txn, table: &TableDef) -> Vec<Row> {
    let catalog = txn.catalog();
    let leaves = if catalog.is_partitioned(table.oid) { catalog.partition_leaves(table.oid) } else { vec![Arc::new(table.clone())] };
    let mut out = Vec::new();
    for leaf in leaves {
        for (_, mut row) in txn.scan_latest(leaf.oid) {
            crate::dml::pad(&mut row, &leaf);
            out.push(remap(&row, &leaf, table));
        }
    }
    out
}
