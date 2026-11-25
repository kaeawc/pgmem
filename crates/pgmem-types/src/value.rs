use crate::datetime::Interval;
use crate::json::Jsonb;
use crate::numeric::Numeric;
use crate::{SqlError, SqlResult, Type};
use std::cmp::Ordering;

/// A runtime SQL value. `Null` is untyped; the static type of a column or expression lives in
/// the plan, not in the value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int2(i16),
    Int4(i32),
    Int8(i64),
    Float4(f32),
    Float8(f64),
    Numeric(Numeric),
    Text(String),
    Bytea(Vec<u8>),
    Uuid([u8; 16]),
    /// Days since 2000-01-01.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since midnight and the zone offset in seconds east of UTC.
    TimeTz(i64, i32),
    /// Microseconds since 2000-01-01 00:00:00 (no time zone).
    Timestamp(i64),
    /// Microseconds since 2000-01-01 00:00:00 UTC (Postgres' internal representation).
    Timestamptz(i64),
    Interval(Interval),
    Jsonb(Jsonb),
    /// A `json` value: validated text, kept exactly as written.
    Json(String),
    Oid(u32),
    /// A `tid`: block number and item (line pointer) number.
    Tid(u32, u16),
    /// A `reg*` value: the OID and the name it prints as (resolved when the value was made,
    /// so output needs no catalog).
    Reg(crate::RegKind, u32, String),
    /// A one-dimensional array; elements may be `Null`.
    Array(Vec<Value>),
    /// An enum value: the enum type's OID and the label's OID (`pg_enum.oid`), so renaming a
    /// label changes stored values too, as in Postgres.
    Enum(u32, u32),
    /// A composite value: its type's OID (`record` for anonymous rows) and the fields.
    Record(u32, Vec<Value>),
    Range(Box<crate::range::Range>),
    /// A multirange: canonical (sorted, non-overlapping, no empty ranges).
    Multirange(Type, Vec<crate::range::Range>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Integer value widened to i64, if this is an integer.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int2(n) => Some(i64::from(*n)),
            Value::Int4(n) => Some(i64::from(*n)),
            Value::Int8(n) => Some(*n),
            _ => None,
        }
    }

    /// Numeric value widened to f64, if this is any number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float4(f) => Some(f64::from(*f)),
            Value::Float8(f) => Some(*f),
            Value::Numeric(n) => Some(n.to_f64()),
            #[allow(clippy::cast_precision_loss)]
            other => other.as_i64().map(|n| n as f64),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) | Value::Json(s) => Some(s),
            _ => None,
        }
    }

    /// SQL truthiness for WHERE / ON / HAVING / CHECK: only `true` passes; NULL does not.
    pub fn is_true(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    /// A short description of the variant, for internal error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Int2(_) => "smallint",
            Value::Int4(_) => "integer",
            Value::Int8(_) => "bigint",
            Value::Float4(_) => "real",
            Value::Float8(_) => "double precision",
            Value::Numeric(_) => "numeric",
            Value::Text(_) => "text",
            Value::Bytea(_) => "bytea",
            Value::Uuid(_) => "uuid",
            Value::Date(_) => "date",
            Value::Time(_) => "time without time zone",
            Value::TimeTz(..) => "time with time zone",
            Value::Timestamp(_) => "timestamp without time zone",
            Value::Timestamptz(_) => "timestamp with time zone",
            Value::Interval(_) => "interval",
            Value::Jsonb(_) => "jsonb",
            Value::Json(_) => "json",
            Value::Oid(_) => "oid",
            Value::Tid(..) => "tid",
            Value::Reg(k, ..) => k.typname(),
            Value::Array(_) => "array",
            Value::Enum(..) => "enum",
            Value::Record(..) => "record",
            Value::Range(_) => "range",
            Value::Multirange(..) => "multirange",
        }
    }

    /// A hashable, type-tagged key used for DISTINCT, GROUP BY, UNIQUE and set operations.
    /// Values that compare equal under [`compare_values`] produce equal keys within a type
    /// family (integers of different widths share a key; so do floats with integers of the
    /// same value only through explicit coercion in the binder).
    pub fn group_key(&self) -> GroupKey {
        match self {
            Value::Null => GroupKey::Null,
            Value::Bool(b) => GroupKey::Bool(*b),
            Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => GroupKey::Int(self.as_i64().unwrap_or_default()),
            Value::Float4(_) | Value::Float8(_) => {
                let f = self.as_f64().unwrap_or_default();
                // Postgres treats all NaNs as equal and -0 == 0 for grouping purposes.
                let canon = if f.is_nan() {
                    f64::NAN.to_bits()
                } else if f == 0.0 {
                    0
                } else {
                    f.to_bits()
                };
                GroupKey::Float(canon)
            }
            Value::Numeric(n) => GroupKey::Text(format!("numeric:{}", n.normalized())),
            Value::Text(s) => GroupKey::Text(s.clone()),
            Value::Bytea(b) => GroupKey::Bytes(b.clone()),
            Value::Uuid(u) => GroupKey::Uuid(*u),
            Value::Timestamptz(t) | Value::Timestamp(t) | Value::Time(t) => GroupKey::Int(*t),
            Value::Date(d) => GroupKey::Int(i64::from(*d)),
            Value::TimeTz(t, z) => GroupKey::Int(*t - i64::from(*z) * 1_000_000),
            Value::Interval(iv) => GroupKey::Int128(iv.total_micros_i128()),
            Value::Jsonb(j) => GroupKey::Text(j.to_string()),
            Value::Json(s) => GroupKey::Text(s.clone()),
            Value::Oid(o) | Value::Reg(_, o, _) => GroupKey::Int(i64::from(*o)),
            Value::Tid(b, i) => GroupKey::Int((i64::from(*b) << 16) | i64::from(*i)),
            Value::Array(items) | Value::Record(_, items) => GroupKey::Array(items.iter().map(Value::group_key).collect()),
            Value::Enum(_, label) => GroupKey::Int(i64::from(*label)),
            Value::Range(r) => range_key(r),
            Value::Multirange(_, rs) => GroupKey::Array(rs.iter().map(range_key).collect()),
        }
    }
}

