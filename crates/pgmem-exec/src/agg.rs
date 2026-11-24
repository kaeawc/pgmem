//! Aggregate functions and the GROUP BY operator.

use crate::eval::{EvalCtx, eval, eval_pred};
use crate::plan::{AggCall, AggKind, Expr, Row};
use crate::run::sort_rows;
use pgmem_types::{GroupKey, Interval, Jsonb, Numeric, SqlError, SqlResult, Type, Value, compare_values};
use std::collections::HashMap;

/// Running state of one aggregate.
pub enum Accumulator {
    Count(i64),
    IntSum {
        sum: i128,
        n: i64,
    },
    FloatSum {
        sum: f64,
        n: i64,
    },
    NumericSum {
        sum: Numeric,
        n: i64,
    },
    IntervalSum {
        sum: Interval,
        n: i64,
    },
    MinMax(Option<Value>),
    Bool(Option<bool>),
    StringAgg(Option<String>),
    Values(Vec<Value>),
    Pairs(Vec<(Value, Value)>),
    /// Welford-free two-pass variance: keep the inputs.
    Stats(Vec<f64>, Vec<Numeric>),
    Bits(Option<i64>),
}

impl Accumulator {
    pub fn new(call: &AggCall) -> Accumulator {
        match call.kind {
            AggKind::CountStar | AggKind::Count => Accumulator::Count(0),
            AggKind::Sum | AggKind::Avg => match call.arg_ty {
                Type::Int2 | Type::Int4 | Type::Int8 => Accumulator::IntSum { sum: 0, n: 0 },
                Type::Float4 | Type::Float8 => Accumulator::FloatSum { sum: 0.0, n: 0 },
                Type::Interval => Accumulator::IntervalSum { sum: Interval::ZERO, n: 0 },
                _ => Accumulator::NumericSum { sum: Numeric::zero(), n: 0 },
            },
            AggKind::Min | AggKind::Max => Accumulator::MinMax(None),
            AggKind::BoolAnd | AggKind::BoolOr => Accumulator::Bool(None),
            AggKind::StringAgg => Accumulator::StringAgg(None),
            AggKind::ArrayAgg | AggKind::JsonbAgg | AggKind::JsonAgg | AggKind::JsonbAggStrict | AggKind::JsonAggStrict => {
                Accumulator::Values(Vec::new())
            }
            AggKind::JsonbObjectAgg | AggKind::JsonObjectAgg | AggKind::JsonbObjectAggStrict | AggKind::JsonObjectAggStrict => {
                Accumulator::Pairs(Vec::new())
            }
            AggKind::StddevSamp | AggKind::StddevPop | AggKind::VarSamp | AggKind::VarPop => Accumulator::Stats(Vec::new(), Vec::new()),
            AggKind::BitAnd | AggKind::BitOr => Accumulator::Bits(None),
        }
    }

