//! Binary-format send/receive functions (`typsend` / `typreceive`).

use crate::datetime::Interval;
use crate::json::Jsonb;
use crate::{SqlError, SqlResult, Type, Value, code};

fn bad(ty: &Type, why: &str) -> SqlError {
    SqlError::new(code::INVALID_BINARY_REPRESENTATION, format!("incorrect binary data format for type {}: {why}", ty.name()))
}

/// Encodes a non-null value in binary format for `ty`. Integer and float values are converted
/// to the declared width (the binder guarantees the value fits).
pub fn encode(v: &Value, ty: &Type) -> SqlResult<Vec<u8>> {
    let mismatch = || SqlError::internal(format!("cannot binary-encode {} as {}", v.kind(), ty.name()));
    Ok(match ty {
        Type::Xid | Type::Cid => {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = v.as_i64().ok_or_else(mismatch)? as u32;
            n.to_be_bytes().to_vec()
        }
        Type::Tid => match v {
            Value::Tid(block, item) => {
                let mut out = block.to_be_bytes().to_vec();
                out.extend_from_slice(&item.to_be_bytes());
                out
            }
            _ => return Err(mismatch()),
        },
        Type::Bool => vec![u8::from(v.as_bool().ok_or_else(mismatch)?)],
        Type::Int2 => {
            let n = v.as_i64().ok_or_else(mismatch)?;
            i16::try_from(n).map_err(|_| SqlError::out_of_range("smallint"))?.to_be_bytes().to_vec()
        }
        Type::Int4 => {
            let n = v.as_i64().ok_or_else(mismatch)?;
            i32::try_from(n).map_err(|_| SqlError::out_of_range("integer"))?.to_be_bytes().to_vec()
        }
        Type::Int8 => v.as_i64().ok_or_else(mismatch)?.to_be_bytes().to_vec(),
        #[allow(clippy::cast_possible_truncation)]
        Type::Float4 => (v.as_f64().ok_or_else(mismatch)? as f32).to_be_bytes().to_vec(),
        Type::Float8 => v.as_f64().ok_or_else(mismatch)?.to_be_bytes().to_vec(),
        Type::Numeric => match v {
            Value::Numeric(n) => n.to_binary(),
            other => match other.as_i64() {
                Some(i) => crate::Numeric::from_i64(i).to_binary(),
                None => return Err(mismatch()),
            },
        },
        Type::Text | Type::Unknown | Type::Varchar | Type::Bpchar | Type::Name | Type::Char | Type::Json | Type::Void => match v {
            Value::Text(s) | Value::Json(s) => s.as_bytes().to_vec(),
            other => crate::text::encode(other, &other.natural_type())?.into_bytes(),
        },
        Type::Bytea => match v {
            Value::Bytea(b) => b.clone(),
            _ => return Err(mismatch()),
        },
        Type::Uuid => match v {
            Value::Uuid(u) => u.to_vec(),
            _ => return Err(mismatch()),
        },
        Type::Timestamptz | Type::Timestamp | Type::Time => match v {
            Value::Timestamptz(t) | Value::Timestamp(t) | Value::Time(t) => t.to_be_bytes().to_vec(),
            _ => return Err(mismatch()),
        },
        Type::Date => match v {
            Value::Date(d) => d.to_be_bytes().to_vec(),
            _ => return Err(mismatch()),
        },
        Type::TimeTz => match v {
            Value::TimeTz(t, z) => {
                let mut out = t.to_be_bytes().to_vec();
                // On the wire the zone is seconds *west* of UTC.
                out.extend_from_slice(&(-z).to_be_bytes());
                out
            }
            _ => return Err(mismatch()),
        },
        Type::Interval => match v {
            Value::Interval(iv) => {
                let mut out = Vec::with_capacity(16);
                out.extend_from_slice(&iv.micros.to_be_bytes());
                out.extend_from_slice(&iv.days.to_be_bytes());
                out.extend_from_slice(&iv.months.to_be_bytes());
                out
            }
            _ => return Err(mismatch()),
        },
        Type::Jsonb => match v {
            Value::Jsonb(j) => {
                let mut out = vec![1u8];
                out.extend_from_slice(j.to_string().as_bytes());
                out
            }
            _ => return Err(mismatch()),
        },
        Type::JsonPath => {
            let mut out = vec![1u8];
            out.extend_from_slice(crate::text::encode(v, ty)?.as_bytes());
            out
        }
        Type::Oid | Type::Reg(_) => match v {
            Value::Oid(o) | Value::Reg(_, o, _) => o.to_be_bytes().to_vec(),
            _ => return Err(mismatch()),
        },
        Type::Int2Vector | Type::OidVector => match v {
            Value::Array(items) => encode_array(items, ty.element().unwrap_or(&Type::Int2), &[])?,
            _ => return Err(mismatch()),
        },
        Type::Array(elem) => match v {
            Value::Array(items) => encode_array(items, elem, &[])?,
            Value::BoundedArray(lb, items) => encode_array(items, elem, lb)?,
            _ => return Err(mismatch()),
        },
        // enum_send is the label's text.
        Type::Enum(_) => match v {
            Value::Enum(t, label) => crate::usertype::enum_label(*t, *label)?.into_bytes(),
            _ => return Err(mismatch()),
        },
        Type::Domain(_) => encode(v, &ty.base())?,
        Type::Composite(_) | Type::Record => match v {
            Value::Record(t, fields) => encode_record(*t, fields)?,
            _ => return Err(mismatch()),
        },
        Type::Bit | Type::VarBit => match v {
            Value::Bit(b) => crate::netbits::bits_to_binary(b),
            _ => return Err(mismatch()),
        },
        Type::Money => match v {
            Value::Money(m) => m.to_be_bytes().to_vec(),
            _ => return Err(mismatch()),
        },
        Type::Inet | Type::Cidr => match v {
            Value::Inet(ip, bits) => crate::netbits::inet_to_binary(ip, *bits, *ty == Type::Cidr),
            _ => return Err(mismatch()),
        },
        Type::MacAddr | Type::MacAddr8 => match v {
            Value::MacAddr(b) => b.clone(),
            _ => return Err(mismatch()),
        },
        Type::Range(sub) => match v {
            Value::Range(r) => encode_range(r, sub)?,
            _ => return Err(mismatch()),
        },
        Type::Multirange(sub) => match v {
            Value::Multirange(_, rs) => {
                let mut out = u32::try_from(rs.len()).unwrap_or(u32::MAX).to_be_bytes().to_vec();
                for r in rs {
                    let body = encode_range(r, sub)?;
                    out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_be_bytes());
                    out.extend_from_slice(&body);
                }
                out
            }
            _ => return Err(mismatch()),
        },
    })
}