fn range_key(r: &crate::range::Range) -> GroupKey {
    let bound = |b: &Option<Value>| b.as_ref().map_or(GroupKey::Null, Value::group_key);
    GroupKey::Array(vec![
        GroupKey::Bool(r.empty),
        bound(&r.lower),
        GroupKey::Bool(r.lower_inc),
        bound(&r.upper),
        GroupKey::Bool(r.upper_inc),
    ])
}

/// See [`Value::group_key`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GroupKey {
    Null,
    Bool(bool),
    Int(i64),
    Int128(i128),
    Float(u64),
    Text(String),
    Bytes(Vec<u8>),
    Uuid([u8; 16]),
    Array(Vec<GroupKey>),
}

/// Total order over two non-null values of compatible types, following Postgres' btree
/// semantics: numbers compare across widths, floats order NaN above everything, text compares
/// bytewise (the "C" collation), arrays compare elementwise with NULL elements sorting last.
pub fn compare_values(a: &Value, b: &Value) -> SqlResult<Ordering> {
    use Value::*;
    let ord = match (a, b) {
        (Null, Null) => Ordering::Equal,
        (Null, _) => Ordering::Greater,
        (_, Null) => Ordering::Less,
        (Numeric(x), Numeric(y)) => x.cmp_num(y),
        (Numeric(x), Int2(_) | Int4(_) | Int8(_)) => x.cmp_num(&crate::Numeric::from_i64(b.as_i64().unwrap_or_default())),
        (Int2(_) | Int4(_) | Int8(_), Numeric(y)) => crate::Numeric::from_i64(a.as_i64().unwrap_or_default()).cmp_num(y),
        (Float4(_) | Float8(_), _) | (_, Float4(_) | Float8(_)) if a.as_f64().is_some() && b.as_f64().is_some() => {
            float_cmp(a.as_f64().unwrap_or_default(), b.as_f64().unwrap_or_default())
        }
        (Int2(_) | Int4(_) | Int8(_), Int2(_) | Int4(_) | Int8(_)) => a.as_i64().cmp(&b.as_i64()),
        (Bool(x), Bool(y)) => x.cmp(y),
        (Text(x), Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Bytea(x), Bytea(y)) => x.cmp(y),
        (Uuid(x), Uuid(y)) => x.cmp(y),
        (Timestamptz(x), Timestamptz(y)) | (Timestamp(x), Timestamp(y)) | (Time(x), Time(y)) => x.cmp(y),
        (Date(x), Date(y)) => x.cmp(y),
        (TimeTz(x, zx), TimeTz(y, zy)) => (*x - i64::from(*zx) * 1_000_000).cmp(&(*y - i64::from(*zy) * 1_000_000)),
        (Interval(x), Interval(y)) => x.cmp(y),
        (Jsonb(x), Jsonb(y)) => x.total_cmp(y),
        (Json(x), Json(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Oid(x) | Reg(_, x, _), Oid(y) | Reg(_, y, _)) => x.cmp(y),
        (Tid(xb, xi), Tid(yb, yi)) => (xb, xi).cmp(&(yb, yi)),
        (Enum(t, x), Enum(u, y)) if t == u => crate::usertype::compare_enum(*t, *x, *y),
        (Range(x), Range(y)) => crate::range::compare(x, y)?,
        (Multirange(_, x), Multirange(_, y)) => crate::range::compare_multi(x, y)?,
        // Composite values compare field by field; two NULL fields are equal and NULL sorts
        // above non-NULL (btree record_cmp semantics).
        (Record(_, x), Record(_, y)) => {
            if x.len() != y.len() {
                return Err(SqlError::new(crate::code::DATATYPE_MISMATCH, "cannot compare record types with different numbers of columns"));
            }
            for (l, r) in x.iter().zip(y.iter()) {
                let o = compare_values(l, r)?;
                if o != Ordering::Equal {
                    return Ok(o);
                }
            }
            Ordering::Equal
        }
        (Array(x), Array(y)) => {
            for (l, r) in x.iter().zip(y.iter()) {
                let o = compare_values(l, r)?;
                if o != Ordering::Equal {
                    return Ok(o);
                }
            }
            x.len().cmp(&y.len())
        }
        _ => {
            return Err(SqlError::new(crate::code::UNDEFINED_FUNCTION, format!("operator does not exist: {} = {}", a.kind(), b.kind())));
        }
    };
    Ok(ord)
}

/// Postgres float ordering: NaN equals NaN and sorts above every other value.
pub fn float_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

impl Value {
    /// Returns the type a value naturally has when no static type is known (used for
    /// literals and set-returning helpers).
    pub fn natural_type(&self) -> Type {
        match self {
            Value::Null => Type::Unknown,
            Value::Bool(_) => Type::Bool,
            Value::Int2(_) => Type::Int2,
            Value::Int4(_) => Type::Int4,
            Value::Int8(_) => Type::Int8,
            Value::Float4(_) => Type::Float4,
            Value::Float8(_) => Type::Float8,
            Value::Numeric(_) => Type::Numeric,
            Value::Text(_) => Type::Text,
            Value::Bytea(_) => Type::Bytea,
            Value::Uuid(_) => Type::Uuid,
            Value::Date(_) => Type::Date,
            Value::Time(_) => Type::Time,
            Value::TimeTz(..) => Type::TimeTz,
            Value::Timestamp(_) => Type::Timestamp,
            Value::Timestamptz(_) => Type::Timestamptz,
            Value::Interval(_) => Type::Interval,
            Value::Jsonb(_) => Type::Jsonb,
            Value::Json(_) => Type::Json,
            Value::Oid(_) => Type::Oid,
            Value::Tid(..) => Type::Tid,
            Value::Reg(k, ..) => Type::Reg(*k),
            Value::Array(items) => {
                let elem = items.iter().find(|v| !v.is_null()).map_or(Type::Text, Value::natural_type);
                Type::array_of(elem)
            }
            Value::Enum(t, _) => Type::Enum(*t),
            Value::Record(crate::usertype::RECORD_OID, _) => Type::Record,
            Value::Record(t, _) if crate::usertype::is_blessed(*t) => Type::Record,
            Value::Record(t, _) => Type::Composite(*t),
            Value::Range(r) => Type::Range(Box::new(r.sub.clone())),
            Value::Multirange(sub, _) => Type::Multirange(Box::new(sub.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_width_integer_compare() {
        assert_eq!(compare_values(&Value::Int4(3), &Value::Int8(3)).ok(), Some(Ordering::Equal));
        assert_eq!(compare_values(&Value::Int2(1), &Value::Int8(3)).ok(), Some(Ordering::Less));
    }

    #[test]
    fn nan_sorts_last() {
        assert_eq!(compare_values(&Value::Float8(f64::NAN), &Value::Float8(1e300)).ok(), Some(Ordering::Greater));
        assert_eq!(compare_values(&Value::Float8(f64::NAN), &Value::Float8(f64::NAN)).ok(), Some(Ordering::Equal));
    }

    #[test]
    fn mismatched_types_error() {
        assert!(compare_values(&Value::Text("a".into()), &Value::Int4(1)).is_err());
    }

    #[test]
    fn group_keys_unify_integer_widths() {
        assert_eq!(Value::Int4(7).group_key(), Value::Int8(7).group_key());
        assert_eq!(Value::Float8(0.0).group_key(), Value::Float8(-0.0).group_key());
    }
}
