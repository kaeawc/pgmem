//! The slice of Postgres' catalog knowledge the deparser needs to reproduce parse analysis:
//! identifier quoting (`quote_identifier`), type names (`format_type`), and which operator or
//! function overload Postgres picks for given argument types (so the implicit casts parse
//! analysis inserts can be printed). Postgres has many cross-type operators (`int8 > int4`,
//! `date < timestamptz`, `float4 > float8`) for which no cast appears, so this cannot simply
//! reuse the executor's resolution (which promotes both operands to a common type).

use pgmem_types::Type;

/// Postgres keywords that `quote_identifier` must quote: every keyword that is not
/// UNRESERVED (reserved, column-name and type/function-name keywords).
fn is_quoted_keyword(s: &str) -> bool {
    crate::bind::is_reserved_keyword(s)
        || matches!(
            s,
            // TYPE_FUNC_NAME_KEYWORD
            "authorization"
                | "binary"
                | "collation"
                | "concurrently"
                | "cross"
                | "current_schema"
                | "freeze"
                | "full"
                | "ilike"
                | "inner"
                | "is"
                | "isnull"
                | "join"
                | "left"
                | "like"
                | "natural"
                | "notnull"
                | "outer"
                | "overlaps"
                | "right"
                | "similar"
                | "tablesample"
                | "verbose"
                // COL_NAME_KEYWORD
                | "between"
                | "bigint"
                | "bit"
                | "boolean"
                | "char"
                | "character"
                | "coalesce"
                | "dec"
                | "decimal"
                | "exists"
                | "extract"
                | "float"
                | "greatest"
                | "grouping"
                | "inout"
                | "int"
                | "integer"
                | "interval"
                | "json"
                | "json_array"
                | "json_arrayagg"
                | "json_exists"
                | "json_object"
                | "json_objectagg"
                | "json_query"
                | "json_scalar"
                | "json_serialize"
                | "json_table"
                | "json_value"
                | "least"
                | "merge_action"
                | "national"
                | "nchar"
                | "none"
                | "normalize"
                | "nullif"
                | "numeric"
                | "out"
                | "overlay"
                | "position"
                | "precision"
                | "real"
                | "row"
                | "setof"
                | "smallint"
                | "substring"
                | "time"
                | "timestamp"
                | "treat"
                | "trim"
                | "values"
                | "varchar"
                | "xmlattributes"
                | "xmlconcat"
                | "xmlelement"
                | "xmlexists"
                | "xmlforest"
                | "xmlnamespaces"
                | "xmlparse"
                | "xmlpi"
                | "xmlroot"
                | "xmlserialize"
                | "xmltable"
        )
}

