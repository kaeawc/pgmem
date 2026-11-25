//! The `pgcrypto` and `uuid-ossp` extensions' functions: digest, hmac, crypt (bcrypt and
//! MD5-crypt), gen_salt, gen_random_bytes, and uuid_generate_v1 / v3 / v4 / v5 with the
//! namespace constants.

use super::{Resolved, resolved, strict};
use crate::eval::EvalCtx;
use md5::Digest as _;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};

fn bytes_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Bytea(b) => b.clone(),
        other => other.as_str().unwrap_or("").as_bytes().to_vec(),
    }
}

fn random(n: usize) -> SqlResult<Vec<u8>> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).map_err(|e| SqlError::internal(format!("random bytes: {e}")))?;
    Ok(b)
}

fn hash(algorithm: &str, data: &[u8]) -> SqlResult<(Vec<u8>, usize)> {
    Ok(match algorithm.to_ascii_lowercase().as_str() {
        "md5" => (md5::Md5::digest(data).to_vec(), 64),
        "sha1" => (sha1::Sha1::digest(data).to_vec(), 64),
        "sha224" => (sha2::Sha224::digest(data).to_vec(), 64),
        "sha256" => (sha2::Sha256::digest(data).to_vec(), 64),
        "sha384" => (sha2::Sha384::digest(data).to_vec(), 128),
        "sha512" => (sha2::Sha512::digest(data).to_vec(), 128),
        _ => {
            return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("Cannot use \"{algorithm}\": No such hash algorithm")));
        }
    })
}

fn digest_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    Ok(Value::Bytea(hash(a[1].as_str().unwrap_or(""), &bytes_of(&a[0]))?.0))
}

/// HMAC (RFC 2104) over any of the digest algorithms.
fn hmac_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let algorithm = a[2].as_str().unwrap_or("");
    let (data, mut key) = (bytes_of(&a[0]), bytes_of(&a[1]));
    let block = hash(algorithm, b"")?.1;
    if key.len() > block {
        key = hash(algorithm, &key)?.0;
    }
    key.resize(block, 0);
    let ipad: Vec<u8> = key.iter().map(|b| b ^ 0x36).chain(data).collect();
    let inner = hash(algorithm, &ipad)?.0;
    let opad: Vec<u8> = key.iter().map(|b| b ^ 0x5c).chain(inner).collect();
    Ok(Value::Bytea(hash(algorithm, &opad)?.0))
}

const CRYPT64: &[u8] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BCRYPT64: &[u8] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// crypt(3)'s base-64: low 6 bits first.
fn to64(out: &mut String, mut v: u32, n: usize) {
    for _ in 0..n {
        out.push(char::from(CRYPT64[(v & 0x3f) as usize]));
        v >>= 6;
    }
}

/// FreeBSD MD5-crypt (`$1$salt$hash`).
fn md5_crypt(pw: &[u8], setting: &str) -> String {
    let salt: String = setting.trim_start_matches("$1$").split('$').next().unwrap_or("").chars().take(8).collect();
    let s = salt.as_bytes();
    let mut ctx = md5::Md5::new();
    ctx.update(pw);
    ctx.update(b"$1$");
    ctx.update(s);
    let alt = md5::Md5::new().chain_update(pw).chain_update(s).chain_update(pw).finalize();
    let mut left = pw.len();
    while left > 0 {
        ctx.update(&alt[..left.min(16)]);
        left = left.saturating_sub(16);
    }
    let mut i = pw.len();
    while i > 0 {
        if i & 1 == 1 {
            ctx.update([0u8]);
        } else {
            ctx.update(&pw[..1]);
        }
        i >>= 1;
    }
    let mut fin = ctx.finalize().to_vec();
    for i in 0..1000 {
        let mut c = md5::Md5::new();
        if i & 1 == 1 {
            c.update(pw);
        } else {
            c.update(&fin);
        }
        if i % 3 != 0 {
            c.update(s);
        }
        if i % 7 != 0 {
            c.update(pw);
        }
        if i & 1 == 1 {
            c.update(&fin);
        } else {
            c.update(pw);
        }
        fin = c.finalize().to_vec();
    }
    let f = |i: usize| u32::from(fin[i]);
    let mut out = format!("$1${salt}$");
    for (a, b, c) in [(0, 6, 12), (1, 7, 13), (2, 8, 14), (3, 9, 15), (4, 10, 5)] {
        to64(&mut out, (f(a) << 16) | (f(b) << 8) | f(c), 4);
    }
    to64(&mut out, f(11), 2);
    out
}

