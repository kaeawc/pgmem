//! Expression binding.

use super::{Binder, Clause, Ctx, Level, agg_misuse, resolve_type_name, window_misuse};
use crate::cast::{CoercionContext, cast_context, cast_value, cast_value_ctx};
use crate::funcs::{self, EQ, common_type, comparison, resolve_binary, resolve_function, resolve_unary};
use crate::plan::{AggCall, AggKind, Expr, Frame, FrameBound, FrameUnits, SortKey, SubqueryKind, WinCall, WinKind};
use pgmem_ir as ir;
use pgmem_types::usertype::RECORD_OID;
use pgmem_types::{Numeric, SqlError, SqlResult, Type, Value, code, text};

pub type Bound = (Expr, Type);

/// Converts a bound expression to `to`. Untyped string literals are parsed directly into the
/// target type (so `'abc'::int` fails at bind time, like Postgres), untyped parameters take
/// the target type, and constants are folded.
pub fn coerce(b: &Binder, e: Expr, from: &Type, to: &Type, context: CoercionContext) -> SqlResult<Expr> {
    coerce_inner(b, e, from, to, -1, context, true)
}

/// Assignment coercion into a column of type `to` with modifier `typmod` (INSERT / UPDATE).
pub fn coerce_assignment(b: &Binder, e: Expr, from: &Type, to: &Type, typmod: i32, column: &str) -> SqlResult<Expr> {
    if cast_context(from, to).is_none_or(|c| c > CoercionContext::Assignment) {
        return Err(SqlError::datatype_mismatch(format!(
            "column \"{column}\" is of type {} but expression is of type {}",
            to.name(),
            from.name()
        ))
        .with_hint("You will need to rewrite or cast the expression."));
    }
    coerce_inner(b, e, from, to, typmod, CoercionContext::Assignment, false)
}

fn coerce_inner(b: &Binder, e: Expr, from: &Type, to: &Type, typmod: i32, context: CoercionContext, explicit: bool) -> SqlResult<Expr> {
    if *to == Type::Unknown {
        return Ok(e);
    }
    if from == to && typmod < 0 {
        return Ok(e);
    }
    // Into a domain: convert to its base type, then check its constraints.
    if let Type::Domain(oid) = to {
        return b.coerce_to_domain(e, from, *oid, typmod, context, explicit);
    }
    // Out of a domain: the value already is a value of the base type.
    if from.is_domain() {
        return coerce_inner(b, e, &from.base(), to, typmod, context, explicit);
    }
    if let Type::Composite(oid) = to
        && *from == Type::Record
    {
        return b.coerce_record(e, *oid, context, explicit);
    }
    // Any composite is a `record`.
    if *to == Type::Record && from.is_composite() {
        return Ok(e);
    }
    // bpchar → another string type drops the padding (`rtrim1`), as Postgres' cast does.
    if *from == Type::Bpchar && to.is_string() && *to != Type::Bpchar {
        let trimmed = match e {
            Expr::Const(v) => Expr::Const(crate::cast::bpchar_trim(v)),
            other => Expr::Call { func: crate::funcs::strict("rtrim", crate::cast::bpchar_trim_fn), args: vec![other] },
        };
        return coerce_inner(b, trimmed, &Type::Text, to, typmod, context, explicit);
    }
    // reg* values are resolved against the catalog when the statement runs.
    if let Type::Reg(_) = to {
        if *from != Type::Unknown && cast_context(from, to).is_none_or(|c| c > context) {
            return Err(SqlError::cannot_coerce(&from.name(), &to.name()));
        }
        if let (Expr::Param(i), Type::Unknown) = (&e, from) {
            b.set_param_type(*i, to);
            return Ok(e);
        }
        return Ok(Expr::Cast { expr: Box::new(e), to: to.clone(), typmod: -1 });
    }
    match (&e, from) {
        (Expr::Param(i), Type::Unknown) => {
            b.set_param_type(*i, to);
            return Ok(if typmod >= 0 { Expr::Cast { expr: Box::new(e), to: to.clone(), typmod } } else { e });
        }
        (Expr::Const(Value::Text(s)), Type::Unknown) => {
            let v = text::decode(s, to)?;
            return Ok(Expr::Const(cast_value_ctx(v, to, typmod, explicit)?));
        }
        (Expr::Const(Value::Null), _) => return Ok(Expr::Const(Value::Null)),
        _ => {}
    }
    if *from != Type::Unknown && cast_context(from, to).is_none_or(|c| c > context) {
        return Err(SqlError::cannot_coerce(&from.name(), &to.name()));
    }
    if let Expr::Const(v) = &e {
        return Ok(Expr::Const(cast_value_ctx(v.clone(), to, typmod, explicit)?));
    }
    Ok(Expr::Cast { expr: Box::new(e), to: to.clone(), typmod })
}

impl Binder<'_> {
    /// `CoerceToDomain`: the conversion to the domain's base type (with the domain's typmod),
    /// wrapped in a check of its NOT NULL and CHECK constraints.
    fn coerce_to_domain(&self, e: Expr, from: &Type, oid: u32, typmod: i32, context: CoercionContext, explicit: bool) -> SqlResult<Expr> {
        let def = self.catalog.type_by_oid(oid).filter(|t| t.oid == oid).cloned();
        let Some(def) = def else { return Err(SqlError::internal(format!("cache lookup failed for type {oid}"))) };
        let pgmem_catalog::TypeDefKind::Domain { base, typmod: dmod, not_null, checks, .. } = &def.kind else {
            return Err(SqlError::internal(format!("type {} is not a domain", def.name)));
        };
        let inner = coerce_inner(self, e, from, base, if typmod >= 0 { typmod } else { *dmod }, context, explicit)?;
        let checks = checks.iter().map(|c| Ok((c.name.clone(), self.bind_domain_check(&c.expr, base)?))).collect::<SqlResult<Vec<_>>>()?;
        Ok(Expr::CoerceToDomain(Box::new(crate::plan::DomainCoercion {
            expr: inner,
            domain: def.name.clone(),
            not_null: *not_null,
            checks,
        })))
    }

    /// Binds a domain CHECK expression; `VALUE` is the only column in scope.
    pub fn bind_domain_check(&self, e: &ir::Expr, base: &Type) -> SqlResult<Expr> {
        let level = Level::new(super::Scope::from_cols(vec![super::ScopeCol::new(None, "value", base.clone())]));
        let ctx = Ctx::new(&level).with_clause(Clause::Check);
        self.bind_predicate(e, "CHECK", &ctx)
    }

    /// `coerce_record_to_complex`: a row value becomes a named composite, field by field.
    fn coerce_record(&self, e: Expr, oid: u32, context: CoercionContext, explicit: bool) -> SqlResult<Expr> {
        let fields = crate::usertypes::composite_fields(oid);
        let target = Type::Composite(oid);
        let Expr::Row { fields: items, types, .. } = e else {
            // A record computed at run time is converted when the statement runs.
            return Ok(Expr::Cast { expr: Box::new(e), to: target, typmod: -1 });
        };
        let cannot = || SqlError::cannot_coerce("record", &target.name());
        if items.len() < fields.len() {
            return Err(cannot().with_detail("Input has too few columns."));
        }
        if items.len() > fields.len() {
            return Err(cannot().with_detail("Input has too many columns."));
        }
        let mut out = Vec::with_capacity(items.len());
        for ((item, ty), f) in items.into_iter().zip(types).zip(&fields) {
            if ty != Type::Unknown && cast_context(&ty, &f.ty).is_none_or(|c| c > context) {
                return Err(cannot().with_detail(format!(
                    "Cannot cast type {} to {} in column {}.",
                    ty.name(),
                    f.ty.name(),
                    out.len() + 1
                )));
            }
            out.push(coerce_inner(self, item, &ty, &f.ty, f.typmod, context, explicit)?);
        }
        let types = fields.iter().map(|f| f.ty.clone()).collect();
        Ok(Expr::Row { fields: out, types, type_oid: oid })
    }
}

/// The column name Postgres derives for an unaliased select-list expression (`FigureColname`).
pub fn output_name(e: &ir::Expr) -> String {
    figure_colname(e).map_or_else(|| "?column?".into(), |(name, _)| name)
}

/// `FigureColnameInternal`: a name and its strength. Column references and function calls
/// (strength 2) beat the type name of a cast around them; a cast of anything weaker (a literal,
/// a CASE, another cast) is named after its target type (strength 1).
fn figure_colname(e: &ir::Expr) -> Option<(String, u8)> {
    match e {
        ir::Expr::Column(parts) => parts.last().filter(|p| p.as_str() != "*").map(|p| (p.clone(), 2)),
        // SQL/JSON constructs bound as internal functions keep their SQL names.
        ir::Expr::Function(f) if f.name.base() == "__pgmem_is_json" => None,
        ir::Expr::Function(f) if f.name.base().starts_with("__pgmem_json_") => {
            let rest = &f.name.base()["__pgmem_".len()..];
            let name = rest.strip_suffix("_absent").or_else(|| rest.strip_suffix("_strict")).unwrap_or(rest);
            Some((name.to_owned(), 2))
        }
        ir::Expr::Function(f) => Some((f.name.base().to_owned(), 2)),
        ir::Expr::Cast { expr, ty } => match figure_colname(expr) {
            Some((name, 2)) => Some((name, 2)),
            _ => Some((type_display_name(ty), 1)),
        },
        // `B'1010'` / `X'1F'` constants (no length limit) are unnamed, like literals.
        ir::Expr::TypedString { ty, .. } if ty.name == "bit" && ty.modifiers == [-1] => None,
        ir::Expr::TypedString { ty, .. } => Some((type_display_name(ty), 1)),
        ir::Expr::Case { .. } => Some(("case".into(), 1)),
        // `x AT TIME ZONE z` is the function call timezone(z, x).
        ir::Expr::AtTimeZone { .. } => Some(("timezone".into(), 2)),
        ir::Expr::Exists { .. } => Some(("exists".into(), 2)),
        ir::Expr::ArraySubquery(_) | ir::Expr::Array(_) => Some(("array".into(), 2)),
        ir::Expr::Row(_) => Some(("row".into(), 2)),
        // A scalar subquery is named after its first output column, `?column?` included.
        ir::Expr::Subquery(q) => {
            let mut body = &q.body;
            while let ir::SetExpr::SetOp { left, .. } = body {
                body = left;
            }
            match body {
                ir::SetExpr::Select(s) => match s.projection.first() {
                    Some(ir::SelectItem::Expr { alias: Some(a), .. }) => Some((a.clone(), 2)),
                    Some(ir::SelectItem::Expr { expr, .. }) => Some((output_name(expr), 2)),
                    _ => None,
                },
                _ => None,
            }
        }
        ir::Expr::Subscript { expr, .. } | ir::Expr::Collate { expr, .. } => figure_colname(expr),
        ir::Expr::FieldAccess { field, .. } => Some((field.clone(), 2)),
        _ => None,
    }
}

/// Postgres' name for a type in a cast-derived column name (`'1'::int` → `int4`).
fn type_display_name(t: &ir::TypeName) -> String {
    // An array cast is named after its element type (`'{1}'::int[]` → `int4`).
    Type::from_sql_name(&t.name).map_or_else(|| t.name.clone(), |ty| ty.typname())
}

