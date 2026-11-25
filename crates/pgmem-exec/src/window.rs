//! Window functions.

use crate::agg::aggregate_rows;
use crate::eval::{EvalCtx, eval};
use crate::plan::{FrameBound, FrameUnits, Row, WinCall, WinKind};
use crate::run::compare_sort;
use pgmem_types::{GroupKey, SqlError, SqlResult, Value};
use std::cmp::Ordering;

/// Appends one column per window call to every row, keeping the input order.
pub fn run_window(rows: Vec<Row>, calls: &[WinCall], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let mut results: Vec<Vec<Value>> = Vec::with_capacity(calls.len());
    for call in calls {
        results.push(compute(&rows, call, ctx)?);
    }
    Ok(rows
        .into_iter()
        .enumerate()
        .map(|(i, mut r)| {
            for res in &results {
                r.push(res[i].clone());
            }
            r
        })
        .collect())
}

fn compute(rows: &[Row], call: &WinCall, ctx: &EvalCtx) -> SqlResult<Vec<Value>> {
    let n = rows.len();
    let mut part_keys = Vec::with_capacity(n);
    let mut order_keys = Vec::with_capacity(n);
    for r in rows {
        part_keys.push(call.partition_by.iter().map(|e| eval(e, r, ctx).map(|v| v.group_key())).collect::<SqlResult<Vec<GroupKey>>>()?);
        order_keys.push(call.order_by.iter().map(|k| eval(&k.expr, r, ctx)).collect::<SqlResult<Vec<Value>>>()?);
    }
    let cmp_order = |a: usize, b: usize| -> Ordering {
        for (i, k) in call.order_by.iter().enumerate() {
            match compare_sort(&order_keys[a][i], &order_keys[b][i], k.desc, k.nulls_first) {
                Ok(Ordering::Equal) | Err(_) => continue,
                Ok(o) => return o,
            }
        }
        Ordering::Equal
    };
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| part_keys[a].cmp(&part_keys[b]).then_with(|| cmp_order(a, b)));
    let mut out = vec![Value::Null; n];
    let mut start = 0;
    while start < n {
        let mut end = start + 1;
        while end < n && part_keys[idx[end]] == part_keys[idx[start]] {
            end += 1;
        }
        let part = &idx[start..end];
        compute_partition(rows, part, call, ctx, &cmp_order, &order_keys, &mut out)?;
        start = end;
    }
    Ok(out)
}

