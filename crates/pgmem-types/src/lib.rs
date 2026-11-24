//! The Postgres type kit pgmem runs on: [`Type`] (with real Postgres OIDs, so `RowDescription`
//! and `ParameterDescription` match what drivers expect), runtime [`Value`]s, [`SqlError`]s
//! carrying SQLSTATEs, and the text and binary wire codecs for every supported type.

pub mod binary;
pub mod calendar;
pub mod datetime;
pub mod error;
#[cfg(test)]
mod go_port;
pub mod json;
pub mod numeric;
pub mod text;
mod value;

pub use datetime::Interval;
pub use error::{ErrorFields, SqlError, SqlResult, code};
pub use json::Jsonb;
pub use numeric::Numeric;
pub use value::{GroupKey, Value, compare_values, float_cmp};

use std::fmt;

/// A Postgres data type. Comparison is structural: `Array(Int4)` is `int4[]` wherever it appears.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    /// The type of an untyped string literal or an unannotated parameter before inference
    /// (Postgres' `unknown`, OID 705). Never stored in a table.
    Unknown,
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Text,
    Bytea,
    Uuid,
    Date,
    Time,
    TimeTz,
    Timestamp,
    Timestamptz,
    Interval,
    Jsonb,
    Array(Box<Type>),
}

impl Type {
    pub fn array_of(elem: Type) -> Type {
        Type::Array(Box::new(elem))
    }

    /// The Postgres OID of this type.
    pub fn oid(&self) -> u32 {
        match self {
            Type::Unknown => 705,
            Type::Bool => 16,
            Type::Bytea => 17,
            Type::Int8 => 20,
            Type::Int2 => 21,
            Type::Int4 => 23,
            Type::Text => 25,
            Type::Float4 => 700,
            Type::Float8 => 701,
            Type::Numeric => 1700,
            Type::Date => 1082,
            Type::Time => 1083,
            Type::Timestamp => 1114,
            Type::Timestamptz => 1184,
            Type::Interval => 1186,
            Type::TimeTz => 1266,
            Type::Uuid => 2950,
            Type::Jsonb => 3802,
            Type::Array(elem) => match elem.as_ref() {
                Type::Bool => 1000,
                Type::Bytea => 1001,
                Type::Int2 => 1005,
                Type::Int4 => 1007,
                Type::Text => 1009,
                Type::Int8 => 1016,
                Type::Float4 => 1021,
                Type::Float8 => 1022,
                Type::Numeric => 1231,
                Type::Date => 1182,
                Type::Time => 1183,
                Type::Timestamp => 1115,
                Type::Timestamptz => 1185,
                Type::TimeTz => 1270,
                Type::Interval => 1187,
                Type::Uuid => 2951,
                Type::Jsonb => 3807,
                _ => 0,
            },
        }
    }

    /// Looks a type up by OID (the inverse of [`Type::oid`]). Used for Parse parameter OIDs.
    pub fn from_oid(oid: u32) -> Option<Type> {
        let scalar = |oid| match oid {
            16 => Some(Type::Bool),
            17 => Some(Type::Bytea),
            20 => Some(Type::Int8),
            21 => Some(Type::Int2),
            23 => Some(Type::Int4),
            25 => Some(Type::Text),
            // varchar and bpchar are carried as text until they get their own types.
            1042 | 1043 => Some(Type::Text),
            700 => Some(Type::Float4),
            701 => Some(Type::Float8),
            1700 => Some(Type::Numeric),
            1082 => Some(Type::Date),
            1083 => Some(Type::Time),
            1114 => Some(Type::Timestamp),
            1266 => Some(Type::TimeTz),
            1184 => Some(Type::Timestamptz),
            1186 => Some(Type::Interval),
            2950 => Some(Type::Uuid),
            3802 => Some(Type::Jsonb),
            705 => Some(Type::Unknown),
            _ => None,
        };
        if let Some(t) = scalar(oid) {
            return Some(t);
        }
        let elem = match oid {
            1000 => Type::Bool,
            1001 => Type::Bytea,
            1005 => Type::Int2,
            1007 => Type::Int4,
            1009 | 1014 | 1015 => Type::Text,
            1016 => Type::Int8,
            1021 => Type::Float4,
            1022 => Type::Float8,
            1231 => Type::Numeric,
            1182 => Type::Date,
            1183 => Type::Time,
            1115 => Type::Timestamp,
            1270 => Type::TimeTz,
            1185 => Type::Timestamptz,
            1187 => Type::Interval,
            2951 => Type::Uuid,
            3807 => Type::Jsonb,
            _ => return None,
        };
        Some(Type::array_of(elem))
    }