const RANGE_EMPTY: u8 = 0x01;
const RANGE_LB_INC: u8 = 0x02;
const RANGE_UB_INC: u8 = 0x04;
const RANGE_LB_INF: u8 = 0x08;
const RANGE_UB_INF: u8 = 0x10;

/// `range_send`: flags, then each finite bound as a length-prefixed subtype value.
fn encode_range(r: &crate::range::Range, sub: &Type) -> SqlResult<Vec<u8>> {
    if r.empty {
        return Ok(vec![RANGE_EMPTY]);
    }
    let mut flags = 0;
    if r.lower_inc {
        flags |= RANGE_LB_INC;
    }
    if r.upper_inc {
        flags |= RANGE_UB_INC;
    }
    if r.lower.is_none() {
        flags |= RANGE_LB_INF;
    }
    if r.upper.is_none() {
        flags |= RANGE_UB_INF;
    }
    let mut out = vec![flags];
    for b in [&r.lower, &r.upper].into_iter().flatten() {
        let body = encode(b, sub)?;
        out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&body);
    }
    Ok(out)
}

fn decode_range(b: &[u8], sub: &Type, ty: &Type) -> SqlResult<crate::range::Range> {
    let (flags, mut rest) = b.split_first().ok_or_else(|| bad(ty, "empty"))?;
    if flags & RANGE_EMPTY != 0 {
        return Ok(crate::range::Range::empty(sub));
    }
    let mut bound = |inf: u8| -> SqlResult<Option<Value>> {
        if flags & inf != 0 {
            return Ok(None);
        }
        let len = rest.get(..4).ok_or_else(|| bad(ty, "short"))?;
        let n = u32::from_be_bytes(len.try_into().map_err(|_| bad(ty, "short"))?) as usize;
        let body = rest.get(4..4 + n).ok_or_else(|| bad(ty, "short"))?;
        rest = &rest[4 + n..];
        decode(body, sub).map(Some)
    };
    let lower = bound(RANGE_LB_INF)?;
    let upper = bound(RANGE_UB_INF)?;
    crate::range::make(sub, lower, upper, flags & RANGE_LB_INC != 0, flags & RANGE_UB_INC != 0)
}