/// `quote_identifier`: double-quotes a name unless it is a lower-case simple identifier that
/// is not a (non-unreserved) keyword.
pub fn quote_ident(s: &str) -> String {
    let simple = s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
    if simple && !is_quoted_keyword(s) {
        return s.to_owned();
    }
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `simple_quote_literal` (standard_conforming_strings is on: only quotes are doubled).
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `format_type_with_typemod`.
pub fn format_type(t: &Type, typmod: i32) -> String {
    match t {
        Type::Array(elem) => format!("{}[]", format_type(elem, typmod)),
        Type::Bpchar if typmod < 0 => "bpchar".into(),
        Type::Bpchar => format!("character({})", typmod - 4),
        Type::Varchar if typmod >= 4 => format!("character varying({})", typmod - 4),
        Type::Numeric if typmod >= 4 => {
            let m = typmod - 4;
            format!("numeric({},{})", (m >> 16) & 0xffff, m & 0xffff)
        }
        other => other.name(),
    }
}

fn int_rank(t: &Type) -> u8 {
    match t {
        Type::Int2 => 1,
        Type::Int4 => 2,
        _ => 3,
    }
}

fn wider_int(a: &Type, b: &Type) -> Type {
    if int_rank(a) >= int_rank(b) { a.clone() } else { b.clone() }
}

fn is_datetime(t: &Type) -> bool {
    matches!(t, Type::Date | Type::Timestamp | Type::Timestamptz)
}

/// The type an operand of a string operator is seen as: `varchar` has no operators of its own
/// (it is binary-coercible to `text`); `bpchar` and `name` do.
fn text_like(t: &Type) -> Type {
    match t {
        Type::Varchar | Type::Char => Type::Text,
        other => other.clone(),
    }
}

/// Argument types of the comparison operator Postgres picks for `l op r` (both known).
fn comparison_args(l: &Type, r: &Type) -> (Type, Type) {
    let same = |t: Type| (t.clone(), t);
    match (l, r) {
        _ if l == r => same(text_like(l)),
        _ if l.is_integer() && r.is_integer() => (l.clone(), r.clone()),
        _ if l.is_float() && r.is_float() => (l.clone(), r.clone()),
        (i, f) if i.is_integer() && f.is_float() => (Type::Float8, f.clone()),
        (f, i) if i.is_integer() && f.is_float() => (f.clone(), Type::Float8),
        (Type::Numeric, i) if i.is_integer() => same(Type::Numeric),
        (i, Type::Numeric) if i.is_integer() => same(Type::Numeric),
        (Type::Numeric, f) if f.is_float() => (Type::Float8, f.clone()),
        (f, Type::Numeric) if f.is_float() => (f.clone(), Type::Float8),
        _ if is_datetime(l) && is_datetime(r) => (l.clone(), r.clone()),
        (Type::Time, Type::TimeTz) | (Type::TimeTz, Type::Time) => same(Type::TimeTz),
        // name = text and text = name are cross-type operators.
        (Type::Name, Type::Text) | (Type::Text, Type::Name) => (l.clone(), r.clone()),
        _ if l.is_string() && r.is_string() => same(Type::Text),
        (Type::Array(a), Type::Array(b)) => {
            let (x, y) = comparison_args(a, b);
            if x == y { (Type::array_of(x), Type::array_of(y)) } else { (l.clone(), r.clone()) }
        }
        _ => (l.clone(), r.clone()),
    }
}

/// Argument and result types of `+ - * / %` for numeric operands.
fn numeric_arith(op: &str, l: &Type, r: &Type) -> Option<(Type, Type, Type)> {
    if !l.is_numeric() || !r.is_numeric() {
        return None;
    }
    Some(match (l, r) {
        _ if l.is_integer() && r.is_integer() => {
            let w = wider_int(l, r);
            if op == "%" || l == r { (w.clone(), w.clone(), w) } else { (l.clone(), r.clone(), w) }
        }
        _ if l.is_float() && r.is_float() => {
            let ret = if *l == Type::Float4 && *r == Type::Float4 { Type::Float4 } else { Type::Float8 };
            (l.clone(), r.clone(), ret)
        }
        (Type::Numeric, Type::Numeric) => (Type::Numeric, Type::Numeric, Type::Numeric),
        (Type::Numeric, i) | (i, Type::Numeric) if i.is_integer() => (Type::Numeric, Type::Numeric, Type::Numeric),
        _ => {
            // A float on one side: the other side goes to float8 (float4 has float48 operators).
            let lt = if l.is_float() { l.clone() } else { Type::Float8 };
            let rt = if r.is_float() { r.clone() } else { Type::Float8 };
            (lt, rt, Type::Float8)
        }
    })
}

/// Date/time arithmetic (`date - date`, `timestamptz + interval`, ...).
fn datetime_arith(op: &str, l: &Type, r: &Type) -> Option<(Type, Type, Type)> {
    use Type::*;
    let t = |a: &Type, b: &Type, ret: Type| Some((a.clone(), b.clone(), ret));
    match (op, l, r) {
        ("-", Date, Date) => t(l, r, Int4),
        ("+" | "-", Date, i) if i.is_integer() => t(l, &Int4, Date),
        ("+", i, Date) if i.is_integer() => t(&Int4, r, Date),
        ("+" | "-", Date, Interval) => t(l, r, Timestamp),
        ("+", Interval, Date) => t(l, r, Timestamp),
        ("+", Date, Time) => t(l, r, Timestamp),
        ("+" | "-", Timestamp | Timestamptz | Time | TimeTz, Interval) => t(l, r, l.clone()),
        ("+", Interval, Timestamp | Timestamptz | Time | TimeTz) => t(l, r, r.clone()),
        ("-", Timestamp, Timestamp) | ("-", Timestamptz, Timestamptz) => t(l, r, Interval),
        ("-", Timestamp | Timestamptz, Timestamp | Timestamptz) => t(&Timestamptz, &Timestamptz, Interval),
        ("-", Time, Time) => t(l, r, Interval),
        ("+" | "-", Interval, Interval) => t(l, r, Interval),
        ("*" | "/", Interval, n) if n.is_numeric() => t(l, &Float8, Interval),
        ("*", n, Interval) if n.is_numeric() => t(&Float8, r, Interval),
        _ => None,
    }
}

fn json_op(op: &str, l: &Type, r: &Type) -> Option<(Type, Type, Type)> {
    if !matches!(l, Type::Jsonb | Type::Json) {
        return None;
    }
    let text_arr = Type::array_of(Type::Text);
    let key = if r.is_integer() { Type::Int4 } else { Type::Text };
    Some(match op {
        "->" => (l.clone(), key, l.clone()),
        "->>" => (l.clone(), key, Type::Text),
        "#>" => (l.clone(), text_arr, l.clone()),
        "#>>" => (l.clone(), text_arr, Type::Text),
        "?" => (Type::Jsonb, Type::Text, Type::Bool),
        "?|" | "?&" => (Type::Jsonb, text_arr, Type::Bool),
        "@>" | "<@" => (Type::Jsonb, Type::Jsonb, Type::Bool),
        "#-" => (Type::Jsonb, text_arr, Type::Jsonb),
        "-" if r.is_integer() => (Type::Jsonb, Type::Int4, Type::Jsonb),
        "-" => (Type::Jsonb, Type::Text, Type::Jsonb),
        "||" => (Type::Jsonb, Type::Jsonb, Type::Jsonb),
        _ => return None,
    })
}

fn array_op(op: &str, l: &Type, r: &Type) -> Option<(Type, Type, Type)> {
    match (op, l, r) {
        ("@>" | "<@" | "&&", Type::Array(_), Type::Array(_)) => {
            let (a, b) = comparison_args(l, r);
            Some((a, b, Type::Bool))
        }
        ("||", Type::Array(_), Type::Array(_)) => Some((l.clone(), r.clone(), l.clone())),
        ("||", Type::Array(_), _) => Some((l.clone(), r.clone(), l.clone())),
        ("||", _, Type::Array(_)) => Some((l.clone(), r.clone(), r.clone())),
        _ => None,
    }
}

fn string_op(op: &str, l: &Type, r: &Type) -> Option<(Type, Type, Type)> {
    match op {
        // LIKE and regex operators exist for text, bpchar and name on the left.
        "~~" | "!~~" | "~~*" | "!~~*" | "~" | "!~" | "~*" | "!~*" => {
            let lt = if matches!(l, Type::Bpchar | Type::Name) { l.clone() } else { Type::Text };
            Some((lt, Type::Text, Type::Bool))
        }
        "^@" => Some((Type::Text, Type::Text, Type::Bool)),
        "||" => {
            let (ls, rs) = (l.is_string(), r.is_string());
            Some(match (ls, rs) {
                (true, true) => (Type::Text, Type::Text, Type::Text),
                // anynonarray || text and text || anynonarray: the other side is left alone.
                (true, false) => (Type::Text, r.clone(), Type::Text),
                (false, true) => (l.clone(), Type::Text, Type::Text),
                (false, false) => return None,
            })
        }
        _ => None,
    }
}

/// The type an unknown-typed literal operand takes when the other operand has type `other`.
fn unknown_peer(op: &str, other: &Type, unknown_on_right: bool) -> Type {
    match (op, other) {
        ("->" | "->>" | "?", Type::Jsonb | Type::Json) if unknown_on_right => Type::Text,
        ("#>" | "#>>" | "?|" | "?&" | "#-", Type::Jsonb | Type::Json) if unknown_on_right => Type::array_of(Type::Text),
        ("+" | "-", Type::Date | Type::Timestamp | Type::Timestamptz | Type::Time | Type::TimeTz) => Type::Interval,
        ("||", t) if !matches!(t, Type::Array(_) | Type::Jsonb) => Type::Text,
        ("~~" | "!~~" | "~~*" | "!~~*" | "~" | "!~" | "~*" | "!~*" | "^@", _) => Type::Text,
        (_, t) => t.clone(),
    }
}

/// Argument and result types of the binary operator Postgres resolves `l op r` to. Unknown
/// operands (string literals) take the type the operator wants.
pub fn resolve_binary(op: &str, l: &Type, r: &Type) -> (Type, Type, Type) {
    let (l, r) = match (l, r) {
        (Type::Unknown, Type::Unknown) => (Type::Text, Type::Text),
        (Type::Unknown, t) => (unknown_peer(op, t, false), t.clone()),
        (t, Type::Unknown) => (t.clone(), unknown_peer(op, t, true)),
        _ => (l.clone(), r.clone()),
    };
    if matches!(op, "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=") {
        let (a, b) = comparison_args(&l, &r);
        return (a, b, Type::Bool);
    }
    let found = datetime_arith(op, &l, &r)
        .or_else(|| json_op(op, &l, &r))
        .or_else(|| array_op(op, &l, &r))
        .or_else(|| string_op(op, &l, &r))
        .or_else(|| if matches!(op, "+" | "-" | "*" | "/" | "%") { numeric_arith(op, &l, &r) } else { None });
    if let Some(f) = found {
        return f;
    }
    match op {
        "&" | "|" | "#" if l.is_integer() && r.is_integer() => {
            let w = wider_int(&l, &r);
            (w.clone(), w.clone(), w)
        }
        "<<" | ">>" if l.is_integer() => (l.clone(), Type::Int4, l),
        "^" => {
            let t = if l == Type::Numeric || r == Type::Numeric { Type::Numeric } else { Type::Float8 };
            (t.clone(), t.clone(), t)
        }
        "&&" | "@>" | "<@" | "@@" => (l.clone(), r, Type::Bool),
        _ => (l.clone(), r, l),
    }
}

/// Argument and result type of a prefix operator.
pub fn resolve_unary(op: &str, t: &Type) -> (Type, Type) {
    let t = if *t == Type::Unknown { Type::Numeric } else { t.clone() };
    match op {
        "|/" | "||/" => (Type::Float8, Type::Float8),
        _ => (t.clone(), t),
    }
}

/// Signature (argument types after coercion, result type) of the builtin functions whose
/// Postgres signature the executor's resolver does not model exactly.
pub fn known_function(name: &str, args: &[Type]) -> Option<(Vec<Type>, Type)> {
    use Type::*;
    let n = args.len();
    let first = args.first().cloned().unwrap_or(Unknown);
    let keep = |ret: Type| Some((args.to_vec(), ret));
    let regclass = Reg(pgmem_types::RegKind::Class);
    match (name, n) {
        ("nextval" | "currval", 1) => Some((vec![regclass], Int8)),
        ("setval", 2) => Some((vec![regclass, Int8], Int8)),
        ("setval", 3) => Some((vec![regclass, Int8, Bool], Int8)),
        ("now" | "transaction_timestamp" | "statement_timestamp" | "clock_timestamp", 0) => keep(Timestamptz),
        ("gen_random_uuid" | "uuid_generate_v4" | "uuidv4" | "uuidv7", 0) => keep(Uuid),
        ("random", 0) => keep(Float8),
        ("extract", 2) => Some((vec![Text, datetime_source(&args[1])], Numeric)),
        ("date_part", 2) => Some((vec![Text, datetime_source(&args[1])], Float8)),
        ("date_trunc", 2) => {
            let src = if args[1] == Date { Timestamptz } else { datetime_source(&args[1]) };
            Some((vec![Text, src.clone()], src))
        }
        ("jsonb_build_object" | "jsonb_build_array", _) => keep(Jsonb),
        ("json_build_object" | "json_build_array", _) => keep(Json),
        ("concat", _) => keep(Text),
        ("concat_ws" | "format", 1..) => {
            let mut a = args.to_vec();
            a[0] = Text;
            Some((a, Text))
        }
        ("length" | "char_length" | "character_length" | "octet_length", 1) if first == Bpchar => keep(Int4),
        ("array_length", 2) => Some((vec![first, Int4], Int4)),
        ("cardinality" | "array_ndims", 1) => keep(Int4),
        ("to_jsonb", 1) => keep(Jsonb),
        ("to_json", 1) => keep(Json),
        _ => None,
    }
}

fn datetime_source(t: &Type) -> Type {
    match t {
        Type::Timestamp | Type::Timestamptz | Type::Interval | Type::Time | Type::TimeTz | Type::Date => t.clone(),
        _ => Type::Timestamptz,
    }
}

/// Coercion targets and result type of an aggregate call.
pub fn aggregate(name: &str, args: &[Type]) -> (Vec<Type>, Type) {
    use Type::*;
    let first = args.first().cloned().unwrap_or(Unknown);
    let resolved = |t: &Type| if *t == Unknown { Text } else { t.clone() };
    match name {
        "count" => (args.to_vec(), Int8),
        "sum" => {
            let ret = match first {
                Int2 | Int4 => Int8,
                Int8 | Numeric | Unknown => Numeric,
                ref t => t.clone(),
            };
            (vec![if first == Unknown { Numeric } else { first.clone() }], ret)
        }
        "avg" => {
            let ret = match first {
                Float4 | Float8 => Float8,
                Interval => Interval,
                _ => Numeric,
            };
            (vec![if first == Unknown { Numeric } else { first.clone() }], ret)
        }
        "min" | "max" => {
            // There is no min/max for varchar or name: they go through text.
            let t = match first {
                Varchar | Name | Char | Unknown => Text,
                ref t => t.clone(),
            };
            (vec![t.clone()], t)
        }
        "string_agg" => {
            if first == Bytea {
                (vec![Bytea, Bytea], Bytea)
            } else {
                (vec![Text, Text], Text)
            }
        }
        "array_agg" => (vec![resolved(&first)], Type::array_of(resolved(&first))),
        "bool_and" | "bool_or" | "every" => (vec![Bool], Bool),
        "jsonb_agg" => (vec![resolved(&first)], Jsonb),
        "json_agg" => (vec![resolved(&first)], Json),
        "jsonb_object_agg" => (args.iter().map(resolved).collect(), Jsonb),
        "json_object_agg" => (args.iter().map(resolved).collect(), Json),
        "stddev" | "stddev_samp" | "stddev_pop" | "variance" | "var_samp" | "var_pop" => {
            if first.is_float() {
                (vec![Float8], Float8)
            } else {
                (vec![first.clone()], Numeric)
            }
        }
        "bit_and" | "bit_or" | "bit_xor" => (vec![first.clone()], first),
        _ => (args.to_vec(), resolved(&first)),
    }
}

pub fn is_aggregate(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "string_agg"
            | "array_agg"
            | "bool_and"
            | "bool_or"
            | "every"
            | "jsonb_agg"
            | "json_agg"
            | "jsonb_object_agg"
            | "json_object_agg"
            | "stddev"
            | "stddev_samp"
            | "stddev_pop"
            | "variance"
            | "var_samp"
            | "var_pop"
            | "bit_and"
            | "bit_or"
            | "bit_xor"
            | "percentile_cont"
            | "percentile_disc"
            | "mode"
    )
}