fn literal(l: &ir::Literal) -> SqlResult<Bound> {
    Ok(match l {
        ir::Literal::Null => (Expr::Const(Value::Null), Type::Unknown),
        ir::Literal::Bool(b) => (Expr::Const(Value::Bool(*b)), Type::Bool),
        ir::Literal::Integer(s) => match s.parse::<i64>() {
            Ok(n) => match i32::try_from(n) {
                Ok(n) => (Expr::Const(Value::Int4(n)), Type::Int4),
                Err(_) => (Expr::Const(Value::Int8(n)), Type::Int8),
            },
            Err(_) => (Expr::Const(Value::Numeric(Numeric::parse(s)?)), Type::Numeric),
        },
        ir::Literal::Decimal(s) => (Expr::Const(Value::Numeric(Numeric::parse(s)?)), Type::Numeric),
        ir::Literal::String(s) => (Expr::Const(Value::Text(s.clone())), Type::Unknown),
    })
}

/// The common type of a set of expressions (CASE branches, COALESCE args, VALUES columns,
/// UNION columns), following `select_common_type`: all unknown → text.
pub fn common_of(types: &[Type], what: &str) -> SqlResult<Type> {
    let mut acc = Type::Unknown;
    for t in types {
        acc = match (&acc, t) {
            (Type::Unknown, t) => t.clone(),
            (a, Type::Unknown) => a.clone(),
            (a, t) => common_type(a, t)
                .filter(|c| *c != Type::Unknown)
                .ok_or_else(|| SqlError::datatype_mismatch(format!("{what} types {} and {} cannot be matched", a.name(), t.name())))?,
        };
    }
    Ok(if acc == Type::Unknown { Type::Text } else { acc })
}

fn aggregate_kind(name: &str) -> Option<AggKind> {
    Some(match name {
        "count" => AggKind::Count,
        "sum" => AggKind::Sum,
        "avg" => AggKind::Avg,
        "min" => AggKind::Min,
        "max" => AggKind::Max,
        "bool_and" | "every" => AggKind::BoolAnd,
        "bool_or" => AggKind::BoolOr,
        "string_agg" => AggKind::StringAgg,
        "array_agg" => AggKind::ArrayAgg,
        "range_agg" => AggKind::RangeAgg,
        "range_intersect_agg" => AggKind::RangeIntersectAgg,
        "jsonb_agg" => AggKind::JsonbAgg,
        "json_agg" | "__pgmem_json_arrayagg" => AggKind::JsonAgg,
        "jsonb_agg_strict" => AggKind::JsonbAggStrict,
        "json_agg_strict" | "__pgmem_json_arrayagg_strict" => AggKind::JsonAggStrict,
        "jsonb_object_agg" => AggKind::JsonbObjectAgg,
        "json_object_agg" | "__pgmem_json_objectagg" => AggKind::JsonObjectAgg,
        "jsonb_object_agg_strict" => AggKind::JsonbObjectAggStrict,
        "json_object_agg_strict" | "__pgmem_json_objectagg_strict" => AggKind::JsonObjectAggStrict,
        "stddev" | "stddev_samp" => AggKind::StddevSamp,
        "stddev_pop" => AggKind::StddevPop,
        "variance" | "var_samp" => AggKind::VarSamp,
        "var_pop" => AggKind::VarPop,
        "bit_and" => AggKind::BitAnd,
        "bit_or" => AggKind::BitOr,
        "bit_xor" => AggKind::BitXor,
        "corr" => AggKind::Corr,
        "covar_pop" => AggKind::CovarPop,
        "covar_samp" => AggKind::CovarSamp,
        "regr_count" => AggKind::RegrCount,
        "regr_sxx" => AggKind::RegrSxx,
        "regr_syy" => AggKind::RegrSyy,
        "regr_sxy" => AggKind::RegrSxy,
        "regr_avgx" => AggKind::RegrAvgX,
        "regr_avgy" => AggKind::RegrAvgY,
        "regr_r2" => AggKind::RegrR2,
        "regr_slope" => AggKind::RegrSlope,
        "regr_intercept" => AggKind::RegrIntercept,
        "percentile_cont" => AggKind::PercentileCont,
        "percentile_disc" => AggKind::PercentileDisc,
        "mode" => AggKind::Mode,
        _ => return None,
    })
}

fn window_kind(name: &str) -> Option<WinKind> {
    Some(match name {
        "row_number" => WinKind::RowNumber,
        "rank" => WinKind::Rank,
        "dense_rank" => WinKind::DenseRank,
        "percent_rank" => WinKind::PercentRank,
        "cume_dist" => WinKind::CumeDist,
        "ntile" => WinKind::Ntile,
        "lag" => WinKind::Lag,
        "lead" => WinKind::Lead,
        "first_value" => WinKind::FirstValue,
        "last_value" => WinKind::LastValue,
        "nth_value" => WinKind::NthValue,
        _ => return None,
    })
}

/// Argument coercion targets and result type of an aggregate.
fn aggregate_types(kind: AggKind, name: &str, args: &[Type]) -> SqlResult<(Vec<Type>, Type)> {
    let err = || funcs::no_function(name, args);
    // Aggregates see a domain's base type.
    let args: &[Type] = &args.iter().map(Type::base).collect::<Vec<_>>();
    let first = args.first().cloned().unwrap_or(Type::Unknown);
    let numeric_arg = |t: &Type| if *t == Type::Unknown { Type::Numeric } else { t.clone() };
    Ok(match kind {
        AggKind::CountStar => (vec![], Type::Int8),
        AggKind::Count => (vec![if first == Type::Unknown { Type::Text } else { first }], Type::Int8),
        AggKind::Sum => {
            let t = numeric_arg(&first);
            let ret = match t {
                Type::Int2 | Type::Int4 => Type::Int8,
                Type::Int8 | Type::Numeric => Type::Numeric,
                Type::Float4 => Type::Float4,
                Type::Float8 => Type::Float8,
                Type::Interval => Type::Interval,
                _ => return Err(err()),
            };
            (vec![t], ret)
        }
        AggKind::Avg => {
            let t = numeric_arg(&first);
            let ret = match t {
                Type::Int2 | Type::Int4 | Type::Int8 | Type::Numeric => Type::Numeric,
                Type::Float4 | Type::Float8 => Type::Float8,
                Type::Interval => Type::Interval,
                _ => return Err(err()),
            };
            (vec![t], ret)
        }
        AggKind::Min | AggKind::Max => {
            let t = if first == Type::Unknown { Type::Text } else { first };
            if matches!(t, Type::Jsonb | Type::Bool | Type::Uuid) && !matches!(t, Type::Uuid) {
                return Err(err());
            }
            (vec![t.clone()], t)
        }
        AggKind::BoolAnd | AggKind::BoolOr => (vec![Type::Bool], Type::Bool),
        AggKind::RangeAgg | AggKind::RangeIntersectAgg => match &first {
            Type::Range(s) => {
                let ret = if kind == AggKind::RangeAgg { Type::Multirange(s.clone()) } else { first.clone() };
                (vec![first.clone()], ret)
            }
            Type::Multirange(_) => (vec![first.clone()], first.clone()),
            _ => return Err(err()),
        },
        AggKind::StringAgg => match args {
            [Type::Bytea, _] => (vec![Type::Bytea, Type::Bytea], Type::Bytea),
            [_, _] => (vec![Type::Text, Type::Text], Type::Text),
            _ => return Err(err()),
        },
        AggKind::ArrayAgg => {
            let t = if first == Type::Unknown { Type::Text } else { first };
            // array_agg(anyarray) stacks the inputs into one more dimension.
            let ret = if matches!(t, Type::Array(_)) { t.clone() } else { Type::array_of(t.clone()) };
            (vec![t], ret)
        }
        AggKind::JsonbAgg | AggKind::JsonbAggStrict | AggKind::JsonAgg | AggKind::JsonAggStrict => {
            let ret = if matches!(kind, AggKind::JsonAgg | AggKind::JsonAggStrict) { Type::Json } else { Type::Jsonb };
            (vec![if first == Type::Unknown { Type::Text } else { first }], ret)
        }
        AggKind::JsonbObjectAgg | AggKind::JsonbObjectAggStrict | AggKind::JsonObjectAgg | AggKind::JsonObjectAggStrict => {
            let ret = if matches!(kind, AggKind::JsonObjectAgg | AggKind::JsonObjectAggStrict) { Type::Json } else { Type::Jsonb };
            let v = args.get(1).cloned().unwrap_or(Type::Text);
            (vec![if first == Type::Unknown { Type::Text } else { first }, if v == Type::Unknown { Type::Text } else { v }], ret)
        }
        AggKind::StddevSamp | AggKind::StddevPop | AggKind::VarSamp | AggKind::VarPop => {
            let t = numeric_arg(&first);
            match t {
                Type::Float4 | Type::Float8 => (vec![Type::Float8], Type::Float8),
                Type::Int2 | Type::Int4 | Type::Int8 | Type::Numeric => (vec![t], Type::Numeric),
                _ => return Err(err()),
            }
        }
        AggKind::BitAnd | AggKind::BitOr | AggKind::BitXor => {
            if !first.is_integer() {
                return Err(err());
            }
            (vec![first.clone()], first)
        }
        AggKind::Corr
        | AggKind::CovarPop
        | AggKind::CovarSamp
        | AggKind::RegrCount
        | AggKind::RegrSxx
        | AggKind::RegrSyy
        | AggKind::RegrSxy
        | AggKind::RegrAvgX
        | AggKind::RegrAvgY
        | AggKind::RegrR2
        | AggKind::RegrSlope
        | AggKind::RegrIntercept => {
            if !args.iter().all(|t| t.is_numeric() || *t == Type::Unknown) {
                return Err(err());
            }
            (vec![Type::Float8, Type::Float8], if kind == AggKind::RegrCount { Type::Int8 } else { Type::Float8 })
        }
        AggKind::PercentileCont
        | AggKind::PercentileDisc
        | AggKind::Mode
        | AggKind::HypRank
        | AggKind::HypDenseRank
        | AggKind::HypPercentRank
        | AggKind::HypCumeDist => return Err(err()),
    })
}