    /// `typlen` as reported in `RowDescription`: the fixed width, or -1 for varlena types.
    pub fn size(&self) -> i16 {
        match self {
            Type::Bool => 1,
            Type::Int2 => 2,
            Type::Int4 | Type::Float4 | Type::Date => 4,
            Type::Int8 | Type::Float8 | Type::Timestamptz | Type::Timestamp | Type::Time => 8,
            Type::TimeTz => 12,
            Type::Interval | Type::Uuid => 16,
            Type::Unknown => -2,
            Type::Numeric | Type::Text | Type::Bytea | Type::Jsonb | Type::Array(_) => -1,
        }
    }

    /// The canonical Postgres name (`format_type` style, as used in error messages).
    pub fn name(&self) -> String {
        match self {
            Type::Array(elem) => format!("{}[]", elem.name()),
            other => other.scalar_name().to_owned(),
        }
    }

    fn scalar_name(&self) -> &'static str {
        match self {
            Type::Unknown => "unknown",
            Type::Bool => "boolean",
            Type::Int2 => "smallint",
            Type::Int4 => "integer",
            Type::Int8 => "bigint",
            Type::Float4 => "real",
            Type::Float8 => "double precision",
            Type::Numeric => "numeric",
            Type::Text => "text",
            Type::Bytea => "bytea",
            Type::Uuid => "uuid",
            Type::Date => "date",
            Type::Time => "time without time zone",
            Type::TimeTz => "time with time zone",
            Type::Timestamp => "timestamp without time zone",
            Type::Timestamptz => "timestamp with time zone",
            Type::Interval => "interval",
            Type::Jsonb => "jsonb",
            Type::Array(_) => "array",
        }
    }

    /// The internal (`pg_type.typname`) name: `int4`, `_int4`, `timestamptz`, ...
    pub fn typname(&self) -> String {
        match self {
            Type::Unknown => "unknown".into(),
            Type::Bool => "bool".into(),
            Type::Int2 => "int2".into(),
            Type::Int4 => "int4".into(),
            Type::Int8 => "int8".into(),
            Type::Float4 => "float4".into(),
            Type::Float8 => "float8".into(),
            Type::Numeric => "numeric".into(),
            Type::Text => "text".into(),
            Type::Bytea => "bytea".into(),
            Type::Uuid => "uuid".into(),
            Type::Date => "date".into(),
            Type::Time => "time".into(),
            Type::TimeTz => "timetz".into(),
            Type::Timestamp => "timestamp".into(),
            Type::Timestamptz => "timestamptz".into(),
            Type::Interval => "interval".into(),
            Type::Jsonb => "jsonb".into(),
            Type::Array(elem) => format!("_{}", elem.typname()),
        }
    }

    /// Resolves a SQL type name as written in DDL or a cast (`int`, `bigint`, `varchar`,
    /// `double precision`, `timestamptz`, ...). Array suffixes are handled by the caller.
    pub fn from_sql_name(name: &str) -> Option<Type> {
        let lower = name.trim().to_ascii_lowercase();
        let t = match lower.as_str() {
            "bool" | "boolean" => Type::Bool,
            "int2" | "smallint" | "smallserial" | "serial2" => Type::Int2,
            "int" | "int4" | "integer" | "serial" | "serial4" => Type::Int4,
            "int8" | "bigint" | "bigserial" | "serial8" => Type::Int8,
            "float4" | "real" => Type::Float4,
            "float8" | "double precision" | "float" => Type::Float8,
            "numeric" | "decimal" => Type::Numeric,
            "text" | "varchar" | "character varying" | "char" | "character" | "bpchar" | "name" | "citext" => Type::Text,
            "bytea" => Type::Bytea,
            "uuid" => Type::Uuid,
            "timestamptz" | "timestamp with time zone" => Type::Timestamptz,
            "timestamp" | "timestamp without time zone" => Type::Timestamp,
            "date" => Type::Date,
            "time" | "time without time zone" => Type::Time,
            "timetz" | "time with time zone" => Type::TimeTz,
            "interval" => Type::Interval,
            "jsonb" | "json" => Type::Jsonb,
            "unknown" => Type::Unknown,
            _ => return None,
        };
        Some(t)
    }

    pub fn is_integer(&self) -> bool {
        matches!(self, Type::Int2 | Type::Int4 | Type::Int8)
    }

    pub fn is_float(&self) -> bool {
        matches!(self, Type::Float4 | Type::Float8)
    }

    pub fn is_numeric(&self) -> bool {
        self.is_integer() || self.is_float() || matches!(self, Type::Numeric)
    }

    pub fn element(&self) -> Option<&Type> {
        match self {
            Type::Array(e) => Some(e),
            _ => None,
        }
    }

    /// The `pg_type.typcategory` letter, used by implicit-coercion and operator resolution.
    pub fn category(&self) -> char {
        match self {
            Type::Bool => 'B',
            Type::Int2 | Type::Int4 | Type::Int8 | Type::Float4 | Type::Float8 | Type::Numeric => 'N',
            Type::Text => 'S',
            Type::Timestamptz | Type::Timestamp | Type::Date | Type::Time | Type::TimeTz => 'D',
            Type::Interval => 'T',
            Type::Array(_) => 'A',
            Type::Unknown => 'X',
            Type::Bytea | Type::Uuid | Type::Jsonb => 'U',
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// Wire format code from Bind / RowDescription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Text,
    Binary,
}

impl Format {
    pub fn from_code(code: i16) -> Format {
        if code == 1 { Format::Binary } else { Format::Text }
    }

    pub fn code(self) -> i16 {
        match self {
            Format::Text => 0,
            Format::Binary => 1,
        }
    }
}

/// Encodes a non-null value for the wire in the requested format.
pub fn encode(value: &Value, ty: &Type, format: Format) -> SqlResult<Vec<u8>> {
    match format {
        Format::Text => Ok(text::encode(value, ty)?.into_bytes()),
        Format::Binary => binary::encode(value, ty),
    }
}

/// Decodes a non-null wire value (a Bind parameter) of the given type and format.
pub fn decode(bytes: &[u8], ty: &Type, format: Format) -> SqlResult<Value> {
    match format {
        Format::Text => {
            let s = std::str::from_utf8(bytes)
                .map_err(|_| SqlError::new(code::INVALID_TEXT_REPRESENTATION, "invalid byte sequence for encoding \"UTF8\""))?;
            text::decode(s, ty)
        }
        Format::Binary => binary::decode(bytes, ty),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_round_trip() {
        for t in [
            Type::Bool,
            Type::Int2,
            Type::Int4,
            Type::Int8,
            Type::Float4,
            Type::Float8,
            Type::Numeric,
            Type::Text,
            Type::Bytea,
            Type::Uuid,
            Type::Date,
            Type::Time,
            Type::TimeTz,
            Type::Timestamp,
            Type::Timestamptz,
            Type::Interval,
            Type::Jsonb,
            Type::array_of(Type::Int4),
            Type::array_of(Type::Text),
            Type::array_of(Type::Int8),
        ] {
            assert_eq!(Type::from_oid(t.oid()), Some(t.clone()), "{t}");
        }
    }

    #[test]
    fn sql_names() {
        assert_eq!(Type::from_sql_name("INTEGER"), Some(Type::Int4));
        assert_eq!(Type::from_sql_name("double precision"), Some(Type::Float8));
        assert_eq!(Type::from_sql_name("varchar"), Some(Type::Text));
        assert_eq!(Type::from_sql_name("nope"), None);
    }
}
