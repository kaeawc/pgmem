//! Output order of UNION / INTERSECT / EXCEPT and SELECT DISTINCT without ORDER BY.
//!
//! SQL leaves that order unspecified, but PostgreSQL's is deterministic and tests snapshot it.
//! Postgres 18 deduplicates either by sorting (rows come out in sort order) or through a
//! hash table (`execGrouping.c` over `lib/simplehash.h`: rows come out in bucket order). It
//! picks the cheaper plan by cost, so for inputs whose planner estimates are exact (FROM-less
//! SELECTs of constants, VALUES lists, and set operations / DISTINCT over them) this module
//! reproduces the cost estimates (`costsize.c`, `prepunion.c`, `planner.c`), the path choice
//! (`add_path`'s fuzzy cost comparison) and, for hashed plans, the hash of every datum
//! (`hashfn.c` and the types' hash functions), the table's initial size (from the estimated
//! group count) and its robin-hood insertion and iteration order. Inputs it cannot estimate
//! (table scans, filters, joins, ...) and types it does not hash keep first-seen order.

use crate::eval::EvalCtx;
use crate::plan::{Expr, Rel, Row, SetOpKind};
use crate::run::{compare_sort, run};
use pgmem_types::{GroupKey, Numeric, SqlResult, Type, Value};
use std::cmp::Ordering;
use std::collections::HashMap;

// ---------------------------------------------------------------------------------------
// Hash functions (src/common/hashfn.c and the hash support functions of each type).

fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(4);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(6);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(8);
    *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c);
    *a ^= c.rotate_left(16);
    *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a);
    *b ^= a.rotate_left(19);
    *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b);
    *c ^= b.rotate_left(4);
    *b = b.wrapping_add(*a);
}

fn final_mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(14));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(11));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(25));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(16));
    *a ^= *c;
    *a = a.wrapping_sub(c.rotate_left(4));
    *b ^= *a;
    *b = b.wrapping_sub(a.rotate_left(14));
    *c ^= *b;
    *c = c.wrapping_sub(b.rotate_left(24));
}

fn le_word(bytes: &[u8]) -> u32 {
    bytes.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (8 * i)))
}

/// `hash_bytes` (Bob Jenkins' lookup3, as Postgres computes it on a little-endian host).
fn hash_bytes(k: &[u8]) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    let init = 0x9e37_79b9u32.wrapping_add(k.len() as u32).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init, init, init);
    let mut rest = k;
    while rest.len() >= 12 {
        a = a.wrapping_add(le_word(&rest[0..4]));
        b = b.wrapping_add(le_word(&rest[4..8]));
        c = c.wrapping_add(le_word(&rest[8..12]));
        mix(&mut a, &mut b, &mut c);
        rest = &rest[12..];
    }
    let n = rest.len();
    if n >= 9 {
        // The lowest byte of c is reserved for the length.
        c = c.wrapping_add(le_word(&rest[8..]) << 8);
    }
    if n >= 5 {
        b = b.wrapping_add(le_word(&rest[4..n.min(8)]));
    }
    if n >= 1 {
        a = a.wrapping_add(le_word(&rest[0..n.min(4)]));
    }
    final_mix(&mut a, &mut b, &mut c);
    c
}

/// `hash_bytes_uint32`.
fn hash_uint32(k: u32) -> u32 {
    let init = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (init.wrapping_add(k), init, init);
    final_mix(&mut a, &mut b, &mut c);
    c
}

fn murmurhash32(mut h: u32) -> u32 {
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^= h >> 16;
    h
}

/// `hashint8` (also `timestamp_hash`, `time_hash`): folds the high half into the low one.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn hash_int8(v: i64) -> u32 {
    let lo = v as u32;
    let hi = (v >> 32) as u32;
    hash_uint32(lo ^ if v >= 0 { hi } else { !hi })
}

