//! Aggregate functions and the GROUP BY operator.

use crate::eval::{EvalCtx, eval, eval_pred};
use crate::plan::{AggCall, AggKind, Expr, Row};
use crate::run::sort_rows;
use pgmem_types::{GroupKey, Interval, Jsonb, Numeric, SqlError, SqlResult, Type, Value, code, compare_values};
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
    /// `float8_regr_accum`: N, Sx, Sy, Sxx, Syy, Sxy (Youngs-Cramer).
    Regr([f64; 6]),
    /// Ordered-set aggregates: the sorted non-null inputs and the direct argument.
    Ordered(Vec<Value>, Option<Value>),
    /// Hypothetical-set aggregates: the sorted rows and the hypothetical row.
    Hypothetical(Vec<Vec<Value>>, Option<Vec<Value>>),
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
            AggKind::ArrayAgg
            | AggKind::JsonbAgg
            | AggKind::JsonAgg
            | AggKind::JsonbAggStrict
            | AggKind::JsonAggStrict
            | AggKind::RangeAgg
            | AggKind::RangeIntersectAgg => Accumulator::Values(Vec::new()),
            AggKind::JsonbObjectAgg | AggKind::JsonObjectAgg | AggKind::JsonbObjectAggStrict | AggKind::JsonObjectAggStrict => {
                Accumulator::Pairs(Vec::new())
            }
            AggKind::StddevSamp | AggKind::StddevPop | AggKind::VarSamp | AggKind::VarPop => Accumulator::Stats(Vec::new(), Vec::new()),
            AggKind::BitAnd | AggKind::BitOr | AggKind::BitXor => Accumulator::Bits(None),
            AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode => Accumulator::Ordered(Vec::new(), None),
            AggKind::HypRank | AggKind::HypDenseRank | AggKind::HypPercentRank | AggKind::HypCumeDist => {
                Accumulator::Hypothetical(Vec::new(), None)
            }
            _ => Accumulator::Regr([0.0; 6]),
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
            Accumulator::Values(items) => add_value(call, items, first)?,
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
                        (Some(s), AggKind::BitXor) => s ^ v,
                        (Some(s), _) => s | v,
                    });
                }
            }
            Accumulator::Regr(t) => {
                // Arguments are (Y, X); a row with either NULL is skipped.
                if let (Some(y), Some(x)) = (first.as_f64(), args.get(1).and_then(Value::as_f64)) {
                    regr_accum(t, y, x)?;
                }
            }
            Accumulator::Hypothetical(rows, direct) => {
                let n = args.len() / 2;
                direct.get_or_insert_with(|| args[n..].to_vec());
                rows.push(args[..n].to_vec());
            }
            Accumulator::Ordered(values, direct) => {
                direct.get_or_insert_with(|| args.get(1).cloned().unwrap_or(Value::Null));
                if !first.is_null() {
                    values.push(first);
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
                    AggKind::RangeAgg | AggKind::RangeIntersectAgg => return range_agg_finish(call.kind, items),
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
            Accumulator::Regr(t) => regr_finish(call.kind, t),
            Accumulator::Ordered(values, direct) => ordered_finish(call, values, direct.as_ref())?,
            Accumulator::Hypothetical(rows, direct) => hypothetical_finish(call, rows, direct.as_deref())?,
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
        // float8_accum's Youngs-Cramer update, in input order.
        let (mut nf, mut sx, mut sxx) = (0.0f64, 0.0f64, 0.0f64);
        for &x in floats {
            let prev = nf;
            nf += 1.0;
            sx += x;
            if prev > 0.0 {
                let tmp = x * nf - sx;
                sxx += tmp * tmp / (nf * prev);
            } else if !x.is_finite() {
                sxx = f64::NAN;
            }
        }
        let var = sxx / if samp { nf - 1.0 } else { nf };
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
    if numer.signum() <= 0 {
        return Ok(Value::Numeric(Numeric::zero()));
    }
    let denom = if samp { count.mul(&count.sub(&Numeric::from_i64(1))) } else { count.mul(&count) };
    let var = numer.div(&denom)?;
    // The square root keeps the variance's scale.
    Ok(Value::Numeric(if sqrt { var.sqrt_to_scale(var.scale())? } else { var }))
}

/// `range_agg` (the union of the non-NULL inputs, as a multirange) and `range_intersect_agg`.
fn range_agg_finish(kind: AggKind, items: &[Value]) -> SqlResult<Value> {
    use pgmem_types::range;
    let mut ranges: Vec<range::Range> = Vec::new();
    for v in items {
        match v {
            Value::Range(r) => ranges.push((**r).clone()),
            Value::Multirange(_, rs) => ranges.extend(rs.iter().cloned()),
            _ => {}
        }
    }
    if ranges.is_empty() {
        return Ok(Value::Null);
    }
    if kind == AggKind::RangeAgg {
        let sub = ranges[0].sub.clone();
        return Ok(Value::Multirange(sub, range::normalize(ranges)?));
    }
    let mut acc = ranges[0].clone();
    for r in &ranges[1..] {
        acc = range::intersect(&acc, r)?;
    }
    Ok(Value::Range(Box::new(acc)))
}

/// `array_agg` / `json[b]_agg` input: checks for array inputs, then collects the value.
fn add_value(call: &AggCall, items: &mut Vec<Value>, first: Value) -> SqlResult<()> {
    if call.kind == AggKind::ArrayAgg && matches!(call.arg_ty, Type::Array(_)) {
        match &first {
            Value::Null => return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "cannot accumulate null arrays")),
            Value::Array(a) if a.is_empty() => {
                return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "cannot accumulate empty arrays"));
            }
            Value::Array(a) => {
                if let Some(Value::Array(prev)) = items.first()
                    && pgmem_types::binary::array_dims(prev) != pgmem_types::binary::array_dims(a)
                {
                    return Err(SqlError::new(code::ARRAY_SUBSCRIPT_ERROR, "cannot accumulate arrays of different dimensionality"));
                }
            }
            _ => {}
        }
    }
    if !(first.is_null() && matches!(call.kind, AggKind::JsonbAggStrict | AggKind::JsonAggStrict)) {
        items.push(first);
    }
    Ok(())
}