/// `record_send`: the column count, then each column's type OID, length (-1 for NULL) and
/// binary value.
fn encode_record(type_oid: u32, fields: &[Value]) -> SqlResult<Vec<u8>> {
    let declared = crate::usertype::lookup(type_oid);
    let mut out = Vec::new();
    put_i32(&mut out, i32::try_from(fields.len()).map_err(|_| SqlError::internal("record too wide"))?);
    for (n, v) in fields.iter().enumerate() {
        let ty = declared.as_ref().and_then(|t| t.fields().get(n).map(|f| f.ty.clone())).unwrap_or_else(|| v.natural_type());
        // Like RowDescription, a domain column reports its base type.
        let ty = ty.base();
        out.extend_from_slice(&ty.oid().to_be_bytes());
        if v.is_null() {
            put_i32(&mut out, -1);
        } else {
            let b = encode(v, &ty)?;
            put_i32(&mut out, i32::try_from(b.len()).map_err(|_| SqlError::internal("field too large"))?);
            out.extend_from_slice(&b);
        }
    }
    Ok(out)
}

/// `record_recv` for a named composite type.
fn decode_record(b: &[u8], type_oid: u32, ty: &Type) -> SqlResult<Value> {
    let t = crate::usertype::lookup(type_oid).ok_or_else(|| SqlError::internal(format!("cache lookup failed for type {type_oid}")))?;
    let fields = t.fields();
    let mut r = Reader { b, pos: 0, ty };
    let n = usize::try_from(r.i32()?).map_err(|_| bad(ty, "negative column count"))?;
    if n != fields.len() {
        return Err(SqlError::datatype_mismatch(format!("wrong number of columns: {n}, expected {}", fields.len())));
    }
    let mut out = Vec::with_capacity(n);
    for f in fields {
        let _oid = r.i32()?;
        let len = r.i32()?;
        if len < 0 {
            out.push(Value::Null);
            continue;
        }
        let chunk = r.bytes(usize::try_from(len).map_err(|_| bad(ty, "bad length"))?)?;
        out.push(decode(chunk, &f.ty)?);
    }
    Ok(Value::Record(type_oid, out))
}

fn put_i32(out: &mut Vec<u8>, n: i32) {
    out.extend_from_slice(&n.to_be_bytes());
}

/// The dimensions of a (rectangular, nested) array value.
pub fn array_dims(items: &[Value]) -> Vec<usize> {
    let mut dims = vec![items.len()];
    let mut cur = items;
    while let Some(Value::Array(inner)) = cur.first() {
        dims.push(inner.len());
        cur = inner;
    }
    dims
}

/// The elements of a nested array in storage (row-major) order.
pub fn array_flatten(items: &[Value]) -> Vec<&Value> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Value::Array(inner) => out.extend(array_flatten(inner)),
            other => out.push(other),
        }
    }
    out
}