/// `hashfloat8` (and `hashfloat4`, which widens first): ±0 hash to 0, every NaN alike.
fn hash_float8(f: f64) -> u32 {
    if f == 0.0 {
        return 0;
    }
    let f = if f.is_nan() { f64::NAN } else { f };
    hash_bytes(&f.to_le_bytes())
}

/// `hash_numeric`: the base-10000 digits without leading / trailing zeros, XOR the weight.
fn hash_numeric(n: &Numeric) -> u32 {
    let Numeric::Value { mantissa, scale } = n else { return 0 };
    let text = mantissa.to_string();
    let digits = text.trim_start_matches('-');
    let scale = *scale as usize;
    let (int_part, frac_part) = if digits.len() > scale {
        (digits[..digits.len() - scale].to_string(), digits[digits.len() - scale..].to_string())
    } else {
        (String::new(), format!("{}{digits}", "0".repeat(scale - digits.len())))
    };
    let int_part = format!("{}{int_part}", "0".repeat((4 - int_part.len() % 4) % 4));
    let frac_part = format!("{frac_part}{}", "0".repeat((4 - frac_part.len() % 4) % 4));
    let groups: Vec<i16> = int_part
        .as_bytes()
        .chunks(4)
        .chain(frac_part.as_bytes().chunks(4))
        .map(|c| c.iter().fold(0i16, |acc, d| acc * 10 + i16::from(d - b'0')))
        .collect();
    let Some(first) = groups.iter().position(|d| *d != 0) else { return u32::MAX };
    let weight = i64::try_from(int_part.len() / 4).unwrap_or(0) - 1 - i64::try_from(first).unwrap_or(0);
    let last = groups.iter().rposition(|d| *d != 0).unwrap_or(first);
    let bytes: Vec<u8> = groups[first..=last].iter().flat_map(|d| d.to_le_bytes()).collect();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let w = weight as u32;
    hash_bytes(&bytes) ^ w
}

/// The hash Postgres' default hash opclass gives a non-NULL datum; `None` for types whose
/// hash pgmem does not reproduce (or cannot: enum labels hash by their OID).
#[allow(clippy::cast_sign_loss)]
fn hash_datum(v: &Value) -> Option<u32> {
    Some(match v {
        Value::Bool(b) => hash_uint32(u32::from(*b)),
        Value::Int2(i) => hash_uint32(i32::from(*i) as u32),
        Value::Int4(i) | Value::Date(i) => hash_uint32(*i as u32),
        Value::Oid(o) => hash_uint32(*o),
        Value::Int8(i) | Value::Time(i) | Value::Timestamp(i) | Value::Timestamptz(i) => hash_int8(*i),
        Value::Float4(f) => hash_float8(f64::from(*f)),
        Value::Float8(f) => hash_float8(*f),
        Value::Numeric(n) => hash_numeric(n),
        // `hashtext` under a deterministic collation hashes the bytes. A `character(n)` value
        // (also a `Text`) hashes without its trailing blanks, so those are left unordered.
        Value::Text(s) if !s.ends_with(' ') => hash_bytes(s.as_bytes()),
        Value::Bytea(b) => hash_bytes(b),
        Value::Uuid(u) => hash_bytes(u),
        _ => return None,
    })
}

/// The hash of a row of grouping columns (`ExecBuildHash32FromAttrs`, then
/// `TupleHashTableHash_internal`'s final murmur mix). NULLs contribute nothing.
fn hash_row(row: &[Value]) -> Option<u32> {
    let mut h = 0u32;
    for v in row {
        h = h.rotate_left(1);
        if !v.is_null() {
            h ^= hash_datum(v)?;
        }
    }
    Some(murmurhash32(h))
}

// ---------------------------------------------------------------------------------------
// lib/simplehash.h: open addressing, linear probing, robin-hood insertion.

const FILLFACTOR: f64 = 0.9;
const GROW_MAX_DIB: usize = 25;
const GROW_MAX_MOVE: usize = 150;
const GROW_MIN_FILLFACTOR: f64 = 0.1;