/// bcrypt's base-64 (standard bit order, its own alphabet, no padding).
fn bcrypt_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(char::from(BCRYPT64[((n >> (18 - 6 * i)) & 0x3f) as usize]));
        }
    }
    out
}

fn bcrypt_decode(s: &str, len: usize) -> Option<Vec<u8>> {
    let vals: Vec<u32> = s.bytes().map(|c| BCRYPT64.iter().position(|x| *x == c).map(|p| p as u32)).collect::<Option<_>>()?;
    let mut out = Vec::new();
    for chunk in vals.chunks(4) {
        let mut n = 0u32;
        for (i, v) in chunk.iter().enumerate() {
            n |= v << (18 - 6 * i);
        }
        for i in 0..chunk.len().saturating_sub(1) {
            out.push(u8::try_from((n >> (16 - 8 * i)) & 0xff).unwrap_or(0));
        }
    }
    out.truncate(len);
    Some(out)
}

/// OpenBSD bcrypt (`$2a$cost$salt+hash`), as pgcrypto's crypt-blowfish.
fn bcrypt(pw: &[u8], setting: &str) -> SqlResult<String> {
    let bad = || SqlError::new(code::INVALID_PARAMETER_VALUE, "invalid salt");
    let rest = setting.get(4..).ok_or_else(bad)?;
    let variant = &setting[..4];
    let (cost, salt) = rest.split_once('$').ok_or_else(bad)?;
    let cost: u32 = cost.parse().map_err(|_| bad())?;
    if !(4..=31).contains(&cost) {
        return Err(bad());
    }
    let salt_bytes = bcrypt_decode(salt.get(..22).ok_or_else(bad)?, 16).ok_or_else(bad)?;
    let mut key: Vec<u8> = pw.to_vec();
    key.push(0);
    key.truncate(72);
    let mut state = blowfish::Blowfish::bc_init_state();
    state.salted_expand_key(&salt_bytes, &key);
    for _ in 0..(1u64 << cost) {
        state.bc_expand_key(&key);
        state.bc_expand_key(&salt_bytes);
    }
    let magic = b"OrpheanBeholderScryDoubt";
    let mut words: Vec<u32> = magic.chunks(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect();
    for _ in 0..64 {
        for j in (0..6).step_by(2) {
            let [l, r] = state.bc_encrypt([words[j], words[j + 1]]);
            words[j] = l;
            words[j + 1] = r;
        }
    }
    let hash: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).take(23).collect();
    Ok(format!("{variant}{cost:02}${}{}", &salt[..22], bcrypt_encode(&hash)))
}

fn crypt_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let pw = bytes_of(&a[0]);
    let setting = a[1].as_str().unwrap_or("");
    let out = if setting.starts_with("$1$") {
        md5_crypt(&pw, setting)
    } else if setting.starts_with("$2a$") || setting.starts_with("$2b$") || setting.starts_with("$2x$") || setting.starts_with("$2y$") {
        bcrypt(&pw, setting)?
    } else {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "invalid salt"));
    };
    Ok(Value::Text(out))
}

fn gen_salt_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let kind = a[0].as_str().unwrap_or("").to_ascii_lowercase();
    let rounds = a.get(1).and_then(Value::as_i64);
    Ok(Value::Text(match kind.as_str() {
        "md5" => {
            let mut s = String::from("$1$");
            for b in random(8)? {
                s.push(char::from(CRYPT64[usize::from(b & 0x3f)]));
            }
            s
        }
        "bf" => {
            let cost = rounds.unwrap_or(6);
            if !(4..=31).contains(&cost) {
                return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "gen_salt: Incorrect number of rounds"));
            }
            format!("$2a${cost:02}${}", &bcrypt_encode(&random(16)?)[..22])
        }
        _ => return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "gen_salt: Unknown salt algorithm")),
    }))
}

fn gen_random_bytes_fn(a: &[Value], _: &EvalCtx) -> SqlResult<Value> {
    let n = a[0].as_i64().unwrap_or(0);
    if !(1..=1024).contains(&n) {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "Length not in range"));
    }
    Ok(Value::Bytea(random(usize::try_from(n).unwrap_or(0))?))
}

// ---------------------------------------------------------------------------------------------
// uuid-ossp
// ---------------------------------------------------------------------------------------------