impl Binder<'_> {
    pub fn bind_expr(&self, e: &ir::Expr, ctx: &Ctx) -> SqlResult<Bound> {
        if matches!(e, ir::Expr::Subquery(_) | ir::Expr::Exists { .. } | ir::Expr::InSubquery { .. } | ir::Expr::ArraySubquery(_)) {
            let place = match ctx.clause {
                Clause::Check => Some("check constraint"),
                Clause::Default => Some("DEFAULT expression"),
                _ => None,
            };
            if let Some(place) = place {
                return Err(SqlError::feature_not_supported(format!("cannot use subquery in {place}")));
            }
        }
        match e {
            ir::Expr::Literal(l) => literal(l),
            ir::Expr::Column(parts) => self.bind_column(parts, ctx),
            ir::Expr::Param(n) => {
                let idx = n - 1;
                Ok((Expr::Param(idx), self.param_type(idx)))
            }
            ir::Expr::TypedString { ty, value } => {
                let (t, typmod) = resolve_type_name(self.catalog, ty)?;
                if let Type::Reg(_) = t {
                    let e = Expr::Cast { expr: Box::new(Expr::Const(Value::Text(value.clone()))), to: t.clone(), typmod: -1 };
                    return Ok((e, t));
                }
                let v = cast_value(text::decode(value, &t)?, &t, typmod)?;
                Ok((Expr::Const(v), t))
            }
            ir::Expr::Unary { op, expr } => {
                let (inner, t) = self.bind_expr(expr, ctx)?;
                let name = match op {
                    ir::UnaryOp::Minus => "-",
                    ir::UnaryOp::Plus => "+",
                    ir::UnaryOp::BitNot => "~",
                    ir::UnaryOp::SquareRoot => "|/",
                    ir::UnaryOp::CubeRoot => "||/",
                    ir::UnaryOp::Abs => "@",
                };
                let r = resolve_unary(name, &t)?;
                let arg = coerce(self, inner, &t, &r.args[0], CoercionContext::Explicit)?;
                Ok((fold(Expr::Call { func: r.func, args: vec![arg] }), r.ret))
            }
            ir::Expr::Binary { op, left, right } => self.bind_binary(op, left, right, ctx),
            ir::Expr::And(l, r) => {
                let l = self.bind_bool(l, "AND", ctx)?;
                let r = self.bind_bool(r, "AND", ctx)?;
                Ok((flatten(Expr::And(vec![l, r])), Type::Bool))
            }
            ir::Expr::Or(l, r) => {
                let l = self.bind_bool(l, "OR", ctx)?;
                let r = self.bind_bool(r, "OR", ctx)?;
                Ok((flatten(Expr::Or(vec![l, r])), Type::Bool))
            }
            ir::Expr::Not(inner) => Ok((Expr::Not(Box::new(self.bind_bool(inner, "NOT", ctx)?)), Type::Bool)),
            ir::Expr::IsNull { expr, negated } => {
                let (e, t) = self.bind_expr(expr, ctx)?;
                if let Expr::Param(i) = &e
                    && t == Type::Unknown
                {
                    // Postgres cannot infer a type from IS NULL either.
                    let _ = i;
                }
                Ok((Expr::IsNull { expr: Box::new(e), negated: *negated }, Type::Bool))
            }
            ir::Expr::IsBool { expr, value, negated } => {
                let e = self.bind_bool(expr, "IS", ctx)?;
                Ok((
                    match value {
                        None => Expr::IsNull { expr: Box::new(e), negated: *negated },
                        Some(v) => {
                            Expr::IsDistinct { left: Box::new(e), right: Box::new(Expr::Const(Value::Bool(*v))), negated: !*negated }
                        }
                    },
                    Type::Bool,
                ))
            }
            ir::Expr::IsDistinctFrom { left, right, negated } => {
                let (l, lt) = self.bind_expr(left, ctx)?;
                let (r, rt) = self.bind_expr(right, ctx)?;
                let t = common_type(&lt, &rt).ok_or_else(|| funcs::no_operator("=", Some(&lt), &rt))?;
                let l = coerce(self, l, &lt, &t, CoercionContext::Implicit)?;
                let r = coerce(self, r, &rt, &t, CoercionContext::Implicit)?;
                Ok((Expr::IsDistinct { left: Box::new(l), right: Box::new(r), negated: *negated }, Type::Bool))
            }
            ir::Expr::Between { expr, low, high, negated, symmetric } => {
                let ge = ir::Expr::Binary { op: ">=".into(), left: expr.clone(), right: low.clone() };
                let le = ir::Expr::Binary { op: "<=".into(), left: expr.clone(), right: high.clone() };
                let mut combined = ir::Expr::And(Box::new(ge), Box::new(le));
                if *symmetric {
                    let ge2 = ir::Expr::Binary { op: ">=".into(), left: expr.clone(), right: high.clone() };
                    let le2 = ir::Expr::Binary { op: "<=".into(), left: expr.clone(), right: low.clone() };
                    combined = ir::Expr::Or(Box::new(combined), Box::new(ir::Expr::And(Box::new(ge2), Box::new(le2))));
                }
                let (b, _) = self.bind_expr(&combined, ctx)?;
                Ok((if *negated { Expr::Not(Box::new(b)) } else { b }, Type::Bool))
            }
            ir::Expr::InList { expr, list, negated } => self.bind_in_list(expr, list, *negated, ctx),
            ir::Expr::InSubquery { expr, query, negated } => {
                if let ir::Expr::Row(items) = &**expr {
                    return self.bind_row_in_subquery(items, query, *negated, ctx);
                }
                let (probe, pt) = self.bind_expr(expr, ctx)?;
                let (plan, ct, correlated) = self.bind_subquery_column(query, ctx).map_err(|e| {
                    if e.message == "subquery must return only one column" { SqlError::syntax("subquery has too many columns") } else { e }
                })?;
                let t = common_type(&pt, &ct).ok_or_else(|| funcs::no_operator("=", Some(&pt), &ct))?;
                let probe = coerce(self, probe, &pt, &t, CoercionContext::Implicit)?;
                let plan = cast_plan_column(plan, &ct, &t);
                let sub = Expr::Subquery {
                    id: self.fresh_id(),
                    plan: Box::new(plan),
                    kind: SubqueryKind::Quantified { probe: Box::new(probe), cmp: EQ, all: false },
                    correlated,
                };
                Ok((if *negated { Expr::Not(Box::new(sub)) } else { sub }, Type::Bool))
            }
            ir::Expr::Exists { query, negated } => {
                let (plan, correlated) = self.bind_subquery_plan(query, ctx)?;
                let sub = Expr::Subquery { id: self.fresh_id(), plan: Box::new(plan), kind: SubqueryKind::Exists, correlated };
                Ok((if *negated { Expr::Not(Box::new(sub)) } else { sub }, Type::Bool))
            }
            ir::Expr::Subquery(query) => {
                let (plan, t, correlated) = self.bind_subquery_column(query, ctx)?;
                Ok((Expr::Subquery { id: self.fresh_id(), plan: Box::new(plan), kind: SubqueryKind::Scalar, correlated }, t))
            }
            ir::Expr::ArraySubquery(query) => {
                let (plan, t, correlated) = self.bind_subquery_column(query, ctx)?;
                Ok((Expr::Subquery { id: self.fresh_id(), plan: Box::new(plan), kind: SubqueryKind::Array, correlated }, Type::array_of(t)))
            }
            ir::Expr::AnyAll { left, op, right, all } => self.bind_any_all(left, op, right, *all, ctx),
            ir::Expr::Like { expr, pattern, escape, negated, case_insensitive } => {
                let func = if *case_insensitive {
                    funcs::strict("~~*", funcs::string::ilike_fn)
                } else {
                    funcs::strict("~~", funcs::string::like_fn)
                };
                let call =
                    self.bind_text_call(func, expr, pattern, escape.as_deref(), if *case_insensitive { "~~*" } else { "~~" }, ctx)?;
                Ok((if *negated { Expr::Not(Box::new(call)) } else { call }, Type::Bool))
            }
            ir::Expr::SimilarTo { expr, pattern, escape, negated } => {
                let call = self.bind_text_call(
                    funcs::strict("similar to", funcs::string::similar_fn),
                    expr,
                    pattern,
                    escape.as_deref(),
                    "~",
                    ctx,
                )?;
                Ok((if *negated { Expr::Not(Box::new(call)) } else { call }, Type::Bool))
            }
            ir::Expr::Case { operand, whens, else_result } => self.bind_case(operand.as_deref(), whens, else_result.as_deref(), ctx),
            ir::Expr::Cast { expr, ty } => self.bind_cast(expr, ty, ctx),
            ir::Expr::Function(f) => self.bind_function(f, ctx),
            ir::Expr::Array(items) => {
                if items.is_empty() {
                    return Err(SqlError::new(code::INDETERMINATE_DATATYPE, "cannot determine type of empty array")
                        .with_hint("Explicitly cast to the desired type, for example ARRAY[]::integer[]."));
                }
                let bound = items.iter().map(|i| self.bind_expr(i, ctx)).collect::<SqlResult<Vec<_>>>()?;
                let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
                let t = common_of(&types, "ARRAY")?;
                let items =
                    bound.into_iter().map(|(e, et)| coerce(self, e, &et, &t, CoercionContext::Implicit)).collect::<SqlResult<Vec<_>>>()?;
                // ARRAY[ARRAY[...], ...] builds a multidimensional array of the same type.
                let ty = if matches!(t, Type::Array(_)) { t } else { Type::array_of(t) };
                Ok((fold(Expr::Array(items)), ty))
            }
            ir::Expr::Subscript { .. } => self.bind_subscripts(e, ctx),
            ir::Expr::Row(items) => {
                let bound = items.iter().map(|i| self.bind_expr(i, ctx)).collect::<SqlResult<Vec<_>>>()?;
                let (fields, types): (Vec<Expr>, Vec<Type>) = bound.into_iter().unzip();
                Ok((Expr::Row { fields, types, type_oid: RECORD_OID }, Type::Record))
            }
            ir::Expr::FieldAccess { expr, field } => {
                let (e, t) = self.bind_expr(expr, ctx)?;
                select_field(e, &t, field)
            }
            ir::Expr::Collate { expr, .. } => self.bind_expr(expr, ctx),
            ir::Expr::AtTimeZone { expr, zone } => {
                let (e, t) = self.bind_expr(expr, ctx)?;
                let (z, zt) = self.bind_expr(zone, ctx)?;
                let r = crate::funcs::datetime::resolve_at_time_zone(&t, &zt)?;
                let z = coerce(self, z, &zt, &r.args[0], CoercionContext::Explicit)?;
                let e = coerce(self, e, &t, &r.args[1], CoercionContext::Implicit)?;
                Ok((Expr::Call { func: r.func, args: vec![z, e] }, r.ret))
            }
            ir::Expr::Default => Err(SqlError::syntax("DEFAULT is not allowed in this context")),
        }
    }

    fn bind_bool(&self, e: &ir::Expr, what: &str, ctx: &Ctx) -> SqlResult<Expr> {
        let (x, t) = self.bind_expr(e, ctx)?;
        if t != Type::Bool && t != Type::Unknown {
            return Err(SqlError::datatype_mismatch(format!("argument of {what} must be type boolean, not type {}", t.name())));
        }
        coerce(self, x, &t, &Type::Bool, CoercionContext::Implicit)
    }

    /// Binds a WHERE/HAVING/ON-style predicate.
    pub fn bind_predicate(&self, e: &ir::Expr, what: &str, ctx: &Ctx) -> SqlResult<Expr> {
        let (x, t) = self.bind_expr(e, ctx)?;
        if t != Type::Bool && t != Type::Unknown {
            return Err(SqlError::datatype_mismatch(format!("argument of {what} must be type boolean, not type {}", t.name())));
        }
        coerce(self, x, &t, &Type::Bool, CoercionContext::Implicit)
    }

    fn bind_column(&self, parts: &[String], ctx: &Ctx) -> SqlResult<Bound> {
        let (qualifier, name) = match parts {
            [n] => (None, n.as_str()),
            [q, n] => (Some(q.as_str()), n.as_str()),
            // `schema.table.column`: the table is matched by its name in FROM.
            [_, q, n] => (Some(q.as_str()), n.as_str()),
            _ => return Err(SqlError::syntax(format!("improper qualified name (too many dotted names): {}", parts.join(".")))),
        };
        if name == "*" {
            return Err(SqlError::syntax("row expansion via \"*\" is not supported here"));
        }
        let mut qualifier_seen = false;
        for (depth, level) in ctx.levels.iter().rev().enumerate() {
            let scope = &level.scope;
            let mut found: Option<usize> = None;
            for (i, c) in scope.cols.iter().enumerate() {
                if c.dropped || c.name != name {
                    continue;
                }
                match qualifier {
                    Some(q) => {
                        if c.qualifier.as_deref() != Some(q) {
                            continue;
                        }
                    }
                    None => {
                        if c.hidden_unqualified {
                            continue;
                        }
                    }
                }
                if found.is_some() {
                    return Err(SqlError::new(code::AMBIGUOUS_COLUMN, format!("column reference \"{name}\" is ambiguous")));
                }
                found = Some(i);
            }
            if let Some(q) = qualifier
                && scope.has_qualifier(q)
            {
                qualifier_seen = true;
                if found.is_none() {
                    return Err(SqlError::undefined_column(&format!("{q}.{name}")));
                }
            }
            if let Some(i) = found {
                let t = scope.cols[i].ty.clone();
                if depth == 0 {
                    return Ok((Expr::Col(i), t));
                }
                // Every level between the reference and its target is correlated.
                let n = ctx.levels.len();
                for l in &ctx.levels[n - depth..] {
                    l.correlated.set(true);
                }
                return Ok((Expr::Outer { depth, idx: i }, t));
            }
        }
        // A bare name that is no column may name a FROM item: a whole-row reference.
        if qualifier.is_none() {
            for (depth, level) in ctx.levels.iter().rev().enumerate() {
                if level.scope.has_qualifier(name) {
                    return Ok(self.whole_row(name, depth, ctx));
                }
            }
        }
        match qualifier {
            Some(q) if !qualifier_seen => {
                // `FROM t AS x` hides the name t: Postgres points at the alias.
                for level in ctx.levels.iter().rev() {
                    let aliased = level.scope.cols.iter().find(|c| {
                        c.table_oid != 0
                            && c.qualifier.as_deref() != Some(q)
                            && self.catalog.table_by_oid(c.table_oid).is_some_and(|t| t.name == q)
                    });
                    if let Some(c) = aliased {
                        return Err(SqlError::new(
                            code::UNDEFINED_TABLE,
                            format!("invalid reference to FROM-clause entry for table \"{q}\""),
                        )
                        .with_hint(format!(
                            "Perhaps you meant to reference the table alias \"{}\".",
                            c.qualifier.as_deref().unwrap_or("")
                        )));
                    }
                }
                Err(SqlError::new(code::UNDEFINED_TABLE, format!("missing FROM-clause entry for table \"{q}\"")))
            }
            Some(q) => Err(SqlError::undefined_column(&format!("{q}.{name}"))),
            None => Err(SqlError::undefined_column(&format!("\"{name}\""))),
        }
    }

    /// The row of FROM item `name` as one composite value: the table's row type when the item
    /// is a table, `record` otherwise.
    fn whole_row(&self, name: &str, depth: usize, ctx: &Ctx) -> Bound {
        let n = ctx.levels.len();
        let level = ctx.levels[n - 1 - depth];
        if depth > 0 {
            for l in &ctx.levels[n - depth..] {
                l.correlated.set(true);
            }
        }
        let positions = level.scope.qualified_star(name);
        let cols: Vec<&super::ScopeCol> = positions.iter().map(|&i| &level.scope.cols[i]).collect();
        let fields = positions.iter().map(|&i| if depth == 0 { Expr::Col(i) } else { Expr::Outer { depth, idx: i } }).collect();
        let types = cols.iter().map(|c| c.ty.clone()).collect();
        let table = cols.first().map(|c| c.table_oid).filter(|&t| t != 0 && cols.iter().all(|c| c.table_oid == t));
        let row_type = table.and_then(|t| self.catalog.table_by_oid(t)).filter(|t| t.live_columns().count() == cols.len());
        match row_type {
            Some(t) => {
                let oid = crate::catalog_views::rowtype_oid_of(t.oid);
                (Expr::Row { fields, types, type_oid: oid }, Type::Composite(oid))
            }
            None => {
                // A subquery's or function's row keeps its column names (a blessed record).
                let named = cols.iter().map(|c| pgmem_types::usertype::Field { name: c.name.clone(), ty: c.ty.clone(), typmod: c.typmod });
                let oid = pgmem_types::usertype::bless(named.collect());
                (Expr::Row { fields, types, type_oid: oid }, Type::Record)
            }
        }
    }

    fn bind_binary(&self, op: &str, left: &ir::Expr, right: &ir::Expr, ctx: &Ctx) -> SqlResult<Bound> {
        if let (ir::Expr::Row(l), ir::Expr::Row(r)) = (left, right) {
            return self.bind_row_compare(op, l, r, ctx);
        }
        let (l, lt) = self.bind_expr(left, ctx)?;
        let (r, rt) = self.bind_expr(right, ctx)?;
        self.apply_binary(op, l, lt, r, rt)
    }

    pub fn apply_binary(&self, op: &str, l: Expr, lt: Type, r: Expr, rt: Type) -> SqlResult<Bound> {
        let res = resolve_binary(op, &lt, &rt)?;
        let l = coerce(self, l, &lt, &res.args[0], CoercionContext::Explicit)?;
        let r = coerce(self, r, &rt, &res.args[1], CoercionContext::Explicit)?;
        Ok((fold(Expr::Call { func: res.func, args: vec![l, r] }), res.ret))
    }

    fn bind_row_compare(&self, op: &str, l: &[ir::Expr], r: &[ir::Expr], ctx: &Ctx) -> SqlResult<Bound> {
        if l.len() != r.len() {
            return Err(SqlError::syntax("unequal number of entries in row expressions"));
        }
        let mut parts = Vec::new();
        for (a, b) in l.iter().zip(r) {
            let (e, _) = self.bind_binary(if op == "<>" { "<>" } else { "=" }, a, b, ctx)?;
            parts.push(e);
        }
        match op {
            "=" => Ok((Expr::And(parts), Type::Bool)),
            "<>" | "!=" => Ok((Expr::Or(parts), Type::Bool)),
            _ => {
                // Lexicographic: (a1 op b1) OR (a1 = b1 AND a2 op b2) ...
                let strict_op = match op {
                    "<=" => "<",
                    ">=" => ">",
                    other => other,
                };
                let mut alternatives = Vec::new();
                for i in 0..l.len() {
                    let mut conj = Vec::new();
                    for j in 0..i {
                        conj.push(self.bind_binary("=", &l[j], &r[j], ctx)?.0);
                    }
                    let last_op = if i == l.len() - 1 { op } else { strict_op };
                    conj.push(self.bind_binary(last_op, &l[i], &r[i], ctx)?.0);
                    alternatives.push(Expr::And(conj));
                }
                Ok((Expr::Or(alternatives), Type::Bool))
            }
        }
    }

    fn bind_in_list(&self, expr: &ir::Expr, list: &[ir::Expr], negated: bool, ctx: &Ctx) -> SqlResult<Bound> {
        if let ir::Expr::Row(lhs) = expr {
            let mut alts = Vec::new();
            for item in list {
                let ir::Expr::Row(rhs) = item else { return Err(SqlError::syntax("IN list items must be row expressions")) };
                alts.push(self.bind_row_compare("=", lhs, rhs, ctx)?.0);
            }
            let e = Expr::Or(alts);
            return Ok((if negated { Expr::Not(Box::new(e)) } else { e }, Type::Bool));
        }
        let (probe, pt) = self.bind_expr(expr, ctx)?;
        let items = list.iter().map(|i| self.bind_expr(i, ctx)).collect::<SqlResult<Vec<_>>>()?;
        let mut types = vec![pt.clone()];
        types.extend(items.iter().map(|(_, t)| t.clone()));
        let t = common_of(&types, "IN").map_err(|_| {
            funcs::no_operator(
                "=",
                Some(&pt),
                &items.iter().map(|(_, t)| t.clone()).find(|t| common_type(&pt, t).is_none()).unwrap_or(Type::Unknown),
            )
        })?;
        let probe = coerce(self, probe, &pt, &t, CoercionContext::Implicit)?;
        let list = items.into_iter().map(|(e, et)| coerce(self, e, &et, &t, CoercionContext::Implicit)).collect::<SqlResult<Vec<_>>>()?;
        let cmp = funcs::comparison_for("=", &t).unwrap_or(EQ);
        Ok((Expr::InList { probe: Box::new(probe), list, cmp, negated }, Type::Bool))
    }

    fn bind_any_all(&self, left: &ir::Expr, op: &str, right: &ir::AnyAllRight, all: bool, ctx: &Ctx) -> SqlResult<Bound> {
        let Some(cmp) = comparison(op) else { return self.bind_any_all_operator(left, op, right, all, ctx) };
        let (l, lt) = self.bind_expr(left, ctx)?;
        match right {
            ir::AnyAllRight::Array(arr) => {
                let (a, at) = self.bind_expr(arr, ctx)?;
                // `x = ANY(int2vector)` treats the vector as its element array.
                let (a, at) = match at {
                    Type::Int2Vector | Type::OidVector => {
                        let arr_t = Type::array_of(at.element().cloned().unwrap_or(Type::Int2));
                        (a, arr_t)
                    }
                    other => (a, other),
                };
                let elem = match &at {
                    Type::Array(e) => (**e).clone(),
                    Type::Unknown => {
                        if lt == Type::Unknown {
                            Type::Text
                        } else {
                            lt.clone()
                        }
                    }
                    other => {
                        return Err(SqlError::datatype_mismatch(format!(
                            "op ANY/ALL (array) requires array on right side, not {}",
                            other.name()
                        )));
                    }
                };
                let t = common_type(&lt, &elem).ok_or_else(|| funcs::no_operator(op, Some(&lt), &elem))?;
                let cmp = funcs::comparison_for(op, &t).unwrap_or(cmp);
                let l = coerce(self, l, &lt, &t, CoercionContext::Implicit)?;
                let a = coerce(self, a, &at, &Type::array_of(t), CoercionContext::Implicit)?;
                Ok((Expr::AnyAll { left: Box::new(l), array: Box::new(a), cmp, all }, Type::Bool))
            }
            ir::AnyAllRight::Subquery(q) => {
                let (plan, ct, correlated) = self.bind_subquery_column(q, ctx)?;
                let t = common_type(&lt, &ct).ok_or_else(|| funcs::no_operator(op, Some(&lt), &ct))?;
                let l = coerce(self, l, &lt, &t, CoercionContext::Implicit)?;
                let plan = cast_plan_column(plan, &ct, &t);
                Ok((
                    Expr::Subquery {
                        id: self.fresh_id(),
                        plan: Box::new(plan),
                        kind: SubqueryKind::Quantified { probe: Box::new(l), cmp, all },
                        correlated,
                    },
                    Type::Bool,
                ))
            }
        }
    }

    fn bind_text_call(
        &self,
        func: crate::plan::Func,
        expr: &ir::Expr,
        pattern: &ir::Expr,
        escape: Option<&ir::Expr>,
        opname: &str,
        ctx: &Ctx,
    ) -> SqlResult<Expr> {
        let (e, et) = self.bind_expr(expr, ctx)?;
        let (p, pt) = self.bind_expr(pattern, ctx)?;
        if !(et == Type::Unknown || funcs::implicit_ok(&et, &Type::Text))
            || !(pt == Type::Unknown || funcs::implicit_ok(&pt, &Type::Text))
            || matches!(
                et,
                Type::Int2
                    | Type::Int4
                    | Type::Int8
                    | Type::Numeric
                    | Type::Float4
                    | Type::Float8
                    | Type::Bool
                    | Type::Uuid
                    | Type::Jsonb
                    | Type::Timestamptz
                    | Type::Timestamp
                    | Type::Date
            )
        {
            return Err(funcs::no_operator(opname, Some(&et), &pt));
        }
        let mut args = vec![
            coerce(self, e, &et, &Type::Text, CoercionContext::Implicit)?,
            coerce(self, p, &pt, &Type::Text, CoercionContext::Implicit)?,
        ];
        if let Some(esc) = escape {
            let (x, xt) = self.bind_expr(esc, ctx)?;
            args.push(coerce(self, x, &xt, &Type::Text, CoercionContext::Implicit)?);
        }
        Ok(Expr::Call { func, args })
    }

    fn bind_case(
        &self,
        operand: Option<&ir::Expr>,
        whens: &[(ir::Expr, ir::Expr)],
        else_result: Option<&ir::Expr>,
        ctx: &Ctx,
    ) -> SqlResult<Bound> {
        let mut conds = Vec::with_capacity(whens.len());
        let mut results = Vec::with_capacity(whens.len() + 1);
        for (w, t) in whens {
            let cond = match operand {
                Some(op) => self.bind_binary("=", op, w, ctx)?.0,
                None => self.bind_bool(w, "CASE/WHEN", ctx)?,
            };
            conds.push(cond);
            results.push(self.bind_expr(t, ctx)?);
        }
        let else_bound = else_result.map(|e| self.bind_expr(e, ctx)).transpose()?;
        // Postgres considers the ELSE result first (it heads transformCaseExpr's list).
        let mut types: Vec<Type> = else_bound.iter().map(|(_, t)| t.clone()).collect();
        types.extend(results.iter().map(|(_, t)| t.clone()));
        let t = common_of(&types, "CASE")?;
        let mut out_whens = Vec::new();
        for (c, (r, rt)) in conds.into_iter().zip(results) {
            out_whens.push((c, coerce(self, r, &rt, &t, CoercionContext::Implicit)?));
        }
        let else_expr = else_bound.map(|(e, et)| coerce(self, e, &et, &t, CoercionContext::Implicit)).transpose()?;
        Ok((Expr::Case { operand: None, whens: out_whens, else_result: else_expr.map(Box::new) }, t))
    }

    fn bind_cast(&self, expr: &ir::Expr, ty: &ir::TypeName, ctx: &Ctx) -> SqlResult<Bound> {
        let (t, typmod) = resolve_type_name(self.catalog, ty)?;
        if let ir::Expr::Array(items) = expr
            && items.is_empty()
            && matches!(t, Type::Array(_))
        {
            return Ok((Expr::Const(Value::Array(Vec::new())), t));
        }
        let (e, et) = self.bind_expr(expr, ctx)?;
        if et != Type::Unknown && cast_context(&et, &t).is_none() {
            return Err(SqlError::cannot_coerce(&et.name(), &t.name()));
        }
        let out = coerce_inner(self, e, &et, &t, typmod, CoercionContext::Explicit, true)?;
        // A cast always produces the target type even when no conversion was needed.
        Ok((out, t))
    }

    fn bind_function(&self, f: &ir::FunctionCall, ctx: &Ctx) -> SqlResult<Bound> {
        let name = match f.name.0.as_slice() {
            [n] => n.as_str(),
            [s, n] if s == "pg_catalog" => n.as_str(),
            // Another schema: only user functions live there.
            _ => {
                let bound = f.args.iter().map(|a| self.bind_expr(a, ctx)).collect::<SqlResult<Vec<_>>>()?;
                let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
                return self.bind_user_call(f, bound, ctx)?.ok_or_else(|| funcs::no_function(&f.name.to_string(), &types));
            }
        };
        if name == "grouping" && f.name.0.len() == 1 {
            // Resolved against the GROUP BY when the query is aggregated.
            if matches!(ctx.clause, Clause::Where | Clause::GroupBy | Clause::JoinOn) {
                return Err(SqlError::new(code::GROUPING_ERROR, format!("grouping operations are not allowed in {}", ctx.clause.name())));
            }
            let args = f.args.iter().map(|a| Ok(self.bind_expr(a, ctx)?.0)).collect::<SqlResult<Vec<_>>>()?;
            return Ok((Expr::Grouping { args, col: usize::MAX, values: Vec::new() }, Type::Int4));
        }
        if f.over.is_none() && !f.within_group.is_empty() {
            let hyp = match name {
                "rank" => Some(AggKind::HypRank),
                "dense_rank" => Some(AggKind::HypDenseRank),
                "percent_rank" => Some(AggKind::HypPercentRank),
                "cume_dist" => Some(AggKind::HypCumeDist),
                _ => None,
            };
            if let Some(kind) = hyp {
                return self.bind_hypothetical(name, kind, f, ctx);
            }
        }
        if let Some(kind) = window_kind(name) {
            return self.bind_window_call(name, kind, None, f, ctx);
        }
        if let Some(mut kind) = aggregate_kind(name) {
            if f.star {
                if kind != AggKind::Count {
                    return Err(SqlError::syntax(format!("{name}(*) specified, but {name} is not an aggregate function that takes *")));
                }
                kind = AggKind::CountStar;
            }
            if f.over.is_some() {
                return self.bind_window_call(name, WinKind::Agg(kind), Some(kind), f, ctx);
            }
            return self.bind_aggregate(name, kind, f, ctx);
        }
        if f.over.is_some() {
            return Err(SqlError::new(
                code::WRONG_OBJECT_TYPE,
                format!("OVER specified, but {name} is not a window function nor an aggregate function"),
            ));
        }
        if f.distinct || f.filter.is_some() || !f.order_by.is_empty() {
            return Err(SqlError::new(
                code::WRONG_OBJECT_TYPE,
                format!("DISTINCT/FILTER/ORDER BY specified, but {name} is not an aggregate function"),
            ));
        }
        if f.star {
            return Err(SqlError::syntax(format!("{name}(*) specified, but {name} is not an aggregate function")));
        }
        let mut bound = f.args.iter().map(|a| self.bind_expr(a, ctx)).collect::<SqlResult<Vec<_>>>()?;
        // `f(VARIADIC arr)`: the array's elements become the arguments (an array known when the
        // statement is bound: an ARRAY[...] constructor or a constant).
        if f.variadic
            && let Some((last, lt)) = bound.pop()
        {
            let Type::Array(elem) = &lt else {
                return Err(SqlError::new(code::DATATYPE_MISMATCH, "VARIADIC argument must be an array"));
            };
            match fold(last) {
                Expr::Array(items) => bound.extend(items.into_iter().map(|e| (e, (**elem).clone()))),
                Expr::Const(Value::Array(items)) => {
                    bound.extend(items.into_iter().map(|v| (Expr::Const(v), (**elem).clone())));
                }
                Expr::Const(Value::Null) => {
                    return Ok((
                        Expr::Const(Value::Null),
                        if name == "num_nulls" || name == "num_nonnulls" { Type::Int4 } else { Type::Text },
                    ));
                }
                _ => return Err(SqlError::feature_not_supported("VARIADIC with a non-constant array is not supported by pgmem")),
            }
        }
        // SQL/JSON query functions also get their RETURNING type's OID (the 4th argument's
        // type), to coerce results at run time under ON ERROR.
        if matches!(name, "__pgmem_json_value" | "__pgmem_json_query" | "__pgmem_json_exists")
            && let Some((_, t)) = bound.get(3)
        {
            let oid = t.oid();
            bound.push((Expr::Const(Value::Oid(oid)), Type::Oid));
        }
        match name {
            "coalesce" | "greatest" | "least" => {
                if bound.is_empty() {
                    return Err(SqlError::syntax(format!("syntax error: {name}() requires arguments")));
                }
                let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
                let t = common_of(&types, &name.to_ascii_uppercase())?;
                let args =
                    bound.into_iter().map(|(e, et)| coerce(self, e, &et, &t, CoercionContext::Implicit)).collect::<SqlResult<Vec<_>>>()?;
                let e = match name {
                    "coalesce" => Expr::Coalesce(args),
                    "greatest" => Expr::MinMax { args, greatest: true },
                    _ => Expr::MinMax { args, greatest: false },
                };
                return Ok((e, t));
            }
            "nullif" => {
                let [(a, at), (b, bt)] = <[Bound; 2]>::try_from(bound).map_err(|_| funcs::no_function(name, &[]))?;
                let t = common_type(&at, &bt).ok_or_else(|| funcs::no_operator("=", Some(&at), &bt))?;
                let t = if t == Type::Unknown { Type::Text } else { t };
                let a = coerce(self, a, &at, &t, CoercionContext::Implicit)?;
                let b = coerce(self, b, &bt, &t, CoercionContext::Implicit)?;
                return Ok((Expr::NullIf(Box::new(a), Box::new(b)), t));
            }
            "pg_typeof" if bound.len() == 1 => {
                // Keep the argument in the tree (it may hold an aggregate) but return its type.
                let (arg, t) = bound.into_iter().next().unwrap_or((Expr::null(), Type::Unknown));
                let v = Value::Reg(pgmem_types::RegKind::Type, t.oid(), t.name());
                let func = funcs::lenient("pg_typeof", |a, _| Ok(a[1].clone()));
                return Ok((Expr::Call { func, args: vec![arg, Expr::Const(v)] }, Type::Reg(pgmem_types::RegKind::Type)));
            }
            _ => {}
        }
        if let ("enum_first" | "enum_last" | "enum_range", Some(Type::Enum(oid))) =
            (name, bound.iter().map(|(_, t)| t).find(|t| **t != Type::Unknown))
        {
            return self.bind_enum_function(name, *oid, bound);
        }
        let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
        if let Some((outs, targets, f)) = select_list_srf(name, &types)? {
            return self.bind_srf_call(name, outs, targets, f, bound, ctx);
        }
        let res = match resolve_function(name, &types) {
            Ok(Some(r)) => r,
            builtin => match self.bind_user_call(f, bound.clone(), ctx)? {
                Some(b) => return Ok(b),
                None => builtin?.ok_or_else(|| funcs::no_function(name, &types))?,
            },
        };
        let args = bound
            .into_iter()
            .zip(&res.args)
            .map(|((e, et), target)| coerce(self, e, &et, target, CoercionContext::Explicit))
            .collect::<SqlResult<Vec<_>>>()?;
        Ok((Expr::Call { func: res.func, args }, res.ret))
    }

    /// A call of a user function: its arguments coerced, omitted ones defaulted, the
    /// function's OID first. None when no user function has the name.
    pub(crate) fn bind_user_call(&self, f: &ir::FunctionCall, bound: Vec<Bound>, ctx: &Ctx) -> SqlResult<Option<Bound>> {
        let Some((def, args)) = self.user_call_args(f, bound, ctx)? else { return Ok(None) };
        if def.returns_set() {
            return Err(SqlError::feature_not_supported(format!(
                "pgmem supports set-returning user functions only in FROM ({} called in the select list)",
                def.name
            )));
        }
        let ret = match &def.ret {
            pgmem_catalog::FuncReturn::Scalar(Type::Unknown) => {
                return Err(SqlError::feature_not_supported("trigger functions can only be called as triggers"));
            }
            pgmem_catalog::FuncReturn::Scalar(t) => t.clone(),
            _ => Type::Record,
        };
        let func = funcs::lenient("user function", crate::functions::call_scalar);
        Ok(Some((Expr::Call { func, args }, ret)))
    }

    /// Resolves a user function call: the function and its argument expressions (OID first).
    pub(crate) fn user_call_args(
        &self,
        f: &ir::FunctionCall,
        bound: Vec<Bound>,
        ctx: &Ctx,
    ) -> SqlResult<Option<(std::sync::Arc<pgmem_catalog::FunctionDef>, Vec<Expr>)>> {
        let cands = self.catalog.find_functions(&f.name);
        if cands.is_empty() {
            return Ok(None);
        }
        let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
        let Some(call) = crate::functions::resolve(f.name.base(), &cands, &types, false)? else {
            if crate::functions::resolve(f.name.base(), &cands, &types, true)?.is_some() {
                return Err(SqlError::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("{} is a procedure", crate::functions::signature(f.name.base(), &types)),
                )
                .with_hint("To call a procedure, use CALL."));
            }
            return Ok(None);
        };
        let mut args = vec![Expr::Const(Value::Oid(call.def.oid))];
        for ((e, et), target) in bound.into_iter().zip(&call.targets) {
            args.push(coerce(self, e, &et, target, CoercionContext::Implicit)?);
        }
        // Defaults are bound on their own (they cannot see the call's scope).
        let _ = ctx;
        let empty = Level::new(super::Scope::default());
        let dctx = Ctx::new(&empty);
        for a in call.def.args.iter().filter(|a| a.mode.is_input()).skip(args.len() - 1) {
            let d = a.default.as_ref().ok_or_else(|| SqlError::internal("argument without default"))?;
            let (e, t) = self.bind_expr(d, &dctx)?;
            args.push(coerce(self, e, &t, &a.ty, CoercionContext::Assignment)?);
        }
        Ok(Some((call.def, args)))
    }

    /// `enum_first(anyenum)`, `enum_last(anyenum)`, `enum_range(anyenum [, anyenum])`: only the
    /// argument types matter for the one-argument forms; the labels are read when the
    /// statement runs.
    fn bind_enum_function(&self, name: &str, oid: u32, bound: Vec<Bound>) -> SqlResult<Bound> {
        let ty = Type::Enum(oid);
        let arity_ok = matches!((name, bound.len()), ("enum_first" | "enum_last", 1) | ("enum_range", 1 | 2));
        if !arity_ok || bound.iter().any(|(_, t)| *t != Type::Unknown && *t != ty) {
            let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
            return Err(funcs::no_function(name, &types));
        }
        let mut args =
            bound.into_iter().map(|(e, t)| coerce(self, e, &t, &ty, CoercionContext::Implicit)).collect::<SqlResult<Vec<_>>>()?;
        args.insert(0, Expr::Const(Value::Oid(oid)));
        let (func, ret) = match name {
            "enum_first" => (funcs::lenient("enum_first", funcs::misc::enum_first), ty),
            "enum_last" => (funcs::lenient("enum_last", funcs::misc::enum_last), ty),
            _ => (funcs::lenient("enum_range", funcs::misc::enum_range), Type::array_of(ty)),
        };
        Ok((Expr::Call { func, args }, ret))
    }

    fn bind_agg_args(&self, f: &ir::FunctionCall, ctx: &Ctx) -> SqlResult<Vec<Bound>> {
        // A window aggregate's arguments may hold the query's aggregates (`sum(sum(x)) OVER ()`).
        let window = ctx.clause == Clause::WindowArg;
        let inner = ctx.with_clause(if window { Clause::WindowArg } else { Clause::AggArg });
        let mut out = Vec::new();
        for a in &f.args {
            let b = self.bind_expr(a, &inner)?;
            if !window && contains_agg(&b.0) {
                return Err(SqlError::new(code::GROUPING_ERROR, "aggregate function calls cannot be nested"));
            }
            if !window && super::select_contains_window(&b.0) {
                return Err(SqlError::new(code::GROUPING_ERROR, "aggregate function calls cannot contain window function calls"));
            }
            out.push(b);
        }
        Ok(out)
    }

    fn make_agg(&self, name: &str, kind: AggKind, f: &ir::FunctionCall, ctx: &Ctx) -> SqlResult<AggCall> {
        let ordered_set = matches!(kind, AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode);
        if ordered_set {
            return self.make_ordered_set_agg(name, kind, f, ctx);
        }
        if !f.within_group.is_empty() {
            return Err(SqlError::new(
                code::WRONG_OBJECT_TYPE,
                format!("{name} is not an ordered-set aggregate, so it cannot have WITHIN GROUP"),
            ));
        }
        let bound = self.bind_agg_args(f, ctx)?;
        let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
        let expected_args = match kind {
            AggKind::CountStar => 0,
            AggKind::StringAgg
            | AggKind::JsonbObjectAgg
            | AggKind::JsonObjectAgg
            | AggKind::JsonbObjectAggStrict
            | AggKind::JsonObjectAggStrict
            | AggKind::Corr
            | AggKind::CovarPop
            | AggKind::CovarSamp
            | AggKind::RegrCount
            | AggKind::RegrSxx
            | AggKind::RegrSyy
            | AggKind::RegrSxy
            | AggKind::RegrAvgX
            | AggKind::RegrAvgY
            | AggKind::RegrR2
            | AggKind::RegrSlope
            | AggKind::RegrIntercept => 2,
            _ => 1,
        };
        if bound.len() != expected_args {
            return Err(funcs::no_function(name, &types));
        }
        let (targets, ret) = aggregate_types(kind, name, &types)?;
        let args = bound
            .into_iter()
            .zip(&targets)
            .map(|((e, et), t)| coerce(self, e, &et, t, CoercionContext::Implicit))
            .collect::<SqlResult<Vec<_>>>()?;
        let inner = ctx.with_clause(Clause::Other);
        let filter = f.filter.as_ref().map(|fl| self.bind_predicate(fl, "FILTER", &ctx.with_clause(Clause::Filter))).transpose()?;
        let order_by = f
            .order_by
            .iter()
            .map(|o| {
                let (e, _) = self.bind_expr(&o.expr, &inner)?;
                Ok(SortKey { expr: e, desc: o.desc, nulls_first: o.nulls_first.unwrap_or(o.desc) })
            })
            .collect::<SqlResult<Vec<_>>>()?;
        Ok(AggCall {
            kind,
            args,
            distinct: f.distinct,
            filter,
            order_by,
            arg_ty: targets.first().cloned().unwrap_or(Type::Unknown),
            ty: ret,
        })
    }

    /// `percentile_cont(fraction) WITHIN GROUP (ORDER BY x)` and friends: the sorted input
    /// becomes argument 0 and the direct argument (if any) argument 1.
    fn make_ordered_set_agg(&self, name: &str, kind: AggKind, f: &ir::FunctionCall, ctx: &Ctx) -> SqlResult<AggCall> {
        if f.within_group.is_empty() {
            let types =
                self.bind_agg_args(f, ctx)?.into_iter().map(|(_, t)| if t == Type::Unknown { Type::Text } else { t }).collect::<Vec<_>>();
            return Err(funcs::no_function(name, &types));
        }
        if f.distinct {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, "cannot use DISTINCT with WITHIN GROUP"));
        }
        let inner = ctx.with_clause(Clause::Other);
        let direct = f.args.iter().map(|a| self.bind_expr(a, &inner)).collect::<SqlResult<Vec<_>>>()?;
        let sorted = f.within_group.iter().map(|o| self.bind_expr(&o.expr, &inner)).collect::<SqlResult<Vec<_>>>()?;
        let mut types: Vec<Type> = direct.iter().map(|(_, t)| t.clone()).collect();
        types.extend(sorted.iter().map(|(_, t)| t.clone()));
        let err = || funcs::no_function(name, &types);
        let want_direct = usize::from(kind != AggKind::Mode);
        if direct.len() != want_direct || sorted.len() != 1 {
            return Err(err());
        }
        let (sexpr, st) = sorted.into_iter().next().ok_or_else(err)?;
        let st = if st == Type::Unknown { Type::Text } else { st.base() };
        let sort_target = match kind {
            AggKind::PercentileCont if st == Type::Interval => Type::Interval,
            AggKind::PercentileCont if st.is_numeric() => Type::Float8,
            AggKind::PercentileCont => return Err(err()),
            _ => st,
        };
        let mut args = vec![coerce(self, sexpr, &types[want_direct], &sort_target, CoercionContext::Implicit)?];
        let mut ty = sort_target.clone();
        if let Some((d, dt)) = direct.into_iter().next() {
            let array = matches!(dt, Type::Array(_));
            let target = if array { Type::array_of(Type::Float8) } else { Type::Float8 };
            let elem_ok = match &dt {
                Type::Array(e) => e.is_numeric(),
                t => t.is_numeric() || *t == Type::Unknown,
            };
            if !elem_ok {
                return Err(err());
            }
            args.push(coerce(self, d, &dt, &target, CoercionContext::Implicit)?);
            if array {
                ty = Type::array_of(ty);
            }
        }
        let filter = f.filter.as_ref().map(|fl| self.bind_predicate(fl, "FILTER", &ctx.with_clause(Clause::Filter))).transpose()?;
        let o = &f.within_group[0];
        let order_by = vec![SortKey { expr: args[0].clone(), desc: o.desc, nulls_first: o.nulls_first.unwrap_or(o.desc) }];
        Ok(AggCall { kind, args, distinct: false, filter, order_by, arg_ty: sort_target, ty })
    }

    /// `rank(args) WITHIN GROUP (ORDER BY cols)` and the other hypothetical-set aggregates.
    fn bind_hypothetical(&self, name: &str, kind: AggKind, f: &ir::FunctionCall, ctx: &Ctx) -> SqlResult<Bound> {
        if !ctx.clause.agg_allowed() {
            return Err(agg_misuse(ctx.clause, "aggregate functions"));
        }
        let inner = ctx.with_clause(Clause::Other);
        let direct = f.args.iter().map(|a| self.bind_expr(a, &inner)).collect::<SqlResult<Vec<_>>>()?;
        let sorted = f.within_group.iter().map(|o| self.bind_expr(&o.expr, &inner)).collect::<SqlResult<Vec<_>>>()?;
        if direct.len() != sorted.len() {
            let types: Vec<Type> = direct.iter().chain(&sorted).map(|(_, t)| t.clone()).collect();
            return Err(funcs::no_function(name, &types));
        }
        let mut args = Vec::new();
        let mut order_by = Vec::new();
        let mut targets = Vec::new();
        for ((d, dt), ((e, et), o)) in direct.iter().zip(sorted.into_iter().zip(&f.within_group)) {
            let t = common_of(&[dt.clone(), et.clone()], "WITHIN GROUP")?;
            let e = coerce(self, e, &et, &t, CoercionContext::Implicit)?;
            order_by.push(SortKey { expr: e.clone(), desc: o.desc, nulls_first: o.nulls_first.unwrap_or(o.desc) });
            args.push(e);
            targets.push((d.clone(), dt.clone(), t));
        }
        for (d, dt, t) in targets {
            args.push(coerce(self, d, &dt, &t, CoercionContext::Implicit)?);
        }
        let filter = f.filter.as_ref().map(|fl| self.bind_predicate(fl, "FILTER", &ctx.with_clause(Clause::Filter))).transpose()?;
        let ty = if matches!(kind, AggKind::HypRank | AggKind::HypDenseRank) { Type::Int8 } else { Type::Float8 };
        let call = AggCall { kind, args, distinct: false, filter, order_by, arg_ty: Type::Unknown, ty: ty.clone() };
        ctx.current().aggs.borrow_mut().push(call.clone());
        Ok((Expr::AggCall(Box::new(call)), ty))
    }

    fn bind_aggregate(&self, name: &str, kind: AggKind, f: &ir::FunctionCall, ctx: &Ctx) -> SqlResult<Bound> {
        if !ctx.clause.agg_allowed() {
            if matches!(ctx.clause, Clause::Other | Clause::AggArg) {
                return Err(SqlError::new(code::GROUPING_ERROR, "aggregate function calls cannot be nested"));
            }
            return Err(agg_misuse(ctx.clause, "aggregate functions"));
        }
        let call = self.make_agg(name, kind, f, ctx)?;
        let ty = call.ty.clone();
        ctx.current().aggs.borrow_mut().push(call.clone());
        Ok((Expr::AggCall(Box::new(call)), ty))
    }

    fn bind_window_call(&self, name: &str, kind: WinKind, agg: Option<AggKind>, f: &ir::FunctionCall, ctx: &Ctx) -> SqlResult<Bound> {
        let Some(over) = &f.over else {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("window function {name} requires an OVER clause")));
        };
        if !ctx.clause.window_allowed() {
            return Err(window_misuse(ctx.clause));
        }
        let spec = resolve_window_spec(over, &ctx.windows)?;
        let inner = ctx.with_clause(Clause::WindowArg);
        let agg_call = match agg {
            Some(k) => Some(self.make_agg(name, k, f, &inner)?),
            None => None,
        };
        let (args, ty) = match &agg_call {
            Some(a) => (a.args.clone(), a.ty.clone()),
            None => {
                let bound = f.args.iter().map(|a| self.bind_expr(a, &inner)).collect::<SqlResult<Vec<_>>>()?;
                let ty = match kind {
                    WinKind::RowNumber | WinKind::Rank | WinKind::DenseRank => Type::Int8,
                    WinKind::PercentRank | WinKind::CumeDist => Type::Float8,
                    WinKind::Ntile => Type::Int4,
                    _ => bound
                        .first()
                        .map(|(_, t)| if *t == Type::Unknown { Type::Text } else { t.clone() })
                        .ok_or_else(|| funcs::no_function(name, &[]))?,
                };
                let mut args = Vec::new();
                for (i, (e, et)) in bound.into_iter().enumerate() {
                    let target = match (kind, i) {
                        (WinKind::Ntile, 0) | (WinKind::Lag | WinKind::Lead, 1) | (WinKind::NthValue, 1) => Type::Int4,
                        (WinKind::Lag | WinKind::Lead, 2) => ty.clone(),
                        _ => {
                            if et == Type::Unknown {
                                Type::Text
                            } else {
                                et.clone()
                            }
                        }
                    };
                    args.push(coerce(self, e, &et, &target, CoercionContext::Implicit)?);
                }
                (args, ty)
            }
        };
        let partition_by = spec.partition_by.iter().map(|p| Ok(self.bind_expr(p, &inner)?.0)).collect::<SqlResult<Vec<_>>>()?;
        let order_by = spec
            .order_by
            .iter()
            .map(|o| Ok(SortKey { expr: self.bind_expr(&o.expr, &inner)?.0, desc: o.desc, nulls_first: o.nulls_first.unwrap_or(o.desc) }))
            .collect::<SqlResult<Vec<_>>>()?;
        let frame = match &spec.frame {
            None => Frame::default_frame(),
            Some(fr) => self.bind_frame(fr, &spec.order_by, &order_by, &ctx.with_clause(Clause::Other))?,
        };
        let call = WinCall { kind, args, partition_by, order_by, frame, ty: ty.clone(), agg: agg_call };
        ctx.current().windows.borrow_mut().push(call.clone());
        Ok((Expr::WindowCall(Box::new(call)), ty))
    }

    /// A window frame. ROWS / GROUPS offsets are counts; a RANGE offset becomes the bound
    /// value `key - offset` or `key + offset` (by direction and sort order), which needs
    /// exactly one ORDER BY key of a type with `in_range` support.
    fn bind_frame(&self, fr: &ir::WindowFrame, order_ir: &[ir::OrderByExpr], order_by: &[SortKey], ctx: &Ctx) -> SqlResult<Frame> {
        let units = match fr.units {
            ir::FrameUnits::Rows => FrameUnits::Rows,
            ir::FrameUnits::Range => FrameUnits::Range,
            ir::FrameUnits::Groups => FrameUnits::Groups,
        };
        let mut range_offsets = Vec::new();
        let mut range_key: Option<(Expr, Type)> = None;
        let mut bound = |b: &ir::FrameBound| -> SqlResult<FrameBound> {
            let (e, preceding) = match b {
                ir::FrameBound::UnboundedPreceding => return Ok(FrameBound::UnboundedPreceding),
                ir::FrameBound::CurrentRow => return Ok(FrameBound::CurrentRow),
                ir::FrameBound::UnboundedFollowing => return Ok(FrameBound::UnboundedFollowing),
                ir::FrameBound::Preceding(e) => (e, true),
                ir::FrameBound::Following(e) => (e, false),
            };
            if units != FrameUnits::Range {
                let (x, t) = self.bind_expr(e, ctx)?;
                let x = coerce(self, x, &t, &Type::Int8, CoercionContext::Implicit)?;
                return Ok(if preceding { FrameBound::Preceding(x) } else { FrameBound::Following(x) });
            }
            let [key] = order_ir else {
                return Err(SqlError::new(
                    code::WINDOWING_ERROR,
                    "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column",
                ));
            };
            let (_, kt) = self.bind_expr(&key.expr, ctx)?;
            let supported = kt.is_numeric()
                || matches!(kt, Type::Date | Type::Time | Type::TimeTz | Type::Timestamp | Type::Timestamptz | Type::Interval);
            if !supported {
                return Err(SqlError::feature_not_supported(format!(
                    "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {}",
                    kt.name()
                )));
            }
            range_offsets.push(self.bind_expr(e, ctx)?.0);
            let minus = preceding != order_by[0].desc;
            let value = ir::Expr::Binary { op: if minus { "-" } else { "+" }.into(), left: Box::new(key.expr.clone()), right: e.clone() };
            let (x, xt) = self.bind_expr(&value, ctx)?;
            // Compare in the bound's type (`date - interval` is a timestamp).
            let (k, kt) = self.bind_expr(&key.expr, ctx)?;
            let target = common_type(&kt, &xt).unwrap_or(xt.clone());
            let x = coerce(self, x, &xt, &target, CoercionContext::Implicit)?;
            if range_key.is_none() {
                range_key = Some((coerce(self, k, &kt, &target, CoercionContext::Implicit)?, target));
            }
            Ok(if preceding { FrameBound::Preceding(x) } else { FrameBound::Following(x) })
        };
        let start = bound(&fr.start)?;
        let end = bound(&fr.end)?;
        Ok(Frame { units, start, end, exclude: fr.exclude, range_offsets, range_key: range_key.map(|(k, _)| k) })
    }

    /// Binds a subquery and returns its plan and whether it references enclosing rows.
    /// A subquery yielding one row as a record (`SET (a, b) = (SELECT ...)`), with its column types.
    pub fn bind_row_subquery(&self, q: &ir::Query, ctx: &Ctx) -> SqlResult<(Expr, Vec<Type>)> {
        let level = Level::new(super::Scope::default());
        let nested = ctx.nested(&level);
        let bound = self.bind_query(q, &nested)?;
        let types = bound.columns.iter().map(|c| c.ty.clone()).collect();
        let e =
            Expr::Subquery { id: self.fresh_id(), plan: Box::new(bound.rel), kind: SubqueryKind::Row, correlated: level.correlated.get() };
        Ok((e, types))
    }

    pub fn bind_subquery_plan(&self, q: &ir::Query, ctx: &Ctx) -> SqlResult<(crate::plan::Rel, bool)> {
        let level = Level::new(super::Scope::default());
        let nested = ctx.nested(&level);
        let bound = self.bind_query(q, &nested)?;
        Ok((bound.rel, level.correlated.get()))
    }

    /// `x op ANY/ALL (...)` for a boolean operator other than a comparison (`~~`, `~`, `@>`...):
    /// the operator is resolved on the left type and the element type.
    fn bind_any_all_operator(&self, left: &ir::Expr, op: &str, right: &ir::AnyAllRight, all: bool, ctx: &Ctx) -> SqlResult<Bound> {
        let (l, lt) = self.bind_expr(left, ctx)?;
        let check = |res: &funcs::Resolved| {
            if res.ret == Type::Bool {
                Ok(())
            } else {
                Err(SqlError::datatype_mismatch("op ANY/ALL (array) requires operator to yield boolean"))
            }
        };
        match right {
            ir::AnyAllRight::Array(arr) => {
                let (a, at) = self.bind_expr(arr, ctx)?;
                let elem = match &at {
                    Type::Array(e) => (**e).clone(),
                    Type::Unknown => Type::Unknown,
                    other => {
                        return Err(SqlError::datatype_mismatch(format!(
                            "op ANY/ALL (array) requires array on right side, not {}",
                            other.name()
                        )));
                    }
                };
                let res = funcs::resolve_binary(op, &lt, &elem)?;
                check(&res)?;
                let l = coerce(self, l, &lt, &res.args[0], CoercionContext::Implicit)?;
                let a = coerce(self, a, &at, &Type::array_of(res.args[1].clone()), CoercionContext::Implicit)?;
                Ok((Expr::AnyAll { left: Box::new(l), array: Box::new(a), cmp: res.func, all }, Type::Bool))
            }
            ir::AnyAllRight::Subquery(q) => {
                let (plan, ct, correlated) = self.bind_subquery_column(q, ctx)?;
                let res = funcs::resolve_binary(op, &lt, &ct)?;
                check(&res)?;
                let l = coerce(self, l, &lt, &res.args[0], CoercionContext::Implicit)?;
                let plan = cast_plan_column(plan, &ct, &res.args[1]);
                let kind = SubqueryKind::Quantified { probe: Box::new(l), cmp: res.func, all };
                Ok((Expr::Subquery { id: self.fresh_id(), plan: Box::new(plan), kind, correlated }, Type::Bool))
            }
        }
    }

    /// A chain of subscripts (`x[1][2:3]`): jsonb subscripts are `->` steps, array subscripts
    /// address one dimension each (int2vector / oidvector start at 0).
    fn bind_subscripts(&self, e: &ir::Expr, ctx: &Ctx) -> SqlResult<Bound> {
        let mut chain = Vec::new();
        let mut cur = e;
        while let ir::Expr::Subscript { expr, index } = cur {
            chain.push(index);
            cur = expr;
        }
        chain.reverse();
        let (mut base, t) = self.bind_expr(cur, ctx)?;
        if t == Type::Jsonb {
            for index in chain {
                let ir::Subscript::Index(i) = index else {
                    return Err(SqlError::datatype_mismatch("jsonb subscript does not support slices"));
                };
                let (k, kt) = self.bind_expr(i, ctx)?;
                let kt2 = if kt.is_integer() {
                    Type::Int4
                } else if kt == Type::Unknown || kt.is_string() {
                    Type::Text
                } else {
                    return Err(SqlError::datatype_mismatch(format!("subscript type {} is not supported", kt.name()))
                        .with_hint("jsonb subscript must be coercible to either integer or text."));
                };
                let k = coerce(self, k, &kt, &kt2, CoercionContext::Implicit)?;
                let func = funcs::lenient("jsonb_subscript", |a, _| crate::funcs::json::subscript(&a[0], &a[1]));
                base = Expr::Call { func, args: vec![base, k] };
            }
            return Ok((base, Type::Jsonb));
        }
        let zero_based = matches!(t, Type::Int2Vector | Type::OidVector);
        let elem = match &t {
            Type::Array(e) => (**e).clone(),
            Type::Int2Vector | Type::OidVector => t.element().cloned().unwrap_or(Type::Int2),
            other => {
                return Err(SqlError::datatype_mismatch(format!(
                    "cannot subscript type {} because it does not support subscripting",
                    other.name()
                )));
            }
        };
        if chain.len() > 6 {
            return Err(SqlError::new(
                code::PROGRAM_LIMIT_EXCEEDED,
                format!("number of array dimensions ({}) exceeds the maximum allowed (6)", chain.len()),
            ));
        }
        let int_expr = |e: &ir::Expr| -> SqlResult<Expr> {
            let (x, xt) = self.bind_expr(e, ctx)?;
            let x = coerce(self, x, &xt, &Type::Int4, CoercionContext::Assignment)?;
            if zero_based {
                return Ok(self.apply_binary("+", x, Type::Int4, Expr::Const(Value::Int4(1)), Type::Int4)?.0);
            }
            Ok(x)
        };
        let slice = chain.iter().any(|s| matches!(s, ir::Subscript::Slice { .. }));
        let mut subs = Vec::with_capacity(chain.len());
        for index in chain {
            subs.push(match index {
                ir::Subscript::Index(i) => crate::plan::SubscriptPart { lower: Some(int_expr(i)?), upper: None, range: false },
                ir::Subscript::Slice { lower, upper } => crate::plan::SubscriptPart {
                    lower: lower.as_ref().map(|l| int_expr(l)).transpose()?,
                    upper: upper.as_ref().map(|u| int_expr(u)).transpose()?,
                    range: true,
                },
            });
        }
        let ty = if slice { t.clone() } else { elem };
        Ok((Expr::Subscript { expr: Box::new(base), subs, slice }, ty))
    }

    /// A set-returning function called outside FROM: allowed in the select list (it becomes a
    /// ProjectSet column), an error elsewhere.
    fn bind_srf_call(
        &self,
        name: &str,
        outs: Vec<Type>,
        targets: Vec<Type>,
        f: crate::plan::SrfFn,
        bound: Vec<Bound>,
        ctx: &Ctx,
    ) -> SqlResult<Bound> {
        if ctx.clause != Clause::SelectList {
            let what = match ctx.clause {
                Clause::Where => "WHERE",
                Clause::JoinOn => "JOIN conditions",
                Clause::GroupBy => "GROUP BY",
                Clause::Having => "HAVING",
                Clause::Values => "VALUES",
                Clause::Check => "check constraints",
                Clause::Default => "DEFAULT expressions",
                Clause::OrderBy => "ORDER BY",
                Clause::Returning => "RETURNING",
                _ => "this context",
            };
            let _ = name;
            return Err(SqlError::feature_not_supported(format!("set-returning functions are not allowed in {what}")));
        }
        let args = bound
            .into_iter()
            .zip(&targets)
            .map(|((e, et), t)| coerce(self, e, &et, t, CoercionContext::Implicit))
            .collect::<SqlResult<Vec<_>>>()?;
        let ty = if outs.len() == 1 { outs[0].clone() } else { Type::Record };
        let cols = outs.len();
        Ok((Expr::SrfCall { srf: crate::plan::Srf { name: "srf", f }, args, cols }, ty))
    }

    /// `(a, b) IN (SELECT x, y ...)`: compares row values against the subquery's rows.
    fn bind_row_in_subquery(&self, items: &[ir::Expr], q: &ir::Query, negated: bool, ctx: &Ctx) -> SqlResult<Bound> {
        let level = Level::new(super::Scope::default());
        let nested = ctx.nested(&level);
        let bound = self.bind_query(q, &nested)?;
        match bound.columns.len().cmp(&items.len()) {
            std::cmp::Ordering::Greater => return Err(SqlError::syntax("subquery has too many columns")),
            std::cmp::Ordering::Less => return Err(SqlError::syntax("subquery has too few columns")),
            std::cmp::Ordering::Equal => {}
        }
        let mut probe = Vec::new();
        let mut cols = Vec::new();
        let mut types = Vec::new();
        for (i, (item, c)) in items.iter().zip(&bound.columns).enumerate() {
            let (e, t) = self.bind_expr(item, ctx)?;
            let common = common_type(&t, &c.ty).ok_or_else(|| funcs::no_operator("=", Some(&t), &c.ty))?;
            probe.push(coerce(self, e, &t, &common, CoercionContext::Implicit)?);
            cols.push(if c.ty == common {
                Expr::Col(i)
            } else {
                Expr::Cast { expr: Box::new(Expr::Col(i)), to: common.clone(), typmod: -1 }
            });
            types.push(common);
        }
        let plan = crate::plan::Rel::Project {
            input: Box::new(bound.rel),
            exprs: vec![Expr::Row { fields: cols, types: types.clone(), type_oid: RECORD_OID }],
        };
        let sub = Expr::Subquery {
            id: self.fresh_id(),
            plan: Box::new(plan),
            kind: SubqueryKind::Quantified {
                probe: Box::new(Expr::Row { fields: probe, types, type_oid: RECORD_OID }),
                cmp: EQ,
                all: false,
            },
            correlated: level.correlated.get(),
        };
        Ok((if negated { Expr::Not(Box::new(sub)) } else { sub }, Type::Bool))
    }

    fn bind_subquery_column(&self, q: &ir::Query, ctx: &Ctx) -> SqlResult<(crate::plan::Rel, Type, bool)> {
        let level = Level::new(super::Scope::default());
        let nested = ctx.nested(&level);
        let bound = self.bind_query(q, &nested)?;
        if bound.columns.len() != 1 {
            return Err(SqlError::syntax(if bound.columns.len() > 1 {
                "subquery must return only one column"
            } else {
                "subquery has too few columns"
            }));
        }
        Ok((bound.rel, bound.columns[0].ty.clone(), level.correlated.get()))
    }
}