    /// Feeds one input row's argument values (already filtered and de-duplicated).
    pub fn add(&mut self, call: &AggCall, args: &[Value]) -> SqlResult<()> {
        let first = args.first().cloned().unwrap_or(Value::Null);
        match self {
            Accumulator::Count(n) => {
                if call.kind == AggKind::CountStar || !first.is_null() {
                    *n += 1;
                }
            }
            Accumulator::IntSum { sum, n } => {
                if let Some(v) = first.as_i64() {
                    *sum += i128::from(v);
                    *n += 1;
                }
            }
            Accumulator::FloatSum { sum, n } => {
                if let Some(v) = first.as_f64() {
                    *sum += v;
                    *n += 1;
                }
            }
            Accumulator::NumericSum { sum, n } => match &first {
                Value::Numeric(v) => {
                    *sum = sum.add(v);
                    *n += 1;
                }
                Value::Null => {}
                other => {
                    if let Some(i) = other.as_i64() {
                        *sum = sum.add(&Numeric::from_i64(i));
                        *n += 1;
                    }
                }
            },
            Accumulator::IntervalSum { sum, n } => {
                if let Value::Interval(iv) = &first {
                    *sum = sum.checked_add(iv)?;
                    *n += 1;
                }
            }
            Accumulator::MinMax(best) => {
                if !first.is_null() {
                    let replace = match best {
                        None => true,
                        Some(b) => {
                            let o = compare_values(&first, b)?;
                            if call.kind == AggKind::Max { o.is_gt() } else { o.is_lt() }
                        }
                    };
                    if replace {
                        *best = Some(first);
                    }
                }
            }
            Accumulator::Bool(state) => {
                if let Value::Bool(b) = first {
                    *state = Some(match (*state, call.kind) {
                        (None, _) => b,
                        (Some(s), AggKind::BoolAnd) => s && b,
                        (Some(s), _) => s || b,
                    });
                }
            }
            Accumulator::StringAgg(state) => {
                if let Value::Text(s) = &first {
                    let sep = match args.get(1) {
                        Some(Value::Text(sep)) => sep.as_str(),
                        _ => "",
                    };
                    match state {
                        None => *state = Some(s.clone()),
                        Some(acc) => {
                            acc.push_str(sep);
                            acc.push_str(s);
                        }
                    }
                } else if let Value::Bytea(_) = &first {
                    return Err(SqlError::feature_not_supported("string_agg(bytea) is not supported by pgmem"));
                }
            }
            Accumulator::Values(items) => {
                if !(first.is_null() && matches!(call.kind, AggKind::JsonbAggStrict | AggKind::JsonAggStrict)) {
                    items.push(first);
                }
            }
            Accumulator::Pairs(items) => {
                if first.is_null() {
                    return Err(SqlError::new("22004", "field name must not be null"));
                }
                let v = args.get(1).cloned().unwrap_or(Value::Null);
                if !(v.is_null() && matches!(call.kind, AggKind::JsonbObjectAggStrict | AggKind::JsonObjectAggStrict)) {
                    items.push((first, v));
                }
            }
            Accumulator::Stats(floats, nums) => match &first {
                Value::Null => {}
                Value::Float4(_) | Value::Float8(_) => floats.push(first.as_f64().unwrap_or_default()),
                Value::Numeric(n) => nums.push(n.clone()),
                other => {
                    if let Some(i) = other.as_i64() {
                        nums.push(Numeric::from_i64(i));
                    }
                }
            },
            Accumulator::Bits(state) => {
                if let Some(v) = first.as_i64() {
                    *state = Some(match (*state, call.kind) {
                        (None, _) => v,
                        (Some(s), AggKind::BitAnd) => s & v,
                        (Some(s), _) => s | v,
                    });
                }
            }
        }
        Ok(())
    }