/// `array_send`: dimensions and lower bounds, then the elements in row-major order.
fn encode_array(items: &[Value], elem: &Type, lower: &[i32]) -> SqlResult<Vec<u8>> {
    let mut out = Vec::new();
    let flat = array_flatten(items);
    let has_null = flat.iter().any(|v| v.is_null());
    let elem_oid = i32::try_from(elem.oid()).map_err(|_| SqlError::internal("element oid overflow"))?;
    if flat.is_empty() {
        put_i32(&mut out, 0);
        put_i32(&mut out, 0);
        put_i32(&mut out, elem_oid);
        return Ok(out);
    }
    let dims = array_dims(items);
    put_i32(&mut out, i32::try_from(dims.len()).map_err(|_| SqlError::internal("too many dimensions"))?);
    put_i32(&mut out, i32::from(has_null));
    put_i32(&mut out, elem_oid);
    for (i, d) in dims.iter().enumerate() {
        put_i32(&mut out, i32::try_from(*d).map_err(|_| SqlError::internal("array too large"))?);
        put_i32(&mut out, lower.get(i).copied().unwrap_or(1));
    }
    for item in flat {
        if item.is_null() {
            put_i32(&mut out, -1);
        } else {
            let b = encode(item, elem)?;
            put_i32(&mut out, i32::try_from(b.len()).map_err(|_| SqlError::internal("element too large"))?);
            out.extend_from_slice(&b);
        }
    }
    Ok(out)
}

fn fixed<const N: usize>(b: &[u8], ty: &Type) -> SqlResult<[u8; N]> {
    b.try_into().map_err(|_| bad(ty, &format!("expected {N} bytes, got {}", b.len())))
}