struct SimpleHash {
    /// Each bucket: the entry's hash and its index in `keys`.
    buckets: Vec<Option<(u32, usize)>>,
    members: usize,
    grow_threshold: usize,
    keys: Vec<Vec<GroupKey>>,
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
impl SimpleHash {
    /// `SH_CREATE` for `nelements` expected entries.
    fn new(nelements: usize) -> SimpleHash {
        let mut t = SimpleHash { buckets: Vec::new(), members: 0, grow_threshold: 0, keys: Vec::new() };
        t.set_size(((nelements.max(1) as f64) / FILLFACTOR) as usize);
        t
    }

    /// `SH_COMPUTE_PARAMETERS`.
    fn set_size(&mut self, size: usize) {
        let size = size.max(2).next_power_of_two();
        self.buckets = vec![None; size];
        self.grow_threshold = (size as f64 * FILLFACTOR) as usize;
    }

    fn mask(&self) -> usize {
        self.buckets.len() - 1
    }

    fn fill(&self) -> f64 {
        self.members as f64 / self.buckets.len() as f64
    }

    /// `SH_GROW` to twice the size.
    fn grow(&mut self) {
        let old = std::mem::take(&mut self.buckets);
        let old_size = old.len();
        self.set_size(old_size * 2);
        let mask = self.mask();
        // Start at a bucket that is empty or holds an entry at its optimal position, so
        // entries move over without conflicts.
        let start = (0..old_size)
            .find(|&i| match old[i] {
                None => true,
                Some((h, _)) => (h as usize) & (old_size - 1) == i,
            })
            .unwrap_or(0);
        for j in 0..old_size {
            if let Some((h, idx)) = old[(start + j) % old_size] {
                let mut cur = (h as usize) & mask;
                while self.buckets[cur].is_some() {
                    cur = (cur + 1) & mask;
                }
                self.buckets[cur] = Some((h, idx));
            }
        }
    }

    /// `SH_INSERT`: the entry index of `key` (a new one when it was not there).
    fn insert(&mut self, hash: u32, key: Vec<GroupKey>) -> usize {
        'restart: loop {
            if self.members >= self.grow_threshold {
                self.grow();
            }
            let mask = self.mask();
            let mut cur = (hash as usize) & mask;
            let mut insertdist = 0usize;
            loop {
                match self.buckets[cur] {
                    None => return self.place(cur, hash, key),
                    Some((h, idx)) if h == hash && self.keys[idx] == key => return idx,
                    Some((h, _)) => {
                        let curdist = cur.wrapping_sub((h as usize) & mask) & mask;
                        if insertdist > curdist {
                            // Robin hood: shift the run forward by one and take this bucket.
                            let mut empty = cur;
                            let mut emptydist = 0usize;
                            loop {
                                empty = (empty + 1) & mask;
                                if self.buckets[empty].is_none() {
                                    break;
                                }
                                emptydist += 1;
                                if emptydist > GROW_MAX_MOVE && self.fill() >= GROW_MIN_FILLFACTOR {
                                    self.grow_threshold = 0;
                                    continue 'restart;
                                }
                            }
                            while empty != cur {
                                let prev = empty.wrapping_sub(1) & mask;
                                self.buckets[empty] = self.buckets[prev];
                                empty = prev;
                            }
                            return self.place(cur, hash, key);
                        }
                    }
                }
                cur = (cur + 1) & mask;
                insertdist += 1;
                if insertdist > GROW_MAX_DIB && self.fill() >= GROW_MIN_FILLFACTOR {
                    self.grow_threshold = 0;
                    continue 'restart;
                }
            }
        }
    }

    fn place(&mut self, bucket: usize, hash: u32, key: Vec<GroupKey>) -> usize {
        let idx = self.keys.len();
        self.keys.push(key);
        self.buckets[bucket] = Some((hash, idx));
        self.members += 1;
        idx
    }