/// `(expr).field` on a bound composite expression.
pub fn select_field(e: Expr, t: &Type, field: &str) -> SqlResult<Bound> {
    match t {
        Type::Composite(oid) => {
            let fields = crate::usertypes::composite_fields(*oid);
            let idx = fields
                .iter()
                .position(|f| f.name == field)
                .ok_or_else(|| SqlError::new(code::UNDEFINED_COLUMN, format!("column \"{field}\" not found in data type {}", t.name())))?;
            let ty = fields[idx].ty.clone();
            Ok((field_of(e, idx), ty))
        }
        // The fields of ROW(...) are f1, f2, ...
        Type::Record => {
            let unknown = || SqlError::new(code::UNDEFINED_COLUMN, format!("could not identify column \"{field}\" in record data type"));
            let Expr::Row { types, .. } = &e else { return Err(unknown()) };
            let n = field
                .strip_prefix('f')
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| (1..=types.len()).contains(n))
                .ok_or_else(unknown)?;
            let ty = types[n - 1].clone();
            Ok((field_of(e, n - 1), if ty == Type::Unknown { Type::Text } else { ty }))
        }
        Type::Domain(_) => select_field(e, &t.base(), field),
        other => Err(SqlError::new(
            code::WRONG_OBJECT_TYPE,
            format!("column notation .{field} applied to type {}, which is not a composite type", other.name()),
        )),
    }
}