fn compute_partition(
    rows: &[Row],
    part: &[usize],
    call: &WinCall,
    ctx: &EvalCtx,
    cmp_order: &dyn Fn(usize, usize) -> Ordering,
    order_keys: &[Vec<Value>],
    out: &mut [Value],
) -> SqlResult<()> {
    let len = part.len();
    // Peer groups: [peer_start[i], peer_end[i]) for each position.
    let mut peer_start = vec![0; len];
    let mut peer_end = vec![0; len];
    let mut i = 0;
    let mut groups = 0usize;
    let mut group_of = vec![0usize; len];
    while i < len {
        let mut j = i + 1;
        while j < len && cmp_order(part[i], part[j]) == Ordering::Equal {
            j += 1;
        }
        for k in i..j {
            peer_start[k] = i;
            peer_end[k] = j;
            group_of[k] = groups;
        }
        groups += 1;
        i = j;
    }
    for o in &call.frame.range_offsets {
        let v = eval(o, &[], ctx)?;
        if pgmem_types::compare_values(&v, &Value::Int4(0)).is_ok_and(Ordering::is_lt)
            || matches!(&v, Value::Interval(iv) if iv.months < 0 || iv.days < 0 || iv.micros < 0)
        {
            return Err(SqlError::new("22013", "invalid preceding or following size in window function"));
        }
    }
    // RANGE offset bounds: the current row's key shifted by the offset (`bound`), compared with
    // the keys in sort order. A NULL key's frame is its peers (None).
    let range_keys: Vec<Value> = match &call.frame.range_key {
        Some(k) => part.iter().map(|&r| eval(k, &rows[r], ctx)).collect::<SqlResult<Vec<_>>>()?,
        None => Vec::new(),
    };
    let key = |p: usize| range_keys.get(p).unwrap_or(&order_keys[part[p]][0]);
    let desc = call.order_by.first().is_some_and(|k| k.desc);
    let before = |a: &Value, b: &Value| -> bool {
        let o = pgmem_types::compare_values(a, b).unwrap_or(Ordering::Equal);
        if desc { o.is_gt() } else { o.is_lt() }
    };
    let range_start = |pos: usize, bound: &crate::plan::Expr| -> SqlResult<Option<usize>> {
        if key(pos).is_null() {
            return Ok(None);
        }
        let b = eval(bound, &rows[part[pos]], ctx)?;
        Ok(Some((0..len).find(|&k| !key(k).is_null() && !before(key(k), &b)).unwrap_or(len)))
    };
    let range_end = |pos: usize, bound: &crate::plan::Expr| -> SqlResult<Option<usize>> {
        if key(pos).is_null() {
            return Ok(None);
        }
        let b = eval(bound, &rows[part[pos]], ctx)?;
        Ok(Some((0..len).rev().find(|&k| !key(k).is_null() && !before(&b, key(k))).map_or(0, |k| k + 1)))
    };
    let frame_range = |pos: usize| -> SqlResult<(usize, usize)> {
        let f = &call.frame;
        let offset = |e: &crate::plan::Expr| -> SqlResult<usize> {
            let v = eval(e, &[], ctx)?;
            let n =
                v.as_i64().ok_or_else(|| SqlError::feature_not_supported("non-integer window frame offsets are not supported by pgmem"))?;
            if n < 0 {
                return Err(SqlError::new("22013", "frame starting offset must not be negative"));
            }
            Ok(usize::try_from(n).unwrap_or(usize::MAX))
        };
        let lo = match (&f.units, &f.start) {
            (_, FrameBound::UnboundedPreceding) => 0,
            (FrameUnits::Rows, FrameBound::Preceding(e)) => pos.saturating_sub(offset(e)?),
            (FrameUnits::Rows, FrameBound::CurrentRow) => pos,
            (FrameUnits::Rows, FrameBound::Following(e)) => pos.saturating_add(offset(e)?),
            (_, FrameBound::CurrentRow) => peer_start[pos],
            (FrameUnits::Range, FrameBound::Preceding(e) | FrameBound::Following(e)) => range_start(pos, e)?.unwrap_or(peer_start[pos]),
            (FrameUnits::Groups, FrameBound::Preceding(e)) => {
                let g = group_of[pos].saturating_sub(offset(e)?);
                group_of.iter().position(|x| *x == g).unwrap_or(0)
            }
            (_, FrameBound::UnboundedFollowing) => len,
            _ => return Err(SqlError::feature_not_supported("RANGE frames with offsets are not supported by pgmem")),
        };
        let hi = match (&f.units, &f.end) {
            (_, FrameBound::UnboundedFollowing) => len,
            (FrameUnits::Rows, FrameBound::Following(e)) => pos.saturating_add(offset(e)?).saturating_add(1).min(len),
            (FrameUnits::Rows, FrameBound::CurrentRow) => pos + 1,
            (FrameUnits::Rows, FrameBound::Preceding(e)) => (pos + 1).saturating_sub(offset(e)?),
            (_, FrameBound::CurrentRow) => peer_end[pos],
            (FrameUnits::Range, FrameBound::Preceding(e) | FrameBound::Following(e)) => range_end(pos, e)?.unwrap_or(peer_end[pos]),
            (FrameUnits::Groups, FrameBound::Following(e)) => {
                let g = group_of[pos].saturating_add(offset(e)?);
                group_of.iter().rposition(|x| *x <= g).map_or(len, |p| p + 1)
            }
            (_, FrameBound::UnboundedPreceding) => 0,
            _ => return Err(SqlError::feature_not_supported("RANGE frames with offsets are not supported by pgmem")),
        };
        Ok((lo.min(len), hi.min(len)))
    };
    // The frame's rows, without the EXCLUDEd ones.
    let frame_for = |pos: usize| -> SqlResult<Vec<usize>> {
        let (lo, hi) = frame_range(pos)?;
        let excluded = |k: usize| match call.frame.exclude {
            pgmem_ir::FrameExclude::NoOthers => false,
            pgmem_ir::FrameExclude::CurrentRow => k == pos,
            pgmem_ir::FrameExclude::Group => peer_start[pos] <= k && k < peer_end[pos],
            pgmem_ir::FrameExclude::Ties => k != pos && peer_start[pos] <= k && k < peer_end[pos],
        };
        Ok((lo..hi.max(lo)).filter(|&k| !excluded(k)).collect())
    };
    let arg = |pos: usize, i: usize| -> SqlResult<Value> {
        match call.args.get(i) {
            Some(e) => eval(e, &rows[part[pos]], ctx),
            None => Ok(Value::Null),
        }
    };
    for pos in 0..len {
        let row_idx = part[pos];
        let value = match call.kind {
            WinKind::RowNumber => Value::Int8(i64::try_from(pos + 1).unwrap_or(i64::MAX)),
            WinKind::Rank => Value::Int8(i64::try_from(peer_start[pos] + 1).unwrap_or(i64::MAX)),
            WinKind::DenseRank => Value::Int8(i64::try_from(group_of[pos] + 1).unwrap_or(i64::MAX)),
            #[allow(clippy::cast_precision_loss)]
            WinKind::PercentRank => Value::Float8(if len <= 1 { 0.0 } else { peer_start[pos] as f64 / (len - 1) as f64 }),
            #[allow(clippy::cast_precision_loss)]
            WinKind::CumeDist => Value::Float8(peer_end[pos] as f64 / len as f64),
            WinKind::Ntile => {
                let buckets = arg(pos, 0)?.as_i64().ok_or_else(|| SqlError::new("22014", "argument of ntile must be greater than zero"))?;
                if buckets <= 0 {
                    return Err(SqlError::new("22014", "argument of ntile must be greater than zero"));
                }
                let b = usize::try_from(buckets).unwrap_or(usize::MAX);
                let per = len / b;
                let extra = len % b;
                // The first `extra` buckets hold per+1 rows.
                let bucket = if pos < extra * (per + 1) { pos / (per + 1) } else { extra + (pos - extra * (per + 1)) / per.max(1) };
                Value::Int4(i32::try_from(bucket + 1).unwrap_or(i32::MAX))
            }
            WinKind::Lag | WinKind::Lead => {
                let off = match call.args.get(1) {
                    Some(_) => arg(pos, 1)?.as_i64().unwrap_or(1),
                    None => 1,
                };
                let target =
                    if call.kind == WinKind::Lag { i64::try_from(pos).unwrap_or(0) - off } else { i64::try_from(pos).unwrap_or(0) + off };
                if target >= 0 && (target as usize) < len {
                    arg(usize::try_from(target).unwrap_or(0), 0)?
                } else if call.args.len() > 2 {
                    arg(pos, 2)?
                } else {
                    Value::Null
                }
            }
            WinKind::FirstValue | WinKind::LastValue | WinKind::NthValue => {
                let members = frame_for(pos)?;
                let target = match call.kind {
                    WinKind::FirstValue => members.first().copied(),
                    WinKind::LastValue => members.last().copied(),
                    _ => {
                        let nth = arg(pos, 1)?.as_i64().unwrap_or(0);
                        if nth <= 0 {
                            return Err(SqlError::new("22016", "argument of nth_value must be greater than zero"));
                        }
                        members.get(usize::try_from(nth - 1).unwrap_or(usize::MAX)).copied()
                    }
                };
                match target {
                    Some(t) => arg(t, 0)?,
                    None => Value::Null,
                }
            }
            WinKind::Agg(_) => {
                let agg = call.agg.as_ref().ok_or_else(|| SqlError::internal("window aggregate without call"))?;
                let members: Vec<&Row> = frame_for(pos)?.into_iter().map(|k| &rows[part[k]]).collect();
                aggregate_rows(agg, &members, ctx)?
            }
        };
        out[row_idx] = value;
    }
    Ok(())
}