/// Decodes a non-null binary parameter of type `ty`.
pub fn decode(b: &[u8], ty: &Type) -> SqlResult<Value> {
    Ok(match ty {
        Type::Bool => Value::Bool(fixed::<1>(b, ty)?[0] != 0),
        Type::Int2 => Value::Int2(i16::from_be_bytes(fixed(b, ty)?)),
        Type::Int4 => Value::Int4(i32::from_be_bytes(fixed(b, ty)?)),
        Type::Int8 => Value::Int8(i64::from_be_bytes(fixed(b, ty)?)),
        Type::Float4 => Value::Float4(f32::from_be_bytes(fixed(b, ty)?)),
        Type::Float8 => Value::Float8(f64::from_be_bytes(fixed(b, ty)?)),
        Type::Numeric => Value::Numeric(crate::Numeric::from_binary(b)?),
        Type::Text | Type::Unknown | Type::Varchar | Type::Bpchar | Type::Name | Type::Char | Type::Json | Type::Void => {
            let s = std::str::from_utf8(b)
                .map_err(|_| SqlError::new(code::INVALID_TEXT_REPRESENTATION, "invalid byte sequence for encoding \"UTF8\""))?;
            crate::text::decode(s, ty)?
        }
        Type::Oid => Value::Oid(u32::from_be_bytes(fixed(b, ty)?)),
        Type::Xid | Type::Cid => Value::Int8(i64::from(u32::from_be_bytes(fixed(b, ty)?))),
        Type::Tid => {
            let raw: [u8; 6] = fixed(b, ty)?;
            Value::Tid(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]), u16::from_be_bytes([raw[4], raw[5]]))
        }
        Type::Reg(k) => {
            let o = u32::from_be_bytes(fixed(b, ty)?);
            Value::Reg(*k, o, o.to_string())
        }
        Type::Bytea => Value::Bytea(b.to_vec()),
        Type::Uuid => Value::Uuid(fixed(b, ty)?),
        Type::Timestamptz => Value::Timestamptz(i64::from_be_bytes(fixed(b, ty)?)),
        Type::Timestamp => Value::Timestamp(i64::from_be_bytes(fixed(b, ty)?)),
        Type::Time => Value::Time(i64::from_be_bytes(fixed(b, ty)?)),
        Type::Date => Value::Date(i32::from_be_bytes(fixed(b, ty)?)),
        Type::TimeTz => {
            let raw: [u8; 12] = fixed(b, ty)?;
            let t = i64::from_be_bytes(raw[0..8].try_into().map_err(|_| bad(ty, "short"))?);
            let z = i32::from_be_bytes(raw[8..12].try_into().map_err(|_| bad(ty, "short"))?);
            Value::TimeTz(t, -z)
        }
        Type::Interval => {
            let raw: [u8; 16] = fixed(b, ty)?;
            let micros = i64::from_be_bytes(raw[0..8].try_into().map_err(|_| bad(ty, "short"))?);
            let days = i32::from_be_bytes(raw[8..12].try_into().map_err(|_| bad(ty, "short"))?);
            let months = i32::from_be_bytes(raw[12..16].try_into().map_err(|_| bad(ty, "short"))?);
            Value::Interval(Interval { months, days, micros })
        }
        Type::Jsonb => {
            let (version, rest) = b.split_first().ok_or_else(|| bad(ty, "empty"))?;
            if *version != 1 {
                return Err(SqlError::new(code::INVALID_BINARY_REPRESENTATION, format!("unsupported jsonb version number {version}")));
            }
            let text = std::str::from_utf8(rest).map_err(|_| bad(ty, "invalid UTF-8"))?;
            Value::Jsonb(Jsonb::parse(text)?)
        }
        Type::JsonPath => {
            let (version, rest) = b.split_first().ok_or_else(|| bad(ty, "empty"))?;
            if *version != 1 {
                return Err(SqlError::new(code::INVALID_BINARY_REPRESENTATION, format!("unsupported jsonpath version number: {version}")));
            }
            let text = std::str::from_utf8(rest).map_err(|_| bad(ty, "invalid UTF-8"))?;
            crate::text::decode(text, ty)?
        }
        Type::Array(elem) => {
            let (lower, items) = decode_array(b, elem, ty)?;
            Value::array_with_bounds(lower, items)
        }
        Type::Int2Vector | Type::OidVector => Value::Array(decode_array(b, ty.element().unwrap_or(&Type::Int2), ty)?.1),
        Type::Enum(oid) => {
            let s = std::str::from_utf8(b).map_err(|_| bad(ty, "invalid UTF-8"))?;
            crate::usertype::parse_enum(s, *oid)?
        }
        Type::Domain(_) => decode(b, &ty.base())?,
        Type::Composite(oid) => decode_record(b, *oid, ty)?,
        Type::Record => return Err(SqlError::feature_not_supported("input of anonymous composite types is not implemented")),
        Type::Range(sub) => Value::Range(Box::new(decode_range(b, sub, ty)?)),
        Type::Bit | Type::VarBit => Value::Bit(crate::netbits::bits_from_binary(b)?),
        Type::Money => Value::Money(i64::from_be_bytes(fixed(b, ty)?)),
        Type::Inet | Type::Cidr => {
            let (ip, bits) = crate::netbits::inet_from_binary(b)?;
            Value::Inet(ip, bits)
        }
        Type::MacAddr => Value::MacAddr(fixed::<6>(b, ty)?.to_vec()),
        Type::MacAddr8 => Value::MacAddr(fixed::<8>(b, ty)?.to_vec()),
        Type::Multirange(sub) => {
            let count = b.get(..4).ok_or_else(|| bad(ty, "short"))?;
            let count = u32::from_be_bytes(count.try_into().map_err(|_| bad(ty, "short"))?);
            let mut rest = &b[4..];
            let mut out = Vec::new();
            for _ in 0..count {
                let len = rest.get(..4).ok_or_else(|| bad(ty, "short"))?;
                let n = u32::from_be_bytes(len.try_into().map_err(|_| bad(ty, "short"))?) as usize;
                out.push(decode_range(rest.get(4..4 + n).ok_or_else(|| bad(ty, "short"))?, sub, ty)?);
                rest = &rest[4 + n..];
            }
            Value::Multirange((**sub).clone(), crate::range::normalize(out)?)
        }
    })
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
    ty: &'a Type,
}

impl Reader<'_> {
    fn i32(&mut self) -> SqlResult<i32> {
        let end = self.pos + 4;
        let chunk = self.b.get(self.pos..end).ok_or_else(|| bad(self.ty, "truncated"))?;
        self.pos = end;
        Ok(i32::from_be_bytes(chunk.try_into().map_err(|_| bad(self.ty, "truncated"))?))
    }

    fn bytes(&mut self, n: usize) -> SqlResult<&[u8]> {
        let end = self.pos + n;
        let chunk = self.b.get(self.pos..end).ok_or_else(|| bad(self.ty, "truncated"))?;
        self.pos = end;
        Ok(chunk)
    }
}