fn regr_accum(t: &mut [f64; 6], y: f64, x: f64) -> SqlResult<()> {
    let [n, sx, sy, sxx, syy, sxy] = t;
    let prev = *n;
    *n += 1.0;
    *sx += x;
    *sy += y;
    if prev > 0.0 {
        let tmp_x = x * *n - *sx;
        let tmp_y = y * *n - *sy;
        let scale = 1.0 / (*n * prev);
        *sxx += tmp_x * tmp_x * scale;
        *syy += tmp_y * tmp_y * scale;
        *sxy += tmp_x * tmp_y * scale;
        if (sx.is_infinite() || sxx.is_infinite() || sy.is_infinite() || syy.is_infinite() || sxy.is_infinite())
            && x.is_finite()
            && y.is_finite()
        {
            return Err(SqlError::new(code::NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow"));
        }
    } else {
        if !x.is_finite() {
            *sxx = f64::NAN;
            *sxy = f64::NAN;
        }
        if !y.is_finite() {
            *syy = f64::NAN;
            *sxy = f64::NAN;
        }
    }
    Ok(())
}

/// The final functions of the `float8_regr_*` aggregates.
fn regr_finish(kind: AggKind, t: &[f64; 6]) -> Value {
    let [n, sx, sy, sxx, syy, sxy] = *t;
    let v = |f: f64| Value::Float8(f);
    if kind == AggKind::RegrCount {
        #[allow(clippy::cast_possible_truncation)]
        return Value::Int8(n as i64);
    }
    if n < 1.0 {
        return Value::Null;
    }
    match kind {
        AggKind::RegrSxx => v(sxx),
        AggKind::RegrSyy => v(syy),
        AggKind::RegrSxy => v(sxy),
        AggKind::RegrAvgX => v(sx / n),
        AggKind::RegrAvgY => v(sy / n),
        AggKind::CovarPop => v(sxy / n),
        AggKind::CovarSamp if n < 2.0 => Value::Null,
        AggKind::CovarSamp => v(sxy / (n - 1.0)),
        AggKind::Corr => {
            if sxx == 0.0 || syy == 0.0 {
                return Value::Null;
            }
            let product = sxx * syy;
            let product = if product == 0.0 || product.is_infinite() { sxx.sqrt() * syy.sqrt() } else { product.sqrt() };
            v((sxy / product).clamp(-1.0, 1.0))
        }
        AggKind::RegrR2 => {
            if sxx == 0.0 {
                return Value::Null;
            }
            if syy == 0.0 {
                return v(1.0);
            }
            v((sxy * sxy) / (sxx * syy))
        }
        AggKind::RegrSlope if sxx == 0.0 => Value::Null,
        AggKind::RegrSlope => v(sxy / sxx),
        AggKind::RegrIntercept if sxx == 0.0 => Value::Null,
        AggKind::RegrIntercept => v((sy - sx * sxy / sxx) / n),
        _ => Value::Null,
    }
}

/// The position of `row` relative to `h` under the aggregate's WITHIN GROUP ordering.
fn order_cmp(call: &AggCall, row: &[Value], h: &[Value]) -> SqlResult<std::cmp::Ordering> {
    use std::cmp::Ordering;
    for (i, key) in call.order_by.iter().enumerate() {
        let (a, b) = (&row[i], &h[i]);
        let o = match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let o = compare_values(a, b)?;
                if key.desc { o.reverse() } else { o }
            }
        };
        if o.is_ne() {
            return Ok(o);
        }
    }
    Ok(Ordering::Equal)
}