/// `(expr).*`: one (expression, name, type) per field of a composite expression.
pub fn expand_fields(e: Expr, t: &Type) -> SqlResult<Vec<(Expr, String, Type)>> {
    let names: Vec<String> = match t {
        Type::Composite(oid) => crate::usertypes::composite_fields(*oid).into_iter().map(|f| f.name).collect(),
        Type::Record => match &e {
            Expr::Row { fields, .. } => (1..=fields.len()).map(|n| format!("f{n}")).collect(),
            _ => return Err(SqlError::new(code::FEATURE_NOT_SUPPORTED, "record type has not been registered")),
        },
        Type::Domain(_) => return expand_fields(e, &t.base()),
        other => return Err(SqlError::syntax(format!("type {} is not composite", other.name()))),
    };
    names
        .into_iter()
        .map(|n| {
            let (fe, ty) = select_field(e.clone(), t, &n)?;
            Ok((fe, n, ty))
        })
        .collect()
}

/// Field `idx` of `e`, taken directly from a row constructor.
fn field_of(e: Expr, idx: usize) -> Expr {
    match e {
        Expr::Row { mut fields, .. } if idx < fields.len() => fields.swap_remove(idx),
        other => Expr::Field { expr: Box::new(other), idx },
    }
}

/// Wraps a single-column plan in a cast when its column type differs from `to`.
fn cast_plan_column(plan: crate::plan::Rel, from: &Type, to: &Type) -> crate::plan::Rel {
    if from == to {
        return plan;
    }
    crate::plan::Rel::Project {
        input: Box::new(plan),
        exprs: vec![Expr::Cast { expr: Box::new(Expr::Col(0)), to: to.clone(), typmod: -1 }],
    }
}