/// `array_recv`: the lower bounds and the (nested) elements.
fn decode_array(b: &[u8], elem: &Type, ty: &Type) -> SqlResult<(Vec<i32>, Vec<Value>)> {
    let mut r = Reader { b, pos: 0, ty };
    let ndim = usize::try_from(r.i32()?).map_err(|_| bad(ty, "negative dimension count"))?;
    let _has_null = r.i32()?;
    let _elem_oid = r.i32()?;
    if ndim == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    if ndim > 6 {
        return Err(SqlError::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!("number of array dimensions ({ndim}) exceeds the maximum allowed (6)"),
        ));
    }
    let mut dims = Vec::with_capacity(ndim);
    let mut lower = Vec::with_capacity(ndim);
    for _ in 0..ndim {
        dims.push(usize::try_from(r.i32()?).map_err(|_| bad(ty, "negative dimension"))?);
        lower.push(r.i32()?);
    }
    let total: usize = dims.iter().product();
    let mut flat = Vec::with_capacity(total);
    for _ in 0..total {
        let n = r.i32()?;
        if n < 0 {
            flat.push(Value::Null);
        } else {
            let chunk = r.bytes(usize::try_from(n).map_err(|_| bad(ty, "bad length"))?)?;
            flat.push(decode(chunk, elem)?);
        }
    }
    Ok((lower, nest(&mut flat.into_iter(), &dims)))
}

/// Rebuilds nested arrays from row-major elements.
fn nest(items: &mut impl Iterator<Item = Value>, dims: &[usize]) -> Vec<Value> {
    match dims {
        [] => Vec::new(),
        [n] => items.take(*n).collect(),
        [n, rest @ ..] => (0..*n).map(|_| Value::Array(nest(items, rest))).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let cases = vec![
            (Value::Int4(-7), Type::Int4),
            (Value::Int8(1 << 40), Type::Int8),
            (Value::Bool(true), Type::Bool),
            (Value::Float8(2.5), Type::Float8),
            (Value::Text("héllo".into()), Type::Text),
            (Value::Bytea(vec![0, 1, 2]), Type::Bytea),
            (Value::Uuid([7; 16]), Type::Uuid),
            (Value::Timestamptz(123_456), Type::Timestamptz),
            (Value::Interval(Interval::new(1, 2, 3)), Type::Interval),
            (Value::Array(vec![Value::Int8(1), Value::Null, Value::Int8(3)]), Type::array_of(Type::Int8)),
            (Value::Array(vec![]), Type::array_of(Type::Text)),
        ];
        for (v, ty) in cases {
            let bytes = encode(&v, &ty).expect("encode");
            assert_eq!(decode(&bytes, &ty).expect("decode"), v, "{ty}");
        }
    }

    #[test]
    fn ranges_round_trip_in_range_send_format() {
        let r = crate::range::make(&Type::Int4, Some(Value::Int4(1)), None, true, false).expect("range");
        let ty = Type::Range(Box::new(Type::Int4));
        let bytes = encode(&Value::Range(Box::new(r.clone())), &ty).expect("encode");
        // flags (lower inclusive, upper infinite), then the lower bound as int4.
        assert_eq!(bytes, [0x02 | 0x10, 0, 0, 0, 4, 0, 0, 0, 1]);
        assert_eq!(decode(&bytes, &ty).expect("decode"), Value::Range(Box::new(r.clone())));
        let empty = encode(&Value::Range(Box::new(crate::range::Range::empty(&Type::Int4))), &ty).expect("encode empty");
        assert_eq!(empty, [0x01]);
        let mty = Type::Multirange(Box::new(Type::Int4));
        let m = Value::Multirange(Type::Int4, vec![r]);
        assert_eq!(decode(&encode(&m, &mty).expect("encode"), &mty).expect("decode"), m);
    }

    #[test]
    fn jsonb_version_byte() {
        let v = Value::Jsonb(Jsonb::parse(r#"{"a":1}"#).expect("json"));
        let bytes = encode(&v, &Type::Jsonb).expect("encode");
        assert_eq!(bytes[0], 1);
        assert_eq!(&bytes[1..], br#"{"a": 1}"#);
    }
}