/// `rank`, `dense_rank`, `percent_rank`, `cume_dist` of the hypothetical row among `rows`.
fn hypothetical_finish(call: &AggCall, rows: &[Vec<Value>], h: Option<&[Value]>) -> SqlResult<Value> {
    #[allow(clippy::cast_precision_loss)]
    let total = rows.len() as f64;
    let Some(h) = h else {
        return Ok(match call.kind {
            AggKind::HypRank | AggKind::HypDenseRank => Value::Int8(1),
            AggKind::HypPercentRank => Value::Float8(0.0),
            _ => Value::Float8(1.0),
        });
    };
    let mut before = 0i64;
    let mut not_after = 0i64;
    let mut distinct_before: Vec<&Vec<Value>> = Vec::new();
    for r in rows {
        match order_cmp(call, r, h)? {
            std::cmp::Ordering::Less => {
                before += 1;
                not_after += 1;
                if !distinct_before.iter().any(|d| order_cmp(call, d, r).is_ok_and(std::cmp::Ordering::is_eq)) {
                    distinct_before.push(r);
                }
            }
            std::cmp::Ordering::Equal => not_after += 1,
            std::cmp::Ordering::Greater => {}
        }
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(match call.kind {
        AggKind::HypRank => Value::Int8(before + 1),
        AggKind::HypDenseRank => Value::Int8(i64::try_from(distinct_before.len()).unwrap_or(i64::MAX) + 1),
        AggKind::HypPercentRank => Value::Float8(if total == 0.0 { 0.0 } else { before as f64 / total }),
        _ => Value::Float8((not_after as f64 + 1.0) / (total + 1.0)),
    })
}

/// `percentile_cont`, `percentile_disc` and `mode` over the sorted inputs.
fn ordered_finish(call: &AggCall, values: &[Value], direct: Option<&Value>) -> SqlResult<Value> {
    if call.kind == AggKind::Mode {
        let mut best: Option<(&Value, usize)> = None;
        let mut i = 0;
        while i < values.len() {
            let mut j = i + 1;
            while j < values.len() && compare_values(&values[i], &values[j])?.is_eq() {
                j += 1;
            }
            if best.is_none_or(|(_, n)| j - i > n) {
                best = Some((&values[i], j - i));
            }
            i = j;
        }
        return Ok(best.map_or(Value::Null, |(v, _)| v.clone()));
    }
    let one = |p: &Value| -> SqlResult<Value> {
        let Some(p) = p.as_f64() else { return Ok(Value::Null) };
        if !(0.0..=1.0).contains(&p) || p.is_nan() {
            return Err(SqlError::new(
                code::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("percentile value {} is not between 0 and 1", pgmem_types::text::format_float8(p)),
            ));
        }
        if values.is_empty() {
            return Ok(Value::Null);
        }
        #[allow(clippy::cast_precision_loss)]
        let count = values.len() as f64;
        if call.kind == AggKind::PercentileDisc {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let row = ((p * count).ceil() as usize).max(1);
            return Ok(values[row.min(values.len()) - 1].clone());
        }
        let pos = p * (count - 1.0);
        let (lo, hi) = (pos.floor(), pos.ceil());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (a, b) = (&values[lo as usize], &values[hi as usize]);
        if lo == hi {
            return Ok(a.clone());
        }
        let frac = pos - lo;
        Ok(match (a, b) {
            (Value::Interval(x), Value::Interval(y)) => Value::Interval(x.checked_add(&y.checked_add(&x.negate()?)?.mul_f64(frac)?)?),
            _ => {
                let (x, y) = (a.as_f64().unwrap_or(f64::NAN), b.as_f64().unwrap_or(f64::NAN));
                Value::Float8(x + (y - x) * frac)
            }
        })
    };
    match direct {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::Array(ps)) => Ok(Value::Array(ps.iter().map(one).collect::<SqlResult<Vec<_>>>()?)),
        Some(p) => one(p),
    }
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

/// GROUP BY GROUPING SETS: one aggregation per set, the keys outside the set NULL, each row
/// followed by its set's index (for `GROUPING()`).
pub fn run_grouping_sets(rows: &[Row], group: &[Expr], sets: &[Vec<usize>], aggs: &[AggCall], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let mut out = Vec::new();
    for (si, set) in sets.iter().enumerate() {
        let keys: Vec<Expr> = set.iter().map(|&i| group[i].clone()).collect();
        for r in run_aggregate(rows, &keys, aggs, ctx)? {
            let mut full = vec![Value::Null; group.len()];
            for (pos, &i) in set.iter().enumerate() {
                full[i] = r[pos].clone();
            }
            full.extend(r[set.len()..].iter().cloned());
            full.push(Value::Int4(i32::try_from(si).unwrap_or(i32::MAX)));
            out.push(full);
        }
    }
    Ok(out)
}