    /// `SH_START_ITERATE` / `SH_ITERATE`: backwards from the first empty bucket.
    fn iterate(&self) -> Vec<usize> {
        let size = self.buckets.len();
        let start = self.buckets.iter().position(Option::is_none).unwrap_or(0);
        (0..size).filter_map(|i| self.buckets[(start + size - i) % size].map(|(_, idx)| idx)).collect()
    }
}

fn key(r: &[Value]) -> Vec<GroupKey> {
    r.iter().map(Value::group_key).collect()
}

/// Groups `rows` through a table sized for `ngroups` groups: each group's first row index
/// and row count, in the table's iteration order. `None` when a value cannot be hashed.
fn hash_groups(rows: &[Row], ngroups: usize) -> Option<Vec<(usize, usize)>> {
    let mut table = SimpleHash::new(ngroups);
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let idx = table.insert(hash_row(r)?, key(r));
        if idx == groups.len() {
            groups.push((i, 0));
        }
        groups[idx].1 += 1;
    }
    Some(table.iterate().into_iter().map(|idx| groups[idx]).collect())
}

// ---------------------------------------------------------------------------------------
// Planner estimates (default cost settings).

const CPU_OPERATOR_COST: f64 = 0.0025;
const CPU_TUPLE_COST: f64 = 0.01;
/// Beyond this many rows hash tables may spill or be capped by `hash_mem`; not modelled.
const MAX_ROWS: f64 = 50_000.0;

/// The sort order a path's output is known to have.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Keys {
    None,
    /// Constant columns: any order holds, but the path carries no pathkeys, so a sorted
    /// SetOp over it (whose pathkeys are its left input's) is unsorted for its parent.
    Const,
    /// Sorted on all columns.
    Real,
}

#[derive(Clone, Copy, Debug)]
struct Path {
    startup: f64,
    total: f64,
    keys: Keys,
}

impl Path {
    fn is_sorted(&self) -> bool {
        self.keys != Keys::None
    }
}

/// `cost_sort` (in memory) over `input` producing `rows` rows.
fn sort_path(input: Path, rows: f64) -> Path {
    let n = rows.max(2.0);
    let startup = input.total + 2.0 * CPU_OPERATOR_COST * n * n.log2();
    Path { startup, total: startup + CPU_OPERATOR_COST * n, keys: Keys::Real }
}

/// `compare_path_costs_fuzzily(new, old)` with set operations' tuple_fraction 0 (startup
/// cost only breaks ties on total cost): `Less` when `new` is better, `Greater` when `old` is.
fn fuzzy_cmp(new: Path, old: Path, fuzz: f64) -> Ordering {
    if new.total > old.total * fuzz {
        Ordering::Greater
    } else if old.total > new.total * fuzz {
        Ordering::Less
    } else if new.startup > old.startup * fuzz {
        Ordering::Greater
    } else if old.startup > new.startup * fuzz {
        Ordering::Less
    } else {
        Ordering::Equal
    }
}

/// The cheapest surviving path after `add_path(hashed)`, `add_path(sorted)` and
/// `set_cheapest`, and whether it is the sorted one.
fn choose(hashed: Path, sorted: Path) -> (Path, bool) {
    let cmp = fuzzy_cmp(sorted, hashed, 1.01);
    let sorted_wins = if sorted.keys == Keys::Real {
        // Better pathkeys: a sorted path fuzzily as cheap removes the hashed one; otherwise
        // both survive and the cheaper total (then startup) wins.
        cmp != Ordering::Greater || sorted.total < hashed.total || (sorted.total == hashed.total && sorted.startup < hashed.startup)
    } else {
        // Same (no) pathkeys: only one survives.
        cmp == Ordering::Less || (cmp == Ordering::Equal && fuzzy_cmp(sorted, hashed, 1.000_000_000_1) == Ordering::Less)
    };
    if sorted_wins { (sorted, true) } else { (hashed, false) }
}

