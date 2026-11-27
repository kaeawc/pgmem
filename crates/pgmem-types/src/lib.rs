//! The Postgres type kit pgmem runs on: [`Type`] (with real Postgres OIDs, so `RowDescription`
//! and `ParameterDescription` match what drivers expect), runtime [`Value`]s, [`SqlError`]s
//! carrying SQLSTATEs, and the text and binary wire codecs for every supported type.

pub mod binary;
pub mod calendar;
pub mod context;
pub mod datetime;
pub mod dtparse;
pub mod error;
#[cfg(test)]
mod go_port;
pub mod hstore;
pub mod json;
pub mod jsonpath;
pub mod jsontext;
pub mod netbits;
pub mod numeric;
pub mod range;
pub mod text;
pub mod usertype;
mod value;
pub mod zone;

pub use datetime::Interval;
pub use error::{ErrorFields, ErrorRef, SourceRefs, SqlError, SqlResult, code, node_key};
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
    /// `jsonpath`: an SQL/JSON path expression, kept in its canonical text form.
    JsonPath,
    /// `character varying`: text with an optional length limit (the typmod).
    Varchar,
    /// `character` (`bpchar`): blank-padded to its length; trailing spaces are insignificant.
    Bpchar,
    /// `name`: identifiers, at most 63 bytes.
    Name,
    /// The single-byte internal type `"char"` (catalog columns such as `relkind`).
    Char,
    /// `json`: validated text, kept exactly as written.
    Json,
    Oid,
    /// System column types: `tid` (`ctid`, a block and an item number), `xid`
    /// (`xmin` / `xmax`) and `cid` (`cmin` / `cmax`).
    Tid,
    Xid,
    Cid,
    /// The OID alias types (`regclass`, `regtype`, ...): an OID that prints as an object name.
    Reg(RegKind),
    /// The result type of functions called for their side effect; prints as an empty string.
    Void,
    /// `int2vector` / `oidvector`: catalog arrays with space-separated output and zero-based
    /// subscripts (`pg_index.indkey`, `pg_proc.proargtypes`).
    Int2Vector,
    OidVector,
    Array(Box<Type>),
    /// A user-defined enum (`CREATE TYPE ... AS ENUM`), by OID.
    Enum(u32),
    /// A domain (`CREATE DOMAIN`), by OID. Values are values of the base type; operators and
    /// functions see the base type (Postgres' `getBaseType`), casts and assignments to the
    /// domain check its constraints.
    Domain(u32),
    /// A named composite type: `CREATE TYPE ... AS (...)` or a table's row type, by OID.
    Composite(u32),
    /// `record`: an anonymous row (`ROW(1, 'a')`).
    Record,
    /// `bit(n)` (fixed length, the typmod) and `bit varying(n)`.
    Bit,
    VarBit,
    /// `money`: cents in an int8.
    Money,
    /// `inet` / `cidr` (IPv4 or IPv6 address and netmask length), `macaddr`, `macaddr8`.
    Inet,
    Cidr,
    MacAddr,
    MacAddr8,
    /// The `citext` extension's case-insensitive text.
    CiText,
    /// The `hstore` extension's key/value map.
    HStore,
    /// A builtin range type over its subtype (`int4range`, `tstzrange`, ...).
    Range(Box<Type>),
    /// A builtin multirange type over its range's subtype (`int4multirange`, ...).
    Multirange(Box<Type>),
}

/// OIDs pgmem gives the extension types (Postgres assigns them at CREATE EXTENSION); each
/// array type is the next OID.
pub const CITEXT_OID: u32 = 90_100;
pub const HSTORE_OID: u32 = 90_102;

/// The `reg*` OID alias types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RegKind {
    Class,
    Type,
    Proc,
    Namespace,
    Role,
    /// `regprocedure`: a function with its argument types.
    Procedure,
}

