//! Expression lowering.

use crate::query::{lower_order_by_expr, lower_query, lower_window_spec};
use crate::{ident, object_name, unsupported};
use pgmem_ir::{AnyAllRight, Expr, FunctionCall, Literal, Subscript, TypeName, UnaryOp};
use pgmem_types::{SqlError, SqlResult};
use sqlparser::ast as sq;

fn b(e: Expr) -> Box<Expr> {
    Box::new(e)
}

pub(crate) fn lower_expr(e: sq::Expr) -> SqlResult<Expr> {
    use sq::Expr as E;
    Ok(match e {
        E::Identifier(i) => lower_identifier(&i),
        E::CompoundIdentifier(parts) => Expr::Column(parts.iter().map(ident).collect()),
        E::Value(v) => lower_value(v.value)?,
        E::Nested(inner) => lower_expr(*inner)?,
        E::IsNull(x) => Expr::IsNull { expr: b(lower_expr(*x)?), negated: false },
        E::IsNotNull(x) => Expr::IsNull { expr: b(lower_expr(*x)?), negated: true },
        E::IsTrue(x) => Expr::IsBool { expr: b(lower_expr(*x)?), value: Some(true), negated: false },
        E::IsNotTrue(x) => Expr::IsBool { expr: b(lower_expr(*x)?), value: Some(true), negated: true },
        E::IsFalse(x) => Expr::IsBool { expr: b(lower_expr(*x)?), value: Some(false), negated: false },
        E::IsNotFalse(x) => Expr::IsBool { expr: b(lower_expr(*x)?), value: Some(false), negated: true },
        E::IsUnknown(x) => Expr::IsBool { expr: b(lower_expr(*x)?), value: None, negated: false },
        E::IsNotUnknown(x) => Expr::IsBool { expr: b(lower_expr(*x)?), value: None, negated: true },
        E::IsJson { expr, kind, unique_keys, negated } => {
            let kind = match kind {
                Some(sq::JsonPredicateType::Scalar) => "scalar",
                Some(sq::JsonPredicateType::Array) => "array",
                Some(sq::JsonPredicateType::Object) => "object",
                Some(sq::JsonPredicateType::Value) | None => "value",
            };
            let unique = matches!(unique_keys, Some(sq::JsonKeyUniqueness::WithUniqueKeys));
            let call = FunctionCall::new(
                "__pgmem_is_json",
                vec![lower_expr(*expr)?, Expr::Literal(Literal::String(kind.into())), Expr::Literal(Literal::Bool(unique))],
            );
            let e = Expr::Function(Box::new(call));
            if negated { Expr::Not(b(e)) } else { e }
        }
        E::IsDistinctFrom(l, r) => Expr::IsDistinctFrom { left: b(lower_expr(*l)?), right: b(lower_expr(*r)?), negated: false },
        E::IsNotDistinctFrom(l, r) => Expr::IsDistinctFrom { left: b(lower_expr(*l)?), right: b(lower_expr(*r)?), negated: true },
        E::In { expr, target, negated, .. } => {
            let expr = b(lower_expr(*expr)?);
            match target {
                sq::InTarget::List(list) => Expr::InList { expr, list: lower_exprs(list)?, negated },
                sq::InTarget::Subquery(q) => Expr::InSubquery { expr, query: Box::new(lower_query(*q)?), negated },
                sq::InTarget::Unnest(arr) => {
                    let any = Expr::AnyAll { left: expr, op: "=".into(), right: AnyAllRight::Array(b(lower_expr(*arr)?)), all: false };
                    if negated { Expr::Not(b(any)) } else { any }
                }
                sq::InTarget::Table { .. } => return Err(unsupported("IN table")),
            }
        }
        E::Between { expr, negated, low, high } => {
            let (low, symmetric) = match lower_expr(*low)? {
                Expr::Function(f) if f.name.base() == crate::SYMMETRIC_MARKER && f.args.len() == 1 => {
                    (f.args.into_iter().next().unwrap_or(Expr::Literal(pgmem_ir::Literal::Null)), true)
                }
                low => (low, false),
            };
            Expr::Between { expr: b(lower_expr(*expr)?), low: b(low), high: b(lower_expr(*high)?), negated, symmetric }
        }
        E::BinaryOp { left, op, right } => lower_binary(*left, &op, *right)?,
        E::Like { negated, any, expr, pattern, escape_char } => {
            if any {
                let op = if negated { "!~~" } else { "~~" };
                return Ok(Expr::AnyAll {
                    left: b(lower_expr(*expr)?),
                    op: op.into(),
                    right: AnyAllRight::Array(b(lower_expr(*pattern)?)),
                    all: false,
                });
            }
            let pattern = lower_expr(*pattern)?;
            // sqlparser reads `x LIKE ALL (arr)` as a call to a function named all.
            if let Expr::Function(f) = &pattern
                && f.name.0.len() == 1
                && matches!(f.name.base(), "all" | "any" | "some")
                && f.args.len() == 1
                && escape_char.is_none()
            {
                let op = if negated { "!~~" } else { "~~" };
                let all = f.name.base() == "all";
                let arr = f.args[0].clone();
                return Ok(Expr::AnyAll { left: b(lower_expr(*expr)?), op: op.into(), right: AnyAllRight::Array(b(arr)), all });
            }
            Expr::Like {
                expr: b(lower_expr(*expr)?),
                pattern: b(pattern),
                escape: escape_char.map(|e| lower_expr(*e)).transpose()?.map(Box::new),
                negated,
                case_insensitive: false,
            }
        }
        E::ILike { negated, any, expr, pattern, escape_char } => {
            if any {
                let op = if negated { "!~~*" } else { "~~*" };
                return Ok(Expr::AnyAll {
                    left: b(lower_expr(*expr)?),
                    op: op.into(),
                    right: AnyAllRight::Array(b(lower_expr(*pattern)?)),
                    all: false,
                });
            }
            let pattern = lower_expr(*pattern)?;
            // sqlparser reads `x LIKE ALL (arr)` as a call to a function named all.
            if let Expr::Function(f) = &pattern
                && f.name.0.len() == 1
                && matches!(f.name.base(), "all" | "any" | "some")
                && f.args.len() == 1
                && escape_char.is_none()
            {
                let op = if negated { "!~~*" } else { "~~*" };
                let all = f.name.base() == "all";
                let arr = f.args[0].clone();
                return Ok(Expr::AnyAll { left: b(lower_expr(*expr)?), op: op.into(), right: AnyAllRight::Array(b(arr)), all });
            }
            Expr::Like {
                expr: b(lower_expr(*expr)?),
                pattern: b(pattern),
                escape: escape_char.map(|e| lower_expr(*e)).transpose()?.map(Box::new),
                negated,
                case_insensitive: true,
            }
        }
        E::SimilarTo { negated, expr, pattern, escape_char } => Expr::SimilarTo {
            expr: b(lower_expr(*expr)?),
            pattern: b(lower_expr(*pattern)?),
            escape: escape_char.map(|e| lower_expr(*e)).transpose()?.map(Box::new),
            negated,
        },
        E::AnyOp { left, compare_op, right, .. } => lower_any_all(*left, &compare_op, *right, false)?,
        E::AllOp { left, compare_op, right } => lower_any_all(*left, &compare_op, *right, true)?,
        E::UnaryOp { op, expr } => lower_unary(op, *expr)?,
        E::Cast { expr, data_type, .. } => Expr::Cast { expr: b(lower_expr(*expr)?), ty: lower_type(&data_type)? },
        E::AtTimeZone { timestamp, time_zone } => Expr::AtTimeZone { expr: b(lower_expr(*timestamp)?), zone: b(lower_expr(*time_zone)?) },
        E::Extract { field, expr, .. } => Expr::Function(Box::new(FunctionCall::new(
            "extract",
            vec![Expr::Literal(Literal::String(datetime_field(&field))), lower_expr(*expr)?],
        ))),
        E::Ceil { expr, field } => lower_ceil_floor("ceil", *expr, field)?,
        E::Floor { expr, field } => lower_ceil_floor("floor", *expr, field)?,
        E::Position { expr, r#in } => {
            // POSITION(sub IN str) is position(str, sub) in Postgres' catalog.
            Expr::Function(Box::new(FunctionCall::new("position", vec![lower_expr(*r#in)?, lower_expr(*expr)?])))
        }
        // `x IS [NOT] [form] NORMALIZED` → is_normalized(x, 'form').
        E::IsNormalized { expr, form, negated } => {
            let form = form.map_or_else(|| "NFC".to_owned(), |f| f.to_string());
            let call = Expr::Function(Box::new(FunctionCall::new(
                "is_normalized",
                vec![lower_expr(*expr)?, Expr::Literal(Literal::String(form))],
            )));
            if negated { Expr::Not(b(call)) } else { call }
        }
        E::Substring { expr, substring_from, substring_for, shorthand, .. } => {
            let mut args = vec![lower_expr(*expr)?];
            match (substring_from, substring_for) {
                (Some(from), Some(len)) => {
                    args.push(lower_expr(*from)?);
                    args.push(lower_expr(*len)?);
                }
                (Some(from), None) => args.push(lower_expr(*from)?),
                (None, Some(len)) => {
                    args.push(Expr::Literal(Literal::Integer("1".into())));
                    args.push(lower_expr(*len)?);
                }
                (None, None) => {}
            }
            Expr::Function(Box::new(FunctionCall::new(if shorthand { "substr" } else { "substring" }, args)))
        }
        E::Trim { trim_where, trim_what, expr, trim_characters } => {
            let name = match trim_where {
                Some(sq::TrimWhereField::Leading) => "ltrim",
                Some(sq::TrimWhereField::Trailing) => "rtrim",
                _ => "btrim",
            };
            let mut args = vec![lower_expr(*expr)?];
            if let Some(what) = trim_what {
                args.push(lower_expr(*what)?);
            } else if let Some(chars) = trim_characters {
                args.extend(lower_exprs(chars)?);
            }
            Expr::Function(Box::new(FunctionCall::new(name, args)))
        }
        E::Overlay { expr, overlay_what, overlay_from, overlay_for } => {
            let mut args = vec![lower_expr(*expr)?, lower_expr(*overlay_what)?, lower_expr(*overlay_from)?];
            if let Some(f) = overlay_for {
                args.push(lower_expr(*f)?);
            }
            Expr::Function(Box::new(FunctionCall::new("overlay", args)))
        }
        E::Collate { expr, collation } => Expr::Collate { expr: b(lower_expr(*expr)?), collation: object_name(&collation)?.to_string() },
        E::TypedString(ts) => {
            let ty = lower_type(&ts.data_type)?;
            match string_value(&ts.value.value) {
                Some(value) => Expr::TypedString { ty, value },
                None => return Err(SqlError::syntax("typed literal must be a string")),
            }
        }
        E::Interval(iv) => lower_interval(iv)?,
        E::Function(f) => lower_function(f)?,
        E::Case { operand, conditions, else_result, .. } => Expr::Case {
            operand: operand.map(|o| lower_expr(*o)).transpose()?.map(Box::new),
            whens: conditions.into_iter().map(|w| Ok((lower_expr(w.condition)?, lower_expr(w.result)?))).collect::<SqlResult<Vec<_>>>()?,
            else_result: else_result.map(|e| lower_expr(*e)).transpose()?.map(Box::new),
        },
        E::Exists { subquery, negated } => Expr::Exists { query: Box::new(lower_query(*subquery)?), negated },
        E::Subquery(q) => Expr::Subquery(Box::new(lower_query(*q)?)),
        E::Tuple(items) => Expr::Row(lower_exprs(items)?),
        E::Array(arr) => Expr::Array(lower_exprs(arr.elem)?),
        E::CompoundFieldAccess { root, access_chain } => {
            // `t.col[1]` arrives as `t` with `.col[1]` accessors: dotted names right after a
            // bare identifier are a column reference, as in Postgres' grammar (`columnref`);
            // only a parenthesized root (`(t).col`) selects a field.
            let mut chain = access_chain.into_iter().peekable();
            let mut cur = match *root {
                root @ (E::Identifier(_) | E::CompoundIdentifier(_)) => {
                    let mut parts = match &root {
                        E::Identifier(i) => vec![ident(i)],
                        E::CompoundIdentifier(ids) => ids.iter().map(ident).collect(),
                        _ => Vec::new(),
                    };
                    while let Some(sq::AccessExpr::Dot(sq::Expr::Identifier(i))) = chain.peek() {
                        parts.push(ident(i));
                        chain.next();
                    }
                    if parts.len() == 1 { lower_expr(root)? } else { Expr::Column(parts) }
                }
                other => lower_expr(other)?,
            };
            for access in chain {
                cur = match access {
                    sq::AccessExpr::Subscript(sq::Subscript::Index { index }) => {
                        Expr::Subscript { expr: b(cur), index: Subscript::Index(b(lower_expr(index)?)) }
                    }
                    sq::AccessExpr::Subscript(sq::Subscript::Slice { lower_bound, upper_bound, stride }) => {
                        if stride.is_some() {
                            return Err(unsupported("array slice stride"));
                        }
                        Expr::Subscript {
                            expr: b(cur),
                            index: Subscript::Slice {
                                lower: lower_bound.map(lower_expr).transpose()?.map(Box::new),
                                upper: upper_bound.map(lower_expr).transpose()?.map(Box::new),
                            },
                        }
                    }
                    sq::AccessExpr::Dot(sq::Expr::Identifier(field)) => Expr::FieldAccess { expr: b(cur), field: ident(&field) },
                    sq::AccessExpr::Dot(_) => return Err(unsupported("composite field access")),
                };
            }
            cur
        }
        E::Wildcard(_) | E::QualifiedWildcard(..) => return Err(SqlError::syntax("syntax error at or near \"*\"")),
        other => return Err(unsupported(&format!("expression `{other}`"))),
    })
}

pub(crate) fn lower_exprs(items: Vec<sq::Expr>) -> SqlResult<Vec<Expr>> {
    items.into_iter().map(lower_expr).collect()
}

/// Unquoted SQL-standard "niladic" keywords parse as identifiers in some positions; Postgres
/// treats them as function calls. The bare `DEFAULT` keyword is the INSERT/UPDATE marker.
fn lower_identifier(i: &sq::Ident) -> Expr {
    if i.quote_style.is_none() {
        let lower = i.value.to_ascii_lowercase();
        match lower.as_str() {
            "default" => return Expr::Default,
            "current_timestamp" | "current_date" | "current_time" | "localtimestamp" | "localtime" | "current_user" | "session_user"
            | "current_role" | "user" | "current_catalog" | "current_schema" => {
                return Expr::Function(Box::new(FunctionCall::new(&lower, Vec::new())));
            }
            _ => {}
        }
    }
    Expr::Column(vec![ident(i)])
}

fn string_value(v: &sq::Value) -> Option<String> {
    match v {
        sq::Value::SingleQuotedString(s)
        | sq::Value::EscapedStringLiteral(s)
        | sq::Value::UnicodeStringLiteral(s)
        | sq::Value::NationalStringLiteral(s)
        | sq::Value::TripleSingleQuotedString(s) => Some(s.clone()),
        sq::Value::DollarQuotedString(d) => Some(d.value.clone()),
        _ => None,
    }
}

fn lower_value(v: sq::Value) -> SqlResult<Expr> {
    if let Some(s) = string_value(&v) {
        return Ok(Expr::Literal(Literal::String(s)));
    }
    Ok(match v {
        sq::Value::Number(n, _) => {
            if n.bytes().all(|c| c.is_ascii_digit() || c == b'_') {
                Expr::Literal(Literal::Integer(n.replace('_', "")))
            } else if let Some(hex) = n.strip_prefix("0x").or_else(|| n.strip_prefix("0X")) {
                let v = i64::from_str_radix(hex, 16).map_err(|_| SqlError::syntax(format!("invalid hexadecimal integer \"{n}\"")))?;
                Expr::Literal(Literal::Integer(v.to_string()))
            } else {
                Expr::Literal(Literal::Decimal(n.replace('_', "")))
            }
        }
        sq::Value::Boolean(v) => Expr::Literal(Literal::Bool(v)),
        sq::Value::Null => Expr::Literal(Literal::Null),
        sq::Value::Placeholder(p) => {
            let n = p
                .strip_prefix('$')
                .and_then(|d| d.parse::<usize>().ok())
                .ok_or_else(|| SqlError::syntax(format!("syntax error at or near \"{p}\"")))?;
            if n == 0 {
                return Err(SqlError::new(pgmem_types::code::UNDEFINED_PARAMETER, "there is no parameter $0"));
            }
            Expr::Param(n)
        }
        sq::Value::DoubleQuotedString(s) => Expr::Column(vec![s]),
        other => return Err(unsupported(&format!("literal {other}"))),
    })
}

pub(crate) fn binary_op_name(op: &sq::BinaryOperator) -> SqlResult<String> {
    use sq::BinaryOperator as B;
    Ok(match op {
        B::Plus => "+".into(),
        B::Minus => "-".into(),
        B::Multiply => "*".into(),
        B::Divide => "/".into(),
        B::Modulo => "%".into(),
        B::StringConcat => "||".into(),
        B::Gt => ">".into(),
        B::Lt => "<".into(),
        B::GtEq => ">=".into(),
        B::LtEq => "<=".into(),
        B::Eq => "=".into(),
        B::NotEq => "<>".into(),
        B::BitwiseOr => "|".into(),
        B::BitwiseAnd => "&".into(),
        B::BitwiseXor | B::PGBitwiseXor => "#".into(),
        B::PGBitwiseShiftLeft => "<<".into(),
        B::PGBitwiseShiftRight => ">>".into(),
        B::PGExp => "^".into(),
        B::PGOverlap => "&&".into(),
        B::PGRegexMatch => "~".into(),
        B::PGRegexIMatch => "~*".into(),
        B::PGRegexNotMatch => "!~".into(),
        B::PGRegexNotIMatch => "!~*".into(),
        B::PGLikeMatch => "~~".into(),
        B::PGILikeMatch => "~~*".into(),
        B::PGNotLikeMatch => "!~~".into(),
        B::PGNotILikeMatch => "!~~*".into(),
        B::PGStartsWith => "^@".into(),
        B::Arrow => "->".into(),
        B::LongArrow => "->>".into(),
        B::HashArrow => "#>".into(),
        B::HashLongArrow => "#>>".into(),
        B::AtArrow => "@>".into(),
        B::ArrowAt => "<@".into(),
        B::HashMinus => "#-".into(),
        B::Question => "?".into(),
        B::QuestionAnd => "?&".into(),
        B::QuestionPipe => "?|".into(),
        B::AtQuestion => "@?".into(),
        B::AtAt => "@@".into(),
        B::Custom(s) => s.clone(),
        B::PGCustomBinaryOperator(parts) => parts.last().cloned().unwrap_or_default(),
        other => return Err(unsupported(&format!("operator {other}"))),
    })
}

/// Parameter names (and defaults) of the built-in functions that take named arguments.
fn builtin_params(name: &str) -> Option<&'static [(&'static str, Option<Literal>)]> {
    use Literal as L;
    static MAKE_INTERVAL: std::sync::LazyLock<Vec<(&str, Option<Literal>)>> = std::sync::LazyLock::new(|| {
        let z = || Some(L::Integer("0".into()));
        vec![
            ("years", z()),
            ("months", z()),
            ("weeks", z()),
            ("days", z()),
            ("hours", z()),
            ("mins", z()),
            ("secs", Some(L::Decimal("0.0".into()))),
        ]
    });
    static MAKE_DATE: [(&str, Option<Literal>); 3] = [("year", None), ("month", None), ("day", None)];
    static MAKE_TIME: [(&str, Option<Literal>); 3] = [("hour", None), ("min", None), ("sec", None)];
    static MAKE_TIMESTAMP: [(&str, Option<Literal>); 6] =
        [("year", None), ("month", None), ("mday", None), ("hour", None), ("min", None), ("sec", None)];
    static MAKE_TIMESTAMPTZ: [(&str, Option<Literal>); 7] =
        [("year", None), ("month", None), ("mday", None), ("hour", None), ("min", None), ("sec", None), ("timezone", None)];
    static JSONB_SET: [(&str, Option<Literal>); 4] =
        [("jsonb_in", None), ("path", None), ("replacement", None), ("create_if_missing", Some(L::Bool(true)))];
    static JSONB_INSERT: [(&str, Option<Literal>); 4] =
        [("jsonb_in", None), ("path", None), ("replacement", None), ("insert_after", Some(L::Bool(false)))];
    static JSONB_PATH: std::sync::LazyLock<Vec<(&str, Option<Literal>)>> = std::sync::LazyLock::new(|| {
        vec![("target", None), ("path", None), ("vars", Some(L::String("{}".into()))), ("silent", Some(L::Bool(false)))]
    });
    Some(match name {
        "make_interval" => MAKE_INTERVAL.as_slice(),
        "make_date" => &MAKE_DATE,
        "make_time" => &MAKE_TIME,
        "make_timestamp" => &MAKE_TIMESTAMP,
        "make_timestamptz" => &MAKE_TIMESTAMPTZ,
        "jsonb_set" => &JSONB_SET,
        "jsonb_insert" => &JSONB_INSERT,
        "jsonb_path_exists"
        | "jsonb_path_match"
        | "jsonb_path_query"
        | "jsonb_path_query_array"
        | "jsonb_path_query_first"
        | "jsonb_path_exists_tz"
        | "jsonb_path_match_tz"
        | "jsonb_path_query_tz"
        | "jsonb_path_query_array_tz"
        | "jsonb_path_query_first_tz" => JSONB_PATH.as_slice(),
        _ => return None,
    })
}

/// Places `name => value` arguments by parameter position, filling defaults for the rest.
fn place_named_args(func: &str, positional: Vec<Expr>, named: Vec<(String, Expr)>) -> SqlResult<Vec<Expr>> {
    let Some(params) = builtin_params(func) else { return Err(unsupported("named arguments for this function")) };
    let mut slots: Vec<Option<Expr>> = vec![None; params.len()];
    for (i, e) in positional.into_iter().enumerate() {
        *slots.get_mut(i).ok_or_else(|| SqlError::new("42883", format!("function {func} does not exist")))? = Some(e);
    }
    for (name, e) in named {
        let i = params
            .iter()
            .position(|(p, _)| *p == name)
            .ok_or_else(|| SqlError::new("42883", format!("function {func} has no parameter named \"{name}\"")))?;
        if slots[i].is_some() {
            return Err(SqlError::syntax(format!("argument name \"{name}\" used more than once")));
        }
        slots[i] = Some(e);
    }
    // Trailing parameters without a value take their defaults (or are omitted when optional).
    let last = slots.iter().rposition(Option::is_some).unwrap_or(0);
    let mut out = Vec::new();
    for (i, slot) in slots.into_iter().enumerate() {
        match (slot, &params[i].1) {
            (Some(e), _) => out.push(e),
            (None, Some(d)) => out.push(Expr::Literal(d.clone())),
            (None, None) if i > last => break,
            (None, None) => return Err(SqlError::new("42883", format!("function {func} does not exist"))),
        }
    }
    Ok(out)
}

fn lower_binary(left: sq::Expr, op: &sq::BinaryOperator, right: sq::Expr) -> SqlResult<Expr> {
    match op {
        sq::BinaryOperator::And => Ok(Expr::And(b(lower_expr(left)?), b(lower_expr(right)?))),
        sq::BinaryOperator::Or => Ok(Expr::Or(b(lower_expr(left)?), b(lower_expr(right)?))),
        // `(s1, e1) OVERLAPS (s2, e2)` is the function overlaps(s1, e1, s2, e2).
        sq::BinaryOperator::Overlaps => {
            let pair = |e: sq::Expr, side: &str| -> SqlResult<Vec<Expr>> {
                match e {
                    sq::Expr::Tuple(items) if items.len() == 2 => items.into_iter().map(lower_expr).collect(),
                    _ => Err(SqlError::syntax(format!("wrong number of parameters on {side} side of OVERLAPS expression"))),
                }
            };
            let mut args = pair(left, "left")?;
            args.extend(pair(right, "right")?);
            Ok(Expr::Function(Box::new(FunctionCall::new("overlaps", args))))
        }
        _ => {
            let name = binary_op_name(op)?;
            let (l, r) = (lower_expr(left)?, lower_expr(right)?);
            // `~~` and friends are LIKE spelled as operators.
            let like = |negated, ci| Expr::Like { expr: b(l.clone()), pattern: b(r.clone()), escape: None, negated, case_insensitive: ci };
            Ok(match name.as_str() {
                "~~" => like(false, false),
                "!~~" => like(true, false),
                "~~*" => like(false, true),
                "!~~*" => like(true, true),
                _ => Expr::Binary { op: name, left: b(l), right: b(r) },
            })
        }
    }
}

fn lower_any_all(left: sq::Expr, op: &sq::BinaryOperator, right: sq::Expr, all: bool) -> SqlResult<Expr> {
    let op = binary_op_name(op)?;
    let right = match right {
        sq::Expr::Subquery(q) => AnyAllRight::Subquery(Box::new(lower_query(*q)?)),
        sq::Expr::Nested(inner) if matches!(*inner, sq::Expr::Subquery(_)) => match *inner {
            sq::Expr::Subquery(q) => AnyAllRight::Subquery(Box::new(lower_query(*q)?)),
            _ => unreachable_expr()?,
        },
        other => AnyAllRight::Array(b(lower_expr(other)?)),
    };
    Ok(Expr::AnyAll { left: b(lower_expr(left)?), op, right, all })
}

fn unreachable_expr() -> SqlResult<AnyAllRight> {
    Err(SqlError::internal("unexpected ANY/ALL operand"))
}

fn lower_unary(op: sq::UnaryOperator, expr: sq::Expr) -> SqlResult<Expr> {
    use sq::UnaryOperator as U;
    // Postgres folds a minus sign into a numeric constant so `-2147483648` is an int4.
    if let (U::Minus, sq::Expr::Value(v)) = (&op, &expr)
        && let sq::Value::Number(n, _) = &v.value
    {
        return Ok(match lower_value(sq::Value::Number(n.clone(), false))? {
            Expr::Literal(Literal::Integer(s)) => Expr::Literal(Literal::Integer(format!("-{s}"))),
            Expr::Literal(Literal::Decimal(s)) => Expr::Literal(Literal::Decimal(format!("-{s}"))),
            other => Expr::Unary { op: UnaryOp::Minus, expr: b(other) },
        });
    }
    let inner = lower_expr(expr)?;
    Ok(match op {
        U::Not => Expr::Not(b(inner)),
        U::Minus => Expr::Unary { op: UnaryOp::Minus, expr: b(inner) },
        U::Plus => Expr::Unary { op: UnaryOp::Plus, expr: b(inner) },
        U::BitwiseNot => Expr::Unary { op: UnaryOp::BitNot, expr: b(inner) },
        U::PGSquareRoot => Expr::Unary { op: UnaryOp::SquareRoot, expr: b(inner) },
        U::PGCubeRoot => Expr::Unary { op: UnaryOp::CubeRoot, expr: b(inner) },
        U::PGAbs => Expr::Unary { op: UnaryOp::Abs, expr: b(inner) },
        other => return Err(unsupported(&format!("unary operator {other}"))),
    })
}

fn lower_ceil_floor(name: &str, expr: sq::Expr, field: sq::CeilFloorKind) -> SqlResult<Expr> {
    let mut args = vec![lower_expr(expr)?];
    match field {
        sq::CeilFloorKind::DateTimeField(sq::DateTimeField::NoDateTime) => {}
        sq::CeilFloorKind::Scale(v) => args.push(lower_value(v.value)?),
        sq::CeilFloorKind::DateTimeField(_) => return Err(unsupported(&format!("{name}(... TO field)"))),
    }
    Ok(Expr::Function(Box::new(FunctionCall::new(name, args))))
}

/// The lower-case Postgres spelling of an EXTRACT / interval field.
pub(crate) fn datetime_field(f: &sq::DateTimeField) -> String {
    use sq::DateTimeField as F;
    match f {
        F::Year | F::Years => "year".into(),
        F::Month | F::Months => "month".into(),
        F::Week(_) | F::Weeks | F::IsoWeek => "week".into(),
        F::Day | F::Days => "day".into(),
        F::DayOfWeek | F::Dow => "dow".into(),
        F::DayOfYear | F::Doy => "doy".into(),
        F::Hour | F::Hours => "hour".into(),
        F::Minute | F::Minutes => "minute".into(),
        F::Second | F::Seconds => "second".into(),
        F::Century => "century".into(),
        F::Decade => "decade".into(),
        F::Epoch => "epoch".into(),
        F::Isodow => "isodow".into(),
        F::Isoyear => "isoyear".into(),
        F::Julian => "julian".into(),
        F::Microsecond | F::Microseconds => "microseconds".into(),
        F::Millisecond | F::Milliseconds => "milliseconds".into(),
        F::Millenium | F::Millennium => "millennium".into(),
        F::Quarter => "quarter".into(),
        F::Timezone => "timezone".into(),
        F::TimezoneHour => "timezone_hour".into(),
        F::TimezoneMinute => "timezone_minute".into(),
        F::Custom(i) => i.value.to_ascii_lowercase(),
        other => other.to_string().to_ascii_lowercase(),
    }
}

fn lower_interval(iv: sq::Interval) -> SqlResult<Expr> {
    let value = match *iv.value {
        sq::Expr::Value(v) => string_value(&v.value).or_else(|| match &v.value {
            sq::Value::Number(n, _) => Some(n.clone()),
            _ => None,
        }),
        _ => None,
    }
    .ok_or_else(|| unsupported("non-literal INTERVAL expression"))?;
    // `interval '1' day` means one day; `interval '1' day to second` keeps the literal.
    let value = match (&iv.leading_field, &iv.last_field) {
        (Some(field), None) if value.trim().parse::<f64>().is_ok() => format!("{value} {}", datetime_field(field)),
        _ => value,
    };
    Ok(Expr::TypedString { ty: TypeName::simple("interval"), value })
}

/// Lowers an SQL/JSON constructor to the internal function implementing its NULL handling,
/// cast to its RETURNING type.
fn lower_json_constructor(base: &str, list: sq::FunctionArgumentList, filter: Option<Box<sq::Expr>>, over: bool) -> SqlResult<Expr> {
    if over {
        return Err(unsupported("SQL/JSON aggregates as window functions"));
    }
    let mut args = Vec::new();
    for arg in list.args {
        match arg {
            sq::FunctionArg::Unnamed(sq::FunctionArgExpr::Expr(e)) => args.push(lower_expr(e)?),
            sq::FunctionArg::ExprNamed { name, arg: sq::FunctionArgExpr::Expr(v), .. } => {
                args.push(lower_expr(name)?);
                args.push(lower_expr(v)?);
            }
            sq::FunctionArg::Named { name, arg: sq::FunctionArgExpr::Expr(v), .. } => {
                args.push(Expr::Column(vec![ident(&name)]));
                args.push(lower_expr(v)?);
            }
            _ => return Err(unsupported(&format!("this {} argument", base.to_uppercase()))),
        }
    }
    // Defaults: JSON_OBJECT / JSON_OBJECTAGG keep NULLs, JSON_ARRAY / JSON_ARRAYAGG drop them.
    let mut absent = matches!(base, "json_array" | "json_arrayagg");
    let mut returning = None;
    let mut order_by = Vec::new();
    for clause in list.clauses {
        match clause {
            sq::FunctionArgumentClause::JsonNullClause(sq::JsonNullClause::AbsentOnNull) => absent = true,
            sq::FunctionArgumentClause::JsonNullClause(sq::JsonNullClause::NullOnNull) => absent = false,
            sq::FunctionArgumentClause::JsonReturningClause(r) => returning = Some(lower_type(&r.data_type)?),
            sq::FunctionArgumentClause::OrderBy(items) => {
                order_by = items.into_iter().map(lower_order_by_expr).collect::<SqlResult<Vec<_>>>()?;
            }
            other => return Err(unsupported(&format!("{} clause {other}", base.to_uppercase()))),
        }
    }
    let internal = match (base, absent) {
        ("json_object", false) => "__pgmem_json_object",
        ("json_object", true) => "__pgmem_json_object_absent",
        ("json_array", false) => "__pgmem_json_array",
        ("json_array", true) => "__pgmem_json_array_absent",
        ("json_arrayagg", false) => "__pgmem_json_arrayagg",
        ("json_arrayagg", true) => "__pgmem_json_arrayagg_strict",
        ("json_objectagg", false) => "__pgmem_json_objectagg",
        _ => "__pgmem_json_objectagg_strict",
    };
    let mut call = FunctionCall::new(internal, args);
    call.order_by = order_by;
    if let Some(f) = filter {
        call.filter = Some(b(lower_expr(*f)?));
    }
    let e = Expr::Function(Box::new(call));
    Ok(match returning {
        Some(ty) if ty.name != "json" => Expr::Cast { expr: b(e), ty },
        _ => e,
    })
}

fn lower_function(f: sq::Function) -> SqlResult<Expr> {
    let name = object_name(&f.name)?;
    let base = name.base().to_owned();
    let mut call = FunctionCall { name: name.clone(), ..FunctionCall::new(&base, Vec::new()) };
    call.name = name;
    match f.args {
        sq::FunctionArguments::None => {}
        sq::FunctionArguments::Subquery(q) => {
            if base == "array" {
                return Ok(Expr::ArraySubquery(Box::new(lower_query(*q)?)));
            }
            call.args.push(Expr::Subquery(Box::new(lower_query(*q)?)));
        }
        sq::FunctionArguments::List(list) => {
            call.distinct = matches!(list.duplicate_treatment, Some(sq::DuplicateTreatment::Distinct));
            // SQL/JSON constructors: `JSON_OBJECT(k VALUE v, k : v ... [NULL|ABSENT ON NULL]
            // [RETURNING type])`, `JSON_ARRAY(...)`, `JSON_ARRAYAGG(...)`, `JSON_OBJECTAGG(...)`.
            if matches!(base.as_str(), "json_object" | "json_array" | "json_arrayagg" | "json_objectagg")
                && (base != "json_object"
                    || list.args.is_empty()
                    || list.args.iter().any(|a| matches!(a, sq::FunctionArg::Named { .. } | sq::FunctionArg::ExprNamed { .. }))
                    || !list.clauses.is_empty())
            {
                return lower_json_constructor(&base, list, f.filter, f.over.is_some());
            }
            let mut named: Vec<(String, Expr)> = Vec::new();
            for arg in list.args {
                match arg {
                    sq::FunctionArg::Named { name, arg: sq::FunctionArgExpr::Expr(e), .. }
                    | sq::FunctionArg::ExprNamed { name: sq::Expr::Identifier(name), arg: sq::FunctionArgExpr::Expr(e), .. } => {
                        named.push((ident(&name), lower_expr(e)?));
                    }
                    sq::FunctionArg::Unnamed(sq::FunctionArgExpr::Expr(e)) => {
                        if !named.is_empty() {
                            return Err(SqlError::syntax("positional argument cannot follow named argument"));
                        }
                        call.args.push(lower_expr(e)?);
                    }
                    sq::FunctionArg::Unnamed(sq::FunctionArgExpr::Wildcard) => call.star = true,
                    sq::FunctionArg::Unnamed(sq::FunctionArgExpr::QualifiedWildcard(n)) => {
                        call.args.push(Expr::Column({
                            let mut parts = object_name(&n)?.0;
                            parts.push("*".into());
                            parts
                        }));
                    }
                    _ => return Err(unsupported("named function arguments")),
                }
            }
            if !named.is_empty() {
                call.args = place_named_args(&base, std::mem::take(&mut call.args), named)?;
            }
            // `normalize(x, NFKC)`: the form is a keyword.
            if base == "normalize"
                && let Some(Expr::Column(parts)) = call.args.get(1)
                && let [form] = parts.as_slice()
            {
                call.args[1] = Expr::Literal(Literal::String(form.to_ascii_uppercase()));
            }
            // `VARIADIC arr` (marked by the token rewrite).
            if let Some(Expr::Function(v)) = call.args.last()
                && v.name.base() == crate::VARIADIC_MARKER
                && v.args.len() == 1
                && let Some(Expr::Function(v)) = call.args.pop()
            {
                call.args.extend(v.args);
                call.variadic = true;
            }
            for clause in list.clauses {
                match clause {
                    sq::FunctionArgumentClause::OrderBy(items) => {
                        call.order_by = items.into_iter().map(lower_order_by_expr).collect::<SqlResult<Vec<_>>>()?;
                    }
                    sq::FunctionArgumentClause::Where(e) => call.filter = Some(b(lower_expr(e)?)),
                    other => return Err(unsupported(&format!("function argument clause {other}"))),
                }
            }
        }
    }
    // `ROW(a, b)` is a row constructor, not a function call.
    if base == "row" && call.name.0.len() == 1 && f.filter.is_none() && f.over.is_none() && !call.star {
        return Ok(Expr::Row(call.args));
    }
    if let Some(filter) = f.filter {
        call.filter = Some(b(lower_expr(*filter)?));
    }
    if !f.within_group.is_empty() {
        call.within_group = f.within_group.into_iter().map(lower_order_by_expr).collect::<SqlResult<Vec<_>>>()?;
    }
    call.over = match f.over {
        None => None,
        Some(sq::WindowType::NamedWindow(n)) => Some(pgmem_ir::WindowSpec { base: Some(ident(&n)), ..Default::default() }),
        Some(sq::WindowType::WindowSpec(spec)) => Some(lower_window_spec(spec)?),
    };
    Ok(Expr::Function(Box::new(call)))
}

/// Lowers a sqlparser data type to a type name as Postgres would canonicalize it.
pub(crate) fn lower_type(dt: &sq::DataType) -> SqlResult<TypeName> {
    use sq::DataType as D;
    let char_len = |l: &Option<sq::CharacterLength>| match l {
        Some(sq::CharacterLength::IntegerLength { length, .. }) => i64::try_from(*length).map(|n| vec![n]).unwrap_or_default(),
        _ => Vec::new(),
    };
    let exact = |info: &sq::ExactNumberInfo| match info {
        sq::ExactNumberInfo::None => Vec::new(),
        sq::ExactNumberInfo::Precision(p) => vec![i64::try_from(*p).unwrap_or(0)],
        sq::ExactNumberInfo::PrecisionAndScale(p, s) => vec![i64::try_from(*p).unwrap_or(0), *s],
    };
    let simple = |n: &str| TypeName::simple(n);
    Ok(match dt {
        D::Int(_) | D::Int4(_) | D::Integer(_) => simple("int4"),
        D::BigInt(_) | D::Int8(_) => simple("int8"),
        D::SmallInt(_) | D::Int2(_) => simple("int2"),
        D::Text => simple("text"),
        D::Varchar(l) | D::CharacterVarying(l) | D::CharVarying(l) | D::Nvarchar(l) => {
            TypeName { modifiers: char_len(l), ..simple("varchar") }
        }
        // A bare `char` / `character` is `character(1)`.
        D::Char(l) | D::Character(l) => {
            let modifiers = if l.is_none() { vec![1] } else { char_len(l) };
            TypeName { modifiers, ..simple("bpchar") }
        }
        D::Bool | D::Boolean => simple("bool"),
        D::Real | D::Float4 => simple("float4"),
        D::DoublePrecision | D::Float8 | D::Double(_) | D::Float64 => simple("float8"),
        D::Float(info) => match info {
            sq::ExactNumberInfo::Precision(p) if *p <= 24 => simple("float4"),
            _ => simple("float8"),
        },
        D::Numeric(info) | D::Decimal(info) | D::Dec(info) => TypeName { modifiers: exact(info), ..simple("numeric") },
        D::Uuid => simple("uuid"),
        D::Bytea => simple("bytea"),
        D::Timestamp(p, tz) => {
            let name = if matches!(tz, sq::TimezoneInfo::WithTimeZone | sq::TimezoneInfo::Tz) { "timestamptz" } else { "timestamp" };
            TypeName { modifiers: p.iter().filter_map(|n| i64::try_from(*n).ok()).collect(), ..simple(name) }
        }
        D::Time(p, tz) => TypeName {
            modifiers: p.iter().filter_map(|n| i64::try_from(*n).ok()).collect(),
            ..simple(if matches!(tz, sq::TimezoneInfo::WithTimeZone | sq::TimezoneInfo::Tz) { "timetz" } else { "time" })
        },
        D::Date => simple("date"),
        D::Interval { precision, .. } => {
            TypeName { modifiers: precision.iter().filter_map(|n| i64::try_from(*n).ok()).collect(), ..simple("interval") }
        }
        D::JSON => simple("json"),
        D::JSONB => simple("jsonb"),
        D::Regclass => simple("regclass"),
        D::Bit(_) => simple("bit"),
        D::BitVarying(_) | D::VarBit(_) => simple("varbit"),
        // The quoted name `"char"` is the internal single-byte type, not character(1).
        D::Custom(name, _)
            if name.0.len() == 1 && name.0[0].as_ident().is_some_and(|i| i.quote_style == Some('"') && i.value == "char") =>
        {
            simple("\"char\"")
        }
        D::Custom(name, mods) => {
            let n = object_name(name)?;
            TypeName {
                name: n.base().to_owned(),
                modifiers: mods.iter().filter_map(|m| m.to_string().parse().ok()).collect(),
                array_dims: 0,
                schema: n.schema().map(str::to_owned),
            }
        }
        D::Array(elem) => {
            let inner = match elem {
                sq::ArrayElemTypeDef::SquareBracket(t, _)
                | sq::ArrayElemTypeDef::AngleBracket(t)
                | sq::ArrayElemTypeDef::Parenthesis(t) => t,
                sq::ArrayElemTypeDef::Qualified(t, _) | sq::ArrayElemTypeDef::ParenthesisNotNull(t) => t,
                sq::ArrayElemTypeDef::None => return Err(SqlError::syntax("array type without element type")),
            };
            let mut t = lower_type(inner)?;
            t.array_dims += 1;
            t
        }
        other => return Err(unsupported(&format!("type {other}"))),
    })
}