    pub fn finish(&self, call: &AggCall) -> SqlResult<Value> {
        Ok(match self {
            Accumulator::Count(n) => Value::Int8(*n),
            Accumulator::IntSum { sum, n } => {
                if *n == 0 {
                    return Ok(Value::Null);
                }
                match (call.kind, &call.ty) {
                    (AggKind::Sum, Type::Int8) => Value::Int8(i64::try_from(*sum).map_err(|_| SqlError::out_of_range("bigint"))?),
                    (AggKind::Sum, _) => Value::Numeric(Numeric::from_i128(*sum)),
                    _ => Value::Numeric(Numeric::from_i128(*sum).div(&Numeric::from_i64(*n))?),
                }
            }
            Accumulator::FloatSum { sum, n } => {
                if *n == 0 {
                    return Ok(Value::Null);
                }
                #[allow(clippy::cast_precision_loss)]
                let v = if call.kind == AggKind::Avg { *sum / *n as f64 } else { *sum };
                #[allow(clippy::cast_possible_truncation)]
                if call.ty == Type::Float4 { Value::Float4(v as f32) } else { Value::Float8(v) }
            }
            Accumulator::NumericSum { sum, n } => {
                if *n == 0 {
                    return Ok(Value::Null);
                }
                if call.kind == AggKind::Avg { Value::Numeric(sum.div(&Numeric::from_i64(*n))?) } else { Value::Numeric(sum.clone()) }
            }
            Accumulator::IntervalSum { sum, n } => {
                if *n == 0 {
                    return Ok(Value::Null);
                }
                #[allow(clippy::cast_precision_loss)]
                if call.kind == AggKind::Avg { Value::Interval(sum.mul_f64(1.0 / *n as f64)?) } else { Value::Interval(*sum) }
            }
            Accumulator::MinMax(v) => v.clone().unwrap_or(Value::Null),
            Accumulator::Bool(v) => v.map_or(Value::Null, Value::Bool),
            Accumulator::StringAgg(v) => v.clone().map_or(Value::Null, Value::Text),
            Accumulator::Values(items) => {
                if items.is_empty() {
                    return Ok(Value::Null);
                }
                match call.kind {
                    AggKind::JsonbAgg | AggKind::JsonbAggStrict => {
                        let arr = items.iter().map(crate::funcs::json::to_json_value).collect::<SqlResult<Vec<_>>>()?;
                        Value::Jsonb(Jsonb::from_value(serde_json::Value::Array(arr)))
                    }
                    AggKind::JsonAgg | AggKind::JsonAggStrict => Value::Json(crate::funcs::json::json_agg_text(items)?),
                    _ => Value::Array(items.clone()),
                }
            }
            Accumulator::Pairs(items) => {
                if items.is_empty() {
                    return Ok(Value::Null);
                }
                if matches!(call.kind, AggKind::JsonObjectAgg | AggKind::JsonObjectAggStrict) {
                    return Ok(Value::Json(crate::funcs::json::json_object_agg_text(items)?));
                }
                let mut map = serde_json::Map::new();
                for (k, v) in items {
                    let key = match k {
                        Value::Text(s) => s.clone(),
                        other => pgmem_types::text::encode(other, &other.natural_type())?,
                    };
                    map.insert(key, crate::funcs::json::to_json_value(v)?);
                }
                Value::Jsonb(Jsonb::from_value(serde_json::Value::Object(map)))
            }
            Accumulator::Stats(floats, nums) => stats_finish(call, floats, nums)?,
            Accumulator::Bits(v) => match (v, &call.ty) {
                (None, _) => Value::Null,
                (Some(b), Type::Int2) => Value::Int2(i16::try_from(*b).unwrap_or_default()),
                (Some(b), Type::Int4) => Value::Int4(i32::try_from(*b).unwrap_or_default()),
                (Some(b), _) => Value::Int8(*b),
            },
        })
    }
}

fn stats_finish(call: &AggCall, floats: &[f64], nums: &[Numeric]) -> SqlResult<Value> {
    let samp = matches!(call.kind, AggKind::StddevSamp | AggKind::VarSamp);
    let sqrt = matches!(call.kind, AggKind::StddevSamp | AggKind::StddevPop);
    if call.ty == Type::Float8 {
        let n = floats.len();
        if n == 0 || (samp && n < 2) {
            return Ok(Value::Null);
        }
        #[allow(clippy::cast_precision_loss)]
        let nf = n as f64;
        let mean = floats.iter().sum::<f64>() / nf;
        let ss: f64 = floats.iter().map(|x| (x - mean).powi(2)).sum();
        let var = ss / if samp { nf - 1.0 } else { nf };
        return Ok(Value::Float8(if sqrt { var.sqrt() } else { var }));
    }
    let n = nums.len();
    if n == 0 || (samp && n < 2) {
        return Ok(Value::Null);
    }
    // Postgres: var = (n*sum(x^2) - sum(x)^2) / (n*(n-1)) computed in numeric.
    let count = Numeric::from_i64(i64::try_from(n).unwrap_or(i64::MAX));
    let sum = nums.iter().fold(Numeric::zero(), |a, b| a.add(b));
    let sumsq = nums.iter().fold(Numeric::zero(), |a, b| a.add(&b.mul(b)));
    let numer = count.mul(&sumsq).sub(&sum.mul(&sum));
    let denom = if samp { count.mul(&count.sub(&Numeric::from_i64(1))) } else { count.mul(&count) };
    let var = numer.div(&denom)?;
    Ok(Value::Numeric(if sqrt { var.sqrt()? } else { var }))
}