/// Window-only functions: result type and coercion targets of their arguments.
pub fn window_function(name: &str, args: &[Type]) -> Option<(Vec<Type>, Type)> {
    use Type::*;
    let first = args.first().cloned().unwrap_or(Unknown);
    let value = if first == Unknown { Text } else { first.clone() };
    Some(match name {
        "row_number" | "rank" | "dense_rank" => (vec![], Int8),
        "percent_rank" | "cume_dist" => (vec![], Float8),
        "ntile" => (vec![Int4], Int4),
        "lag" | "lead" => {
            let mut a = vec![value.clone()];
            if args.len() > 1 {
                a.push(Int4);
            }
            if args.len() > 2 {
                a.push(value.clone());
            }
            (a, value)
        }
        "first_value" | "last_value" => (vec![value.clone()], value),
        "nth_value" => (vec![value.clone(), Int4], value),
        _ => return None,
    })
}

/// The common type of a set of expressions (`select_common_type`): unknowns defer to the
/// others, all-unknown resolves to text.
pub fn common_type(types: &[Type]) -> Type {
    let mut acc = Type::Unknown;
    for t in types {
        acc = match (&acc, t) {
            (Type::Unknown, t) => t.clone(),
            (a, Type::Unknown) => a.clone(),
            (a, t) if a == t => a.clone(),
            (a, t) => crate::funcs::common_type(a, t).unwrap_or_else(|| a.clone()),
        };
    }
    if acc == Type::Unknown { Type::Text } else { acc }
}