const NS_DNS: [u8; 16] = [0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8];

fn namespace(last: u8) -> Value {
    let mut b = NS_DNS;
    b[3] = last;
    Value::Uuid(b)
}

fn with_version(mut b: [u8; 16], version: u8) -> Value {
    b[6] = (b[6] & 0x0f) | (version << 4);
    b[8] = (b[8] & 0x3f) | 0x80;
    Value::Uuid(b)
}

fn name_based(a: &[Value], version: u8) -> SqlResult<Value> {
    let Value::Uuid(ns) = &a[0] else { return Err(SqlError::internal("uuid namespace expected")) };
    let mut data = ns.to_vec();
    data.extend_from_slice(a[1].as_str().unwrap_or("").as_bytes());
    let h = if version == 3 { md5::Md5::digest(&data).to_vec() } else { sha1::Sha1::digest(&data).to_vec() };
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    Ok(with_version(b, version))
}

/// Version 1: 100 ns ticks since 1582-10-15, a random clock sequence and (multicast) node.
fn uuid_v1(_: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    // Postgres epoch (2000-01-01) to the Gregorian epoch, in 100 ns ticks.
    const GREGORIAN_TO_PG_TICKS: i64 = 0x01b2_1dd2_1381_4000 + 946_684_800 * 10_000_000;
    let ticks = u64::try_from(ctx.env.clock.now_micros() * 10 + GREGORIAN_TO_PG_TICKS).unwrap_or(0);
    let rnd = random(8)?;
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&u32::try_from(ticks & 0xffff_ffff).unwrap_or(0).to_be_bytes());
    b[4..6].copy_from_slice(&u16::try_from((ticks >> 32) & 0xffff).unwrap_or(0).to_be_bytes());
    b[6..8].copy_from_slice(&u16::try_from((ticks >> 48) & 0x0fff).unwrap_or(0).to_be_bytes());
    b[8..10].copy_from_slice(&rnd[..2]);
    b[10..16].copy_from_slice(&rnd[2..8]);
    b[10] |= 0x01;
    Ok(with_version(b, 1))
}

pub fn resolve(name: &str, args: &[Type]) -> SqlResult<Option<Resolved>> {
    use Type::{Bytea, Int4, Text, Uuid};
    let n = args.len();
    let data = |t: &Type| if *t == Bytea { Bytea } else { Text };
    match name {
        "digest" if n == 2 => resolved(strict("digest", digest_fn), vec![data(&args[0]), Text], Bytea),
        "hmac" if n == 3 => resolved(strict("hmac", hmac_fn), vec![data(&args[0]), data(&args[1]), Text], Bytea),
        "crypt" if n == 2 => resolved(strict("crypt", crypt_fn), vec![Text, Text], Text),
        "gen_salt" if n == 1 || n == 2 => resolved(strict("gen_salt", gen_salt_fn), [Text, Int4][..n].to_vec(), Text),
        "gen_random_bytes" if n == 1 => resolved(strict("gen_random_bytes", gen_random_bytes_fn), vec![Int4], Bytea),
        "uuid_nil" if n == 0 => resolved(strict("uuid_nil", |_, _| Ok(Value::Uuid([0; 16]))), vec![], Uuid),
        "uuid_ns_dns" if n == 0 => resolved(strict("uuid_ns_dns", |_, _| Ok(namespace(0x10))), vec![], Uuid),
        "uuid_ns_url" if n == 0 => resolved(strict("uuid_ns_url", |_, _| Ok(namespace(0x11))), vec![], Uuid),
        "uuid_ns_oid" if n == 0 => resolved(strict("uuid_ns_oid", |_, _| Ok(namespace(0x12))), vec![], Uuid),
        "uuid_ns_x500" if n == 0 => resolved(strict("uuid_ns_x500", |_, _| Ok(namespace(0x14))), vec![], Uuid),
        "uuid_generate_v1" | "uuid_generate_v1mc" if n == 0 => resolved(strict("uuid_generate_v1", uuid_v1), vec![], Uuid),
        "uuid_generate_v3" if n == 2 => resolved(strict("uuid_generate_v3", |a, _| name_based(a, 3)), vec![Uuid, Text], Uuid),
        "uuid_generate_v5" if n == 2 => resolved(strict("uuid_generate_v5", |a, _| name_based(a, 5)), vec![Uuid, Text], Uuid),
        _ => Ok(None),
    }
}