/// Evaluates an aggregate call's inputs over a set of rows and folds them.
pub fn aggregate_rows(call: &AggCall, rows: &[&Row], ctx: &EvalCtx) -> SqlResult<Value> {
    let mut acc = Accumulator::new(call);
    // Collect argument tuples (with sort keys when ordered).
    let mut inputs: Vec<Vec<Value>> = Vec::new();
    let mut sort_input: Vec<Row> = Vec::new();
    for row in rows {
        if let Some(f) = &call.filter
            && !eval_pred(f, row, ctx)?
        {
            continue;
        }
        let args = call.args.iter().map(|a| eval(a, row, ctx)).collect::<SqlResult<Vec<_>>>()?;
        if call.order_by.is_empty() {
            inputs.push(args);
        } else {
            // Sort on (args ++ keys): evaluate keys against the input row, then reorder.
            let mut combined = args.clone();
            for k in &call.order_by {
                combined.push(eval(&k.expr, row, ctx)?);
            }
            sort_input.push(combined);
        }
    }
    if !call.order_by.is_empty() {
        let width = call.args.len();
        let keys: Vec<crate::plan::SortKey> = call
            .order_by
            .iter()
            .enumerate()
            .map(|(i, k)| crate::plan::SortKey { expr: Expr::Col(width + i), desc: k.desc, nulls_first: k.nulls_first })
            .collect();
        sort_rows(&mut sort_input, &keys, ctx)?;
        inputs = sort_input
            .into_iter()
            .map(|mut r| {
                r.truncate(width);
                r
            })
            .collect();
    }
    if call.distinct {
        let mut seen = std::collections::HashSet::new();
        inputs.retain(|args| {
            // DISTINCT ignores NULL inputs for strict aggregates like count(DISTINCT x).
            seen.insert(args.iter().map(Value::group_key).collect::<Vec<GroupKey>>())
        });
        if !matches!(call.kind, AggKind::ArrayAgg | AggKind::JsonbAgg | AggKind::JsonAgg) {
            inputs.retain(|a| a.first().is_some_and(|v| !v.is_null()));
        }
    }
    for args in &inputs {
        acc.add(call, args)?;
    }
    acc.finish(call)
}

/// GROUP BY: groups appear in first-seen order. With no grouping keys there is exactly one
/// output row, even for empty input.
pub fn run_aggregate(rows: &[Row], group: &[Expr], aggs: &[AggCall], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let mut order: Vec<(Vec<Value>, Vec<&Row>)> = Vec::new();
    let mut index: HashMap<Vec<GroupKey>, usize> = HashMap::new();
    for row in rows {
        let keys = group.iter().map(|g| eval(g, row, ctx)).collect::<SqlResult<Vec<_>>>()?;
        let gk: Vec<GroupKey> = keys.iter().map(Value::group_key).collect();
        match index.get(&gk) {
            Some(i) => order[*i].1.push(row),
            None => {
                index.insert(gk, order.len());
                order.push((keys, vec![row]));
            }
        }
    }
    if group.is_empty() && order.is_empty() {
        order.push((Vec::new(), Vec::new()));
    }
    // Postgres (16+) sorts a group's input for the first ordered aggregate, so aggregates
    // without ORDER BY see the rows in that order too.
    let presort = aggs.iter().find(|c| !c.order_by.is_empty()).map(|c| c.order_by.clone());
    let mut out = Vec::with_capacity(order.len());
    for (keys, members) in order {
        let sorted: Option<Vec<Row>> = match &presort {
            Some(sort_keys) => {
                let mut rows: Vec<Row> = members.iter().map(|r| (*r).clone()).collect();
                sort_rows(&mut rows, sort_keys, ctx)?;
                Some(rows)
            }
            None => None,
        };
        let members: Vec<&Row> = match &sorted {
            Some(rows) => rows.iter().collect(),
            None => members,
        };
        let mut r = keys;
        for call in aggs {
            r.push(aggregate_rows(call, &members, ctx)?);
        }
        out.push(r);
    }
    Ok(out)
}
