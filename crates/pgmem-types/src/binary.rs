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
        Type::Text | Type::Unknown => match v {
            Value::Text(s) => s.as_bytes().to_vec(),
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
        Type::Array(elem) => match v {
            Value::Array(items) => encode_array(items, elem)?,
            _ => return Err(mismatch()),
        },
    })
}

fn put_i32(out: &mut Vec<u8>, n: i32) {
    out.extend_from_slice(&n.to_be_bytes());
}

fn encode_array(items: &[Value], elem: &Type) -> SqlResult<Vec<u8>> {
    let mut out = Vec::new();
    let has_null = items.iter().any(Value::is_null);
    let elem_oid = i32::try_from(elem.oid()).map_err(|_| SqlError::internal("element oid overflow"))?;
    if items.is_empty() {
        put_i32(&mut out, 0);
        put_i32(&mut out, 0);
        put_i32(&mut out, elem_oid);
        return Ok(out);
    }
    put_i32(&mut out, 1);
    put_i32(&mut out, i32::from(has_null));
    put_i32(&mut out, elem_oid);
    put_i32(&mut out, i32::try_from(items.len()).map_err(|_| SqlError::internal("array too large"))?);
    put_i32(&mut out, 1);
    for item in items {
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
        Type::Text | Type::Unknown => Value::Text(
            String::from_utf8(b.to_vec())
                .map_err(|_| SqlError::new(code::INVALID_TEXT_REPRESENTATION, "invalid byte sequence for encoding \"UTF8\""))?,
        ),
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
        Type::Array(elem) => Value::Array(decode_array(b, elem, ty)?),
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

fn decode_array(b: &[u8], elem: &Type, ty: &Type) -> SqlResult<Vec<Value>> {
    let mut r = Reader { b, pos: 0, ty };
    let ndim = r.i32()?;
    let _has_null = r.i32()?;
    let _elem_oid = r.i32()?;
    match ndim {
        0 => return Ok(Vec::new()),
        1 => {}
        _ => return Err(SqlError::feature_not_supported("multidimensional arrays are not supported")),
    }
    let len = usize::try_from(r.i32()?).map_err(|_| bad(ty, "negative dimension"))?;
    let _lower = r.i32()?;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let n = r.i32()?;
        if n < 0 {
            out.push(Value::Null);
        } else {
            let chunk = r.bytes(usize::try_from(n).map_err(|_| bad(ty, "bad length"))?)?;
            out.push(decode(chunk, elem)?);
        }
    }
    Ok(out)
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
    fn jsonb_version_byte() {
        let v = Value::Jsonb(Jsonb::parse(r#"{"a":1}"#).expect("json"));
        let bytes = encode(&v, &Type::Jsonb).expect("encode");
        assert_eq!(bytes[0], 1);
        assert_eq!(&bytes[1..], br#"{"a": 1}"#);
    }
}