/// Merges a named window into an OVER spec (`OVER (w ORDER BY x)`).
fn resolve_window_spec(spec: &ir::WindowSpec, named: &[ir::NamedWindow]) -> SqlResult<ir::WindowSpec> {
    let Some(base) = &spec.base else { return Ok(spec.clone()) };
    let parent = named
        .iter()
        .find(|w| &w.name == base)
        .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("window \"{base}\" does not exist")))?;
    let parent = resolve_window_spec(&parent.spec, named)?;
    Ok(ir::WindowSpec {
        base: None,
        partition_by: if spec.partition_by.is_empty() { parent.partition_by } else { spec.partition_by.clone() },
        order_by: if spec.order_by.is_empty() { parent.order_by } else { spec.order_by.clone() },
        frame: spec.frame.clone().or(parent.frame),
    })
}

pub fn contains_agg(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if matches!(x, Expr::AggCall(_)) {
            found = true;
        }
    });
    found
}

pub fn contains_window(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        if matches!(x, Expr::WindowCall(_)) {
            found = true;
        }
    });
    found
}

/// Folds constant function calls whose arguments are all constants (immutable builtins only;
/// volatile ones like `random()` and `now()` take no arguments and are never folded here).
fn fold(e: Expr) -> Expr {
    e
}

/// Flattens nested AND/OR lists.
fn flatten(e: Expr) -> Expr {
    match e {
        Expr::And(items) => Expr::And(items.into_iter().flat_map(|i| if let Expr::And(inner) = i { inner } else { vec![i] }).collect()),
        Expr::Or(items) => Expr::Or(items.into_iter().flat_map(|i| if let Expr::Or(inner) = i { inner } else { vec![i] }).collect()),
        other => other,
    }
}

/// The set-returning functions usable in the select list: (output column types, argument
/// types, implementation).
fn select_list_srf(name: &str, types: &[Type]) -> SqlResult<Option<funcs::array::SrfResolution>> {
    if let Some(r) = funcs::array::resolve_srf(name, types)? {
        return Ok(Some(r));
    }
    if let Some((cols, f)) = funcs::json::resolve_srf(name, types) {
        return Ok(Some((cols.into_iter().map(|(_, t)| t).collect(), vec![funcs::json::srf_arg_type(name)], f)));
    }
    if let Some((cols, targets, f)) = funcs::json::resolve_path_srf(name, types) {
        return Ok(Some((cols.into_iter().map(|(_, t)| t).collect(), targets, f)));
    }
    Ok(None)
}