impl RegKind {
    pub fn typname(self) -> &'static str {
        match self {
            RegKind::Class => "regclass",
            RegKind::Type => "regtype",
            RegKind::Proc => "regproc",
            RegKind::Namespace => "regnamespace",
            RegKind::Role => "regrole",
            RegKind::Procedure => "regprocedure",
        }
    }
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
            Type::JsonPath => 4072,
            Type::Varchar => 1043,
            Type::Bpchar => 1042,
            Type::Name => 19,
            Type::Char => 18,
            Type::Json => 114,
            Type::Oid => 26,
            Type::Tid => 27,
            Type::Xid => 28,
            Type::Cid => 29,
            Type::Reg(RegKind::Class) => 2205,
            Type::Reg(RegKind::Type) => 2206,
            Type::Reg(RegKind::Proc) => 24,
            Type::Reg(RegKind::Namespace) => 4089,
            Type::Reg(RegKind::Role) => 4096,
            Type::Reg(RegKind::Procedure) => 2202,
            Type::Void => 2278,
            Type::Int2Vector => 22,
            Type::OidVector => 30,
            Type::Enum(o) | Type::Domain(o) | Type::Composite(o) => *o,
            Type::Record => usertype::RECORD_OID,
            Type::Bit => 1560,
            Type::VarBit => 1562,
            Type::Money => 790,
            Type::Inet => 869,
            Type::Cidr => 650,
            Type::MacAddr => 829,
            Type::MacAddr8 => 774,
            Type::CiText => CITEXT_OID,
            Type::HStore => HSTORE_OID,
            Type::Range(s) => range::entry(s).map_or(0, |e| e.1),
            Type::Multirange(s) => range::entry(s).map_or(0, |e| e.2),
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
                Type::JsonPath => 4073,
                Type::Varchar => 1015,
                Type::Bpchar => 1014,
                Type::Name => 1003,
                Type::Char => 1002,
                Type::Json => 199,
                Type::Oid => 1028,
                Type::Tid => 1010,
                Type::Xid => 1011,
                Type::Cid => 1012,
                Type::Reg(RegKind::Class) => 2210,
                Type::Reg(RegKind::Type) => 2211,
                Type::Reg(RegKind::Proc) => 1008,
                Type::Reg(RegKind::Namespace) => 4090,
                Type::Reg(RegKind::Role) => 4097,
                Type::Reg(RegKind::Procedure) => 2207,
                Type::Enum(o) | Type::Domain(o) | Type::Composite(o) => usertype::array_oid(*o),
                Type::Record => usertype::RECORD_ARRAY_OID,
                Type::Bit => 1561,
                Type::VarBit => 1563,
                Type::Money => 791,
                Type::Inet => 1041,
                Type::Cidr => 651,
                Type::MacAddr => 1040,
                Type::MacAddr8 => 775,
                Type::CiText => CITEXT_OID + 1,
                Type::HStore => HSTORE_OID + 1,
                Type::Range(s) => range::entry(s).map_or(0, |e| e.3),
                Type::Multirange(s) => range::entry(s).map_or(0, |e| e.4),
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
            1043 => Some(Type::Varchar),
            1042 => Some(Type::Bpchar),
            19 => Some(Type::Name),
            18 => Some(Type::Char),
            114 => Some(Type::Json),
            26 => Some(Type::Oid),
            27 => Some(Type::Tid),
            28 => Some(Type::Xid),
            29 => Some(Type::Cid),
            2205 => Some(Type::Reg(RegKind::Class)),
            2206 => Some(Type::Reg(RegKind::Type)),
            24 => Some(Type::Reg(RegKind::Proc)),
            4089 => Some(Type::Reg(RegKind::Namespace)),
            4096 => Some(Type::Reg(RegKind::Role)),
            2202 => Some(Type::Reg(RegKind::Procedure)),
            2278 => Some(Type::Void),
            22 => Some(Type::Int2Vector),
            30 => Some(Type::OidVector),
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
            4072 => Some(Type::JsonPath),
            705 => Some(Type::Unknown),
            usertype::RECORD_OID => Some(Type::Record),
            1560 => Some(Type::Bit),
            1562 => Some(Type::VarBit),
            790 => Some(Type::Money),
            869 => Some(Type::Inet),
            650 => Some(Type::Cidr),
            829 => Some(Type::MacAddr),
            774 => Some(Type::MacAddr8),
            CITEXT_OID => Some(Type::CiText),
            HSTORE_OID => Some(Type::HStore),
            _ => None,
        };
        if let Some(t) = scalar(oid) {
            return Some(t);
        }
        for (sub, r, m, ra, ma, _, _) in range::RANGES {
            let t = if oid == *r || oid == *ra {
                Type::Range(Box::new(sub.clone()))
            } else if oid == *m || oid == *ma {
                Type::Multirange(Box::new(sub.clone()))
            } else {
                continue;
            };
            return Some(if oid == *ra || oid == *ma { Type::array_of(t) } else { t });
        }
        let elem = match oid {
            1000 => Type::Bool,
            1001 => Type::Bytea,
            1005 => Type::Int2,
            1007 => Type::Int4,
            1009 => Type::Text,
            1015 => Type::Varchar,
            1014 => Type::Bpchar,
            1003 => Type::Name,
            1002 => Type::Char,
            199 => Type::Json,
            1028 => Type::Oid,
            1010 => Type::Tid,
            1011 => Type::Xid,
            1012 => Type::Cid,
            2210 => Type::Reg(RegKind::Class),
            2211 => Type::Reg(RegKind::Type),
            1008 => Type::Reg(RegKind::Proc),
            4090 => Type::Reg(RegKind::Namespace),
            4097 => Type::Reg(RegKind::Role),
            2207 => Type::Reg(RegKind::Procedure),
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
            4073 => Type::JsonPath,
            usertype::RECORD_ARRAY_OID => Type::Record,
            1561 => Type::Bit,
            1563 => Type::VarBit,
            791 => Type::Money,
            1041 => Type::Inet,
            651 => Type::Cidr,
            1040 => Type::MacAddr,
            775 => Type::MacAddr8,
            o if o == CITEXT_OID + 1 => Type::CiText,
            o if o == HSTORE_OID + 1 => Type::HStore,
            _ => return usertype::type_for_oid(oid),
        };
        Some(Type::array_of(elem))
    }

    /// `typlen` as reported in `RowDescription`: the fixed width, or -1 for varlena types.
    pub fn size(&self) -> i16 {
        match self {
            Type::Bool | Type::Char => 1,
            Type::Int2 => 2,
            Type::Name => 64,
            Type::Int4 | Type::Float4 | Type::Date | Type::Oid | Type::Reg(_) | Type::Xid | Type::Cid => 4,
            Type::Tid => 6,
            Type::Int8 | Type::Float8 | Type::Timestamptz | Type::Timestamp | Type::Time => 8,
            Type::TimeTz => 12,
            Type::Interval | Type::Uuid => 16,
            Type::Money | Type::MacAddr8 => 8,
            Type::MacAddr => 6,
            Type::Bit | Type::VarBit | Type::Inet | Type::Cidr | Type::CiText | Type::HStore => -1,
            Type::Unknown => -2,
            Type::Void => 4,
            Type::Int2Vector | Type::OidVector | Type::Composite(_) | Type::Record => -1,
            Type::Enum(_) => 4,
            Type::Domain(_) => self.base().size(),
            Type::Numeric
            | Type::Text
            | Type::Bytea
            | Type::Jsonb
            | Type::JsonPath
            | Type::Array(_)
            | Type::Range(_)
            | Type::Multirange(_)
            | Type::Varchar
            | Type::Bpchar
            | Type::Json => -1,
        }
    }

    /// The canonical Postgres name (`format_type` style, as used in error messages).
    pub fn name(&self) -> String {
        match self {
            Type::Array(elem) => format!("{}[]", elem.name()),
            Type::Enum(o) | Type::Domain(o) | Type::Composite(o) => usertype::type_name(*o),
            Type::Range(_) | Type::Multirange(_) => self.typname(),
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
            Type::JsonPath => "jsonpath",
            Type::Varchar => "character varying",
            Type::Bpchar => "character",
            Type::Name => "name",
            Type::Char => "\"char\"",
            Type::Json => "json",
            Type::Oid => "oid",
            Type::Tid => "tid",
            Type::Xid => "xid",
            Type::Cid => "cid",
            Type::Reg(k) => k.typname(),
            Type::Void => "void",
            Type::Int2Vector => "int2vector",
            Type::OidVector => "oidvector",
            Type::Array(_) => "array",
            Type::Record => "record",
            Type::Bit => "bit",
            Type::VarBit => "bit varying",
            Type::Money => "money",
            Type::Inet => "inet",
            Type::Cidr => "cidr",
            Type::MacAddr => "macaddr",
            Type::MacAddr8 => "macaddr8",
            Type::CiText => "citext",
            Type::HStore => "hstore",
            Type::Enum(_) | Type::Domain(_) | Type::Composite(_) | Type::Range(_) | Type::Multirange(_) => "???",
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
            Type::JsonPath => "jsonpath".into(),
            Type::Varchar => "varchar".into(),
            Type::Bpchar => "bpchar".into(),
            Type::Name => "name".into(),
            Type::Char => "char".into(),
            Type::Json => "json".into(),
            Type::Oid => "oid".into(),
            Type::Tid => "tid".into(),
            Type::Xid => "xid".into(),
            Type::Cid => "cid".into(),
            Type::Reg(k) => k.typname().into(),
            Type::Void => "void".into(),
            Type::Int2Vector => "int2vector".into(),
            Type::OidVector => "oidvector".into(),
            Type::Array(elem) => format!("_{}", elem.typname()),
            Type::Record => "record".into(),
            Type::Bit => "bit".into(),
            Type::VarBit => "varbit".into(),
            Type::Money => "money".into(),
            Type::Inet => "inet".into(),
            Type::Cidr => "cidr".into(),
            Type::MacAddr => "macaddr".into(),
            Type::MacAddr8 => "macaddr8".into(),
            Type::CiText => "citext".into(),
            Type::HStore => "hstore".into(),
            Type::Enum(o) | Type::Domain(o) | Type::Composite(o) => usertype::raw_type_name(*o),
            Type::Range(s) => range::entry(s).map_or("anyrange", |e| e.5).into(),
            Type::Multirange(s) => range::entry(s).map_or("anymultirange", |e| e.6).into(),
        }
    }

    /// The type operators and functions see: a domain's base type (Postgres' `getBaseType`),
    /// every other type itself.
    pub fn base(&self) -> Type {
        match self {
            Type::Domain(o) => usertype::domain_base(*o).map_or(Type::Unknown, |(b, _)| b),
            other => other.clone(),
        }
    }

    pub fn is_domain(&self) -> bool {
        matches!(self, Type::Domain(_))
    }

    /// True for named composite types and `record`.
    pub fn is_composite(&self) -> bool {
        matches!(self, Type::Composite(_) | Type::Record)
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
            "text" => Type::Text,
            "citext" => Type::CiText,
            "hstore" => Type::HStore,
            "varchar" | "character varying" => Type::Varchar,
            "char" | "character" | "bpchar" => Type::Bpchar,
            "name" => Type::Name,
            "\"char\"" => Type::Char,
            "json" => Type::Json,
            "oid" => Type::Oid,
            "tid" => Type::Tid,
            "xid" => Type::Xid,
            "cid" => Type::Cid,
            "regclass" => Type::Reg(RegKind::Class),
            "regtype" => Type::Reg(RegKind::Type),
            "regproc" => Type::Reg(RegKind::Proc),
            "regnamespace" => Type::Reg(RegKind::Namespace),
            "regrole" => Type::Reg(RegKind::Role),
            "regprocedure" => Type::Reg(RegKind::Procedure),
            "void" => Type::Void,
            "int2vector" => Type::Int2Vector,
            "oidvector" => Type::OidVector,
            "bytea" => Type::Bytea,
            "uuid" => Type::Uuid,
            "timestamptz" | "timestamp with time zone" => Type::Timestamptz,
            "timestamp" | "timestamp without time zone" => Type::Timestamp,
            "date" => Type::Date,
            "time" | "time without time zone" => Type::Time,
            "timetz" | "time with time zone" => Type::TimeTz,
            "interval" => Type::Interval,
            "jsonb" => Type::Jsonb,
            "jsonpath" => Type::JsonPath,
            "unknown" => Type::Unknown,
            "bit" => Type::Bit,
            "varbit" | "bit varying" => Type::VarBit,
            "money" => Type::Money,
            "inet" => Type::Inet,
            "cidr" => Type::Cidr,
            "macaddr" => Type::MacAddr,
            "macaddr8" => Type::MacAddr8,
            // `_int4`: an array type by its pg_type name.
            other if other.starts_with('_') && other.len() > 1 => return Type::from_sql_name(&other[1..]).map(Type::array_of),
            other => {
                let e = range::RANGES.iter().find(|e| e.5 == other || e.6 == other)?;
                let sub = Box::new(e.0.clone());
                if e.5 == other { Type::Range(sub) } else { Type::Multirange(sub) }
            }
        };
        Some(t)
    }

    pub fn is_integer(&self) -> bool {
        matches!(self, Type::Int2 | Type::Int4 | Type::Int8)
    }

    pub fn is_float(&self) -> bool {
        matches!(self, Type::Float4 | Type::Float8)
    }

    /// The string-category types that convert to `text` implicitly (`varchar`, `bpchar`,
    /// `name`, `"char"`) and `text` itself.
    pub fn is_string(&self) -> bool {
        matches!(self, Type::Text | Type::Varchar | Type::Bpchar | Type::Name | Type::Char)
    }

    /// The type a function or operator lookup sees: string types resolve through `text`, as
    /// Postgres' implicit casts make them do.
    pub fn lookup_type(&self) -> Type {
        let base = self.base();
        if base.is_string() || base == Type::CiText { Type::Text } else { base }
    }

    pub fn is_numeric(&self) -> bool {
        self.is_integer() || self.is_float() || matches!(self, Type::Numeric)
    }

    pub fn element(&self) -> Option<&Type> {
        match self {
            Type::Array(e) => Some(e),
            Type::Int2Vector => Some(&Type::Int2),
            Type::OidVector => Some(&Type::Oid),
            _ => None,
        }
    }

    /// The `pg_type.typcategory` letter, used by implicit-coercion and operator resolution.
    pub fn category(&self) -> char {
        match self {
            Type::Bool => 'B',
            Type::Int2 | Type::Int4 | Type::Int8 | Type::Float4 | Type::Float8 | Type::Numeric => 'N',
            Type::Text | Type::Varchar | Type::Bpchar | Type::Name => 'S',
            Type::Char => 'Z',
            Type::Void => 'P',
            Type::Int2Vector | Type::OidVector => 'A',
            Type::Oid | Type::Reg(_) => 'N',
            Type::Json => 'U',
            Type::Timestamptz | Type::Timestamp | Type::Date | Type::Time | Type::TimeTz => 'D',
            Type::Interval => 'T',
            Type::Array(_) => 'A',
            Type::Unknown => 'X',
            Type::Enum(_) => 'E',
            Type::Composite(_) | Type::Record => 'C',
            Type::Range(_) | Type::Multirange(_) => 'R',
            Type::CiText => 'S',
            Type::HStore => 'U',
            Type::Bit | Type::VarBit => 'V',
            Type::Money => 'N',
            Type::Inet | Type::Cidr => 'I',
            Type::MacAddr | Type::MacAddr8 => 'U',
            Type::Domain(_) => self.base().category(),
            Type::Bytea | Type::Uuid | Type::Jsonb | Type::JsonPath | Type::Tid | Type::Xid | Type::Cid => 'U',
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
            Type::Varchar,
            Type::Bpchar,
            Type::Name,
            Type::Char,
            Type::Json,
            Type::Oid,
            Type::Reg(RegKind::Class),
            Type::Reg(RegKind::Type),
            Type::array_of(Type::Varchar),
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
        assert_eq!(Type::from_sql_name("varchar"), Some(Type::Varchar));
        assert_eq!(Type::from_sql_name("\"char\""), Some(Type::Char));
        assert_eq!(Type::from_sql_name("nope"), None);
    }
}