/// How a node deduplicates (or matches) its rows.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Strategy {
    /// Through a hash table sized for this many groups (built from the right input when
    /// `swap`: Postgres hashes the INTERSECT input with fewer groups).
    Hash {
        groups: usize,
        swap: bool,
    },
    Sort,
}

#[derive(Clone, Copy, Debug)]
struct Est {
    rows: f64,
    /// Estimated distinct groups, as a parent INTERSECT / EXCEPT sees this input.
    groups: f64,
    cheapest: Path,
    /// The cheapest path sorted on all columns.
    sorted: Path,
    strategy: Option<Strategy>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Col {
    Const,
    /// A column of a multi-row VALUES list, with its estimated number of distinct values.
    Var(f64),
}

/// True for an expression Postgres folds to a constant at plan time: a literal, a cast of
/// one, or a function of constants (`-1::int8` is `-(1::int8)`). Functions with side effects
/// or a changing result are not folded.
fn const_expr(e: &Expr) -> bool {
    const NOT_FOLDED: [&str; 7] = ["nextval", "setval", "currval", "lastval", "random", "gen_random", "txid"];
    match e {
        Expr::Const(_) => true,
        Expr::Cast { expr, .. } => const_expr(expr),
        Expr::Call { func, args } => {
            !args.is_empty() && !NOT_FOLDED.iter().any(|p| func.name.starts_with(p)) && args.iter().all(const_expr)
        }
        _ => false,
    }
}

fn bool_expr(e: &Expr) -> bool {
    match e {
        Expr::Const(v) => matches!(v, Value::Bool(_)),
        Expr::Cast { to, .. } => *to == Type::Bool,
        _ => false,
    }
}

/// A SELECT of constants or over a VALUES list: rows, the cost of producing them, columns.
fn leaf(rel: &Rel) -> Option<(f64, f64, Vec<Col>)> {
    match rel {
        Rel::Values { rows } => {
            let width = rows.first().map_or(0, Vec::len);
            if rows.is_empty() || !rows.iter().flatten().all(const_expr) {
                return None;
            }
            if rows.len() == 1 {
                // A single-row VALUES (or FROM-less SELECT) becomes a Result of constants.
                return Some((1.0, CPU_TUPLE_COST, vec![Col::Const; width]));
            }
            #[allow(clippy::cast_precision_loss)]
            let m = rows.len() as f64;
            if m > MAX_ROWS {
                return None;
            }
            // Without statistics a VALUES column has as many distinct values as rows,
            // except a boolean one (two).
            let cols = (0..width).map(|c| Col::Var(if rows.iter().any(|r| bool_expr(&r[c])) { 2.0 } else { m })).collect();
            Some((m, (CPU_OPERATOR_COST + CPU_TUPLE_COST) * m, cols))
        }
        Rel::Project { input, exprs } => {
            let (rows, cost, cols) = leaf(input)?;
            let out = exprs
                .iter()
                .map(|e| match e {
                    Expr::Col(j) => cols.get(*j).copied(),
                    _ if const_expr(e) => Some(Col::Const),
                    // A cast of a constant input column folds to a constant too.
                    Expr::Cast { expr, .. } => match expr.as_ref() {
                        Expr::Col(j) if cols.get(*j) == Some(&Col::Const) => Some(Col::Const),
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Some((rows, cost, out))
        }
        _ => None,
    }
}

fn leaf_est(rel: &Rel) -> Option<Est> {
    let (rows, cost, cols) = leaf(rel)?;
    let all_const = cols.iter().all(|c| *c == Col::Const);
    // estimate_num_groups: the product of the columns' distinct counts, at most the rows.
    let product: f64 = cols.iter().map(|c| if let Col::Var(nd) = c { *nd } else { 1.0 }).product();
    let groups = product.min(rows).ceil().max(1.0);
    let cheapest = Path { startup: 0.0, total: cost, keys: if all_const { Keys::Const } else { Keys::None } };
    let sorted = if all_const { cheapest } else { sort_path(cheapest, rows) };
    Some(Est { rows, groups, cheapest, sorted, strategy: None })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn strategy_of(sort: bool, groups: f64, swap: bool) -> Strategy {
    if sort { Strategy::Sort } else { Strategy::Hash { groups: groups as usize, swap } }
}

/// SELECT DISTINCT (no ON) over a constant SELECT or a VALUES list (`create_distinct_paths`).
fn distinct_est(input: &Rel) -> Option<Est> {
    let (_, _, cols) = leaf(input)?;
    let inp = leaf_est(input)?;
    if cols.iter().all(|c| *c == Col::Const) {
        // Every row is the same: Postgres keeps the first (a LIMIT 1 plan).
        return Some(Est { rows: 1.0, groups: 1.0, ..inp });
    }
    // Hashing groups by every column; the sort skips the constant ones.
    #[allow(clippy::cast_precision_loss)]
    let (k_hash, k_sort) = (cols.len() as f64, cols.iter().filter(|c| **c != Col::Const).count() as f64);
    let (n, g) = (inp.rows, inp.groups);
    let start = inp.cheapest.total + CPU_OPERATOR_COST * k_hash * n;
    let hashed = Path { startup: start, total: start + CPU_TUPLE_COST * g, keys: Keys::None };
    let s = sort_path(inp.cheapest, n);
    let sorted = Path { total: s.total + CPU_OPERATOR_COST * k_sort * n, ..s };
    let (cheapest, sort) = choose(hashed, sorted);
    Some(Est { rows: g, groups: g, cheapest, sorted, strategy: Some(strategy_of(sort, g, false)) })
}

/// The inputs a UNION reads in one Append: nested UNIONs are flattened into it when they
/// are UNION ALL or match the top one's ALL (`recurse_union_children`).
fn union_children<'a>(rel: &'a Rel, top_all: bool, out: &mut Vec<&'a Rel>) {
    if let Rel::SetOp { op: SetOpKind::Union, left, right, .. } = rel {
        for child in [left, right] {
            match child.as_ref() {
                Rel::SetOp { op: SetOpKind::Union, all, .. } if *all == top_all || *all => union_children(child, top_all, out),
                other => out.push(other),
            }
        }
    }
}

/// The number of output columns.
#[allow(clippy::cast_precision_loss)]
fn width(rel: &Rel) -> Option<f64> {
    match rel {
        Rel::SetOp { left, .. } => width(left),
        Rel::Distinct { input, .. } => width(input),
        other => leaf(other).map(|(_, _, cols)| cols.len() as f64),
    }
}

fn est(rel: &Rel) -> Option<Est> {
    match rel {
        Rel::SetOp { op: SetOpKind::Union, all, .. } => {
            let mut children = Vec::new();
            union_children(rel, *all, &mut children);
            let ests = children.into_iter().map(est).collect::<Option<Vec<_>>>()?;
            let n: f64 = ests.iter().map(|e| e.rows).sum();
            if n > MAX_ROWS {
                return None;
            }
            let k = width(rel)?;
            let append = Path {
                startup: ests.first()?.cheapest.startup,
                total: ests.iter().map(|e| e.cheapest.total).sum::<f64>() + CPU_TUPLE_COST * 0.5 * n,
                keys: Keys::None,
            };
            if *all {
                return Some(Est { rows: n, groups: n, cheapest: append, sorted: sort_path(append, n), strategy: None });
            }
            // Postgres takes every input row to be distinct.
            let g = n;
            let start = append.total + CPU_OPERATOR_COST * k * n;
            let hashed = Path { startup: start, total: start + CPU_TUPLE_COST * g, keys: Keys::None };
            let s = sort_path(append, n);
            let sorted = Path { total: s.total + CPU_OPERATOR_COST * k * n, ..s };
            let (cheapest, sort) = choose(hashed, sorted);
            Some(Est { rows: g, groups: g, cheapest, sorted, strategy: Some(strategy_of(sort, g, false)) })
        }
        Rel::SetOp { op, all, left, right } => {
            let (mut l, mut r) = (est(left)?, est(right)?);
            let swap = *op == SetOpKind::Intersect && l.groups > r.groups;
            if swap {
                std::mem::swap(&mut l, &mut r);
            }
            let k = width(rel)?;
            let g = l.groups;
            let out = match (op, all) {
                (SetOpKind::Except, true) => l.rows,
                (_, true) => l.rows.min(r.rows),
                _ => g,
            };
            let compare = CPU_OPERATOR_COST * (l.rows + r.rows) * k;
            let start = l.cheapest.total + r.cheapest.total + compare;
            let hashed = Path { startup: start, total: start + CPU_OPERATOR_COST * out, keys: Keys::None };
            let ls = if l.cheapest.is_sorted() { l.cheapest } else { l.sorted };
            let rs = if r.cheapest.is_sorted() { r.cheapest } else { r.sorted };
            // A sorted SetOp's output has its left input's pathkeys.
            let keys = if ls.keys == Keys::Real { Keys::Real } else { Keys::None };
            let sorted = Path { startup: ls.startup + rs.startup, total: ls.total + rs.total + compare + CPU_OPERATOR_COST * out, keys };
            let (cheapest, sort) = choose(hashed, sorted);
            let sorted = if keys == Keys::Real { sorted } else { sort_path(cheapest, out) };
            Some(Est { rows: out, groups: out, cheapest, sorted, strategy: Some(strategy_of(sort, g, swap)) })
        }
        Rel::Distinct { input, on: None } => distinct_est(input),
        other => leaf_est(other),
    }
}

// ---------------------------------------------------------------------------------------
// Execution.

/// Distinct rows of `rows` with their counts, in first-seen order.
fn first_seen(rows: Vec<Row>) -> Vec<(Row, usize)> {
    let mut index: HashMap<Vec<GroupKey>, usize> = HashMap::new();
    let mut out: Vec<(Row, usize)> = Vec::new();
    for r in rows {
        match index.entry(key(&r)) {
            std::collections::hash_map::Entry::Occupied(e) => out[*e.get()].1 += 1,
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(out.len());
                out.push((r, 1));
            }
        }
    }
    out
}

/// `groups` in sort order: ascending, NULLs last, column by column.
fn sort_groups(groups: &mut [(Row, usize)]) -> bool {
    let mut ok = true;
    groups.sort_by(|(a, _), (b, _)| {
        for (x, y) in a.iter().zip(b) {
            match compare_sort(x, y, false, false) {
                Ok(Ordering::Equal) => {}
                Ok(o) => return o,
                Err(_) => ok = false,
            }
        }
        Ordering::Equal
    });
    ok
}

/// Distinct rows of `rows` with their counts, in the order the strategy emits them.
fn grouped(rows: Vec<Row>, strategy: Option<Strategy>) -> Vec<(Row, usize)> {
    match strategy {
        Some(Strategy::Hash { groups, .. }) => {
            if let Some(order) = hash_groups(&rows, groups) {
                return order.into_iter().map(|(i, n)| (rows[i].clone(), n)).collect();
            }
            first_seen(rows)
        }
        Some(Strategy::Sort) => {
            let unsorted = first_seen(rows);
            let mut sorted = unsorted.clone();
            if sort_groups(&mut sorted) { sorted } else { unsorted }
        }
        None => first_seen(rows),
    }
}

/// SELECT DISTINCT (no ON) of `rows`, read from `input`: in Postgres' order where known.
pub fn distinct(input: &Rel, rows: Vec<Row>) -> Vec<Row> {
    let strategy = distinct_est(input).and_then(|e| e.strategy);
    grouped(rows, strategy).into_iter().map(|(r, _)| r).collect()
}

/// UNION / INTERSECT / EXCEPT (`rel` is a `Rel::SetOp`).
pub fn set_op(rel: &Rel, ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let Rel::SetOp { op, all, left, right } = rel else { return run(rel, ctx) };
    let strategy = est(rel).and_then(|e| e.strategy);
    if *op == SetOpKind::Union {
        let mut children = Vec::new();
        union_children(rel, *all, &mut children);
        let mut rows = Vec::new();
        for c in children {
            rows.extend(run(c, ctx)?);
        }
        if *all {
            return Ok(rows);
        }
        return Ok(grouped(rows, strategy).into_iter().map(|(r, _)| r).collect());
    }
    let (mut l, mut r) = (run(left, ctx)?, run(right, ctx)?);
    if let Some(Strategy::Hash { swap: true, .. }) = strategy {
        std::mem::swap(&mut l, &mut r);
    }
    let mut right_counts: HashMap<Vec<GroupKey>, usize> = HashMap::new();
    for row in &r {
        *right_counts.entry(key(row)).or_default() += 1;
    }
    let mut out = Vec::new();
    for (row, nl) in grouped(l, strategy) {
        let nr = right_counts.get(&key(&row)).copied().unwrap_or(0);
        let copies = match (op, all) {
            (SetOpKind::Intersect, true) => nl.min(nr),
            (SetOpKind::Intersect, false) => usize::from(nr > 0),
            (_, true) => nl.saturating_sub(nr),
            (_, false) => usize::from(nr == 0),
        };
        out.extend(std::iter::repeat_n(row, copies));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::cast_sign_loss)]
    fn h(v: i32) -> u32 {
        v as u32
    }

    // Reference values from PostgreSQL 18 (`SELECT hashint4(1)`, `hash_numeric(1.5)`, ...).
    #[test]
    fn hashes_match_postgres() {
        assert_eq!(hash_datum(&Value::Int4(1)), Some(h(-1_905_060_026)));
        assert_eq!(hash_datum(&Value::Int4(0)), Some(h(-272_711_505)));
        assert_eq!(hash_datum(&Value::Bool(true)), Some(h(-1_905_060_026)));
        assert_eq!(hash_datum(&Value::Int2(5)), Some(h(-1_330_264_708)));
        assert_eq!(hash_datum(&Value::Int8(-5)), Some(h(1_412_886_042)));
        assert_eq!(hash_datum(&Value::Int8(5_000_000_000)), Some(h(-694_934_712)));
        assert_eq!(hash_datum(&Value::Text("abc".into())), Some(h(-785_388_649)));
        assert_eq!(hash_datum(&Value::Text(String::new())), Some(h(-1_477_818_771)));
        assert_eq!(hash_datum(&Value::Text("hello world, longer text!".into())), Some(h(-1_500_234_796)));
        assert_eq!(hash_datum(&Value::Float8(1.5)), Some(h(630_860_146)));
        assert_eq!(hash_datum(&Value::Float4(1.5)), Some(h(630_860_146)));
        assert_eq!(hash_datum(&Value::Date(8766)), Some(h(-1_661_621_463)));
        let num = |s: &str| Value::Numeric(Numeric::parse(s).expect("numeric"));
        assert_eq!(hash_datum(&num("1.5")), Some(h(692_967_894)));
        assert_eq!(hash_datum(&num("10000")), Some(h(1_324_868_425)));
        assert_eq!(hash_datum(&num("0.0001")), Some(h(-1_324_868_425)));
        assert_eq!(hash_datum(&num("-12345.678")), Some(h(573_919_094)));
        assert_eq!(hash_datum(&num("0")), Some(h(-1)));
    }

    #[test]
    fn hash_table_iterates_like_simplehash() {
        // SELECT 1 UNION SELECT 2 ... UNION SELECT 5 → 1, 3, 5, 4, 2 in Postgres 18.
        let rows: Vec<Row> = (1..=5).map(|i| vec![Value::Int4(i)]).collect();
        let order: Vec<usize> = hash_groups(&rows, 5).expect("hashable").into_iter().map(|(i, _)| i).collect();
        assert_eq!(order, vec![0, 2, 4, 3, 1]);
    }
}
