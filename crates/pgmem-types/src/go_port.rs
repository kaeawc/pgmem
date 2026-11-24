//! Ports of the Go `types` package tests. Where pgmem-go kept non-Postgres behaviour (raw,
//! unnormalized jsonb; rejecting escape-format bytea input), the port asserts what Postgres
//! does (see PARITY.md).

use crate::{Format, Type, Value, binary, decode, encode, text};

const SAMPLE_JSON: &str = r#"{"k":"v","n":42}"#;
/// Postgres' canonical jsonb rendering of `SAMPLE_JSON`.
const SAMPLE_JSON_PG: &str = r#"{"k": "v", "n": 42}"#;

fn json() -> Value {
    Value::Jsonb(crate::Jsonb::parse(SAMPLE_JSON).expect("json"))
}

/// Deviation from pgmem-go: Go echoed the input text; Postgres normalizes jsonb output.
#[test]
fn jsonb_text_round_trip() {
    let enc = text::encode(&json(), &Type::Jsonb).expect("encode");
    assert_eq!(enc, SAMPLE_JSON_PG);
    assert_eq!(text::decode(&enc, &Type::Jsonb).expect("decode"), json());
}

/// Deviation from pgmem-go: the payload after the version byte is the normalized text.
#[test]
fn jsonb_binary_round_trip() {
    let enc = binary::encode(&json(), &Type::Jsonb).expect("encode");
    assert_eq!(enc[0], 1, "version byte");
    assert_eq!(&enc[1..], SAMPLE_JSON_PG.as_bytes());
    assert_eq!(binary::decode(&enc, &Type::Jsonb).expect("decode"), json());
}

#[test]
fn jsonb_rejects_bad_version_byte() {
    assert!(binary::decode(&[0x02, b'x'], &Type::Jsonb).is_err());
}

#[test]
fn jsonb_lookup_by_oid_and_name() {
    assert_eq!(Type::from_oid(3802).map(|t| t.typname()), Some("jsonb".into()));
    assert_eq!(Type::from_sql_name("jsonb"), Some(Type::Jsonb));
}

const BYTEA_SAMPLE: &[u8] = &[0x00, 0x01, 0xfe, 0xff, b'h', b'i'];

#[test]
fn bytea_text_round_trip() {
    let v = Value::Bytea(BYTEA_SAMPLE.to_vec());
    let enc = text::encode(&v, &Type::Bytea).expect("encode");
    assert_eq!(enc, r"\x0001feff6869");
    assert_eq!(text::decode(&enc, &Type::Bytea).expect("decode"), v);
}

#[test]
fn bytea_binary_round_trip() {
    let v = Value::Bytea(BYTEA_SAMPLE.to_vec());
    let enc = encode(&v, &Type::Bytea, Format::Binary).expect("encode");
    assert_eq!(enc, BYTEA_SAMPLE);
    assert_eq!(decode(&enc, &Type::Bytea, Format::Binary).expect("decode"), v);
}

/// Deviation from pgmem-go: Go rejected non-hex text input; Postgres accepts the escape
/// format, where plain characters stand for themselves (`'abc'::bytea` is `\x616263`).
#[test]
fn bytea_rejects_legacy_escape_format() {
    assert_eq!(text::decode("abc", &Type::Bytea).expect("escape format"), Value::Bytea(b"abc".to_vec()));
    // Malformed escapes are still rejected.
    assert!(text::decode(r"\9", &Type::Bytea).is_err());
}

#[test]
fn bytea_lookup_by_oid_and_name() {
    assert_eq!(Type::from_oid(17).map(|t| t.typname()), Some("bytea".into()));
    assert_eq!(Type::from_sql_name("bytea"), Some(Type::Bytea));
}

const CANON: &str = "01020304-0506-0708-090a-0b0c0d0e0f10";
const NO_DASH: &str = "0102030405060708090a0b0c0d0e0f10";
const RAW: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

#[test]
fn uuid_text_encode_decode_round_trip() {
    assert_eq!(text::encode(&Value::Uuid(RAW), &Type::Uuid).expect("encode"), CANON);
    assert_eq!(text::decode(CANON, &Type::Uuid).expect("decode"), Value::Uuid(RAW));
}

#[test]
fn uuid_binary_round_trip() {
    let enc = binary::encode(&Value::Uuid(RAW), &Type::Uuid).expect("encode");
    assert_eq!(enc, RAW);
    assert_eq!(binary::decode(&enc, &Type::Uuid).expect("decode"), Value::Uuid(RAW));
}

#[test]
fn uuid_accepts_bare_hex_input() {
    assert_eq!(text::decode(NO_DASH, &Type::Uuid).expect("decode"), Value::Uuid(RAW));
}

#[test]
fn uuid_rejects_bad_input() {
    for input in ["", "too-short", "01020304-0506-0708-090a-0b0c0d0e0f1g"] {
        let err = text::decode(input, &Type::Uuid).expect_err(input);
        assert_eq!(err.code, "22P02", "{input}");
        assert_eq!(err.message, format!("invalid input syntax for type uuid: \"{input}\""));
    }
}

#[test]
fn uuid_lookup_by_oid_and_name() {
    assert_eq!(Type::from_oid(2950).map(|t| t.typname()), Some("uuid".into()));
    assert_eq!(Type::from_sql_name("uuid"), Some(Type::Uuid));
}

/// 2025-07-04 12:30:45.123456 UTC in Postgres microseconds.
fn sample_ts() -> i64 {
    crate::datetime::parse_timestamptz("2025-07-04 12:30:45.123456+00").expect("ts")
}

#[test]
fn timestamptz_binary_round_trip() {
    let v = Value::Timestamptz(sample_ts());
    let enc = binary::encode(&v, &Type::Timestamptz).expect("encode");
    assert_eq!(enc.len(), 8);
    assert_eq!(binary::decode(&enc, &Type::Timestamptz).expect("decode"), v);
}

#[test]
fn timestamptz_binary_epoch() {
    let epoch = crate::datetime::parse_timestamptz("2000-01-01 00:00:00+00").expect("epoch");
    assert_eq!(binary::encode(&Value::Timestamptz(epoch), &Type::Timestamptz).expect("encode"), [0u8; 8]);
    let one_hour = 3_600_000_000i64.to_be_bytes();
    let v = binary::decode(&one_hour, &Type::Timestamptz).expect("decode");
    assert_eq!(text::encode(&v, &Type::Timestamptz).expect("text"), "2000-01-01 01:00:00+00");
}

#[test]
fn timestamptz_text_round_trip() {
    let want = crate::datetime::parse_timestamptz("2025-07-04 12:30:45+00").expect("ts");
    let enc = text::encode(&Value::Timestamptz(want), &Type::Timestamptz).expect("encode");
    assert_eq!(enc, "2025-07-04 12:30:45+00");
    assert_eq!(text::decode(&enc, &Type::Timestamptz).expect("decode"), Value::Timestamptz(want));
}

#[test]
fn timestamptz_lookup_by_oid_and_name() {
    assert_eq!(Type::from_oid(1184).map(|t| t.typname()), Some("timestamptz".into()));
    assert_eq!(Type::from_sql_name("timestamptz"), Some(Type::Timestamptz));
}
