//! Message framing for protocol version 3.

use pgmem_types::SqlError;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest message we accept (Postgres' limit for most messages is 1 GB).
const MAX_MESSAGE: usize = 1 << 30;

pub const SSL_REQUEST: i32 = 80_877_103;
pub const GSSENC_REQUEST: i32 = 80_877_104;
pub const CANCEL_REQUEST: i32 = 80_877_102;

/// A message before authentication completes (no type byte).
pub enum StartupMessage {
    Ssl,
    GssEnc,
    /// A query-cancel request. pgmem runs each statement to completion, so it is ignored.
    Cancel,
    Startup {
        minor: u16,
        params: Vec<(String, String)>,
    },
}

pub async fn read_startup<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<StartupMessage> {
    let len = usize::try_from(r.read_i32().await?).map_err(|_| bad("negative startup length"))?;
    if !(8..=10_000).contains(&len) {
        return Err(bad("invalid startup packet length"));
    }
    let code = r.read_i32().await?;
    let mut body = vec![0u8; len - 8];
    r.read_exact(&mut body).await?;
    match code {
        SSL_REQUEST => Ok(StartupMessage::Ssl),
        GSSENC_REQUEST => Ok(StartupMessage::GssEnc),
        CANCEL_REQUEST => Ok(StartupMessage::Cancel),
        v if v >> 16 == 3 => {
            let minor = u16::try_from(v & 0xffff).unwrap_or(0);
            let mut params = Vec::new();
            let mut cur = Reader::new(&body);
            loop {
                let k = cur.cstr().map_err(|_| bad("bad startup parameters"))?;
                if k.is_empty() {
                    break;
                }
                let v = cur.cstr().map_err(|_| bad("bad startup parameters"))?;
                params.push((k, v));
            }
            Ok(StartupMessage::Startup { minor, params })
        }
        v => Err(bad(&format!("unsupported frontend protocol {}.{}", v >> 16, v & 0xffff))),
    }
}

/// A frontend message: type byte and body.
pub struct Message {
    pub tag: u8,
    pub body: Vec<u8>,
}

pub async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Message>> {
    let tag = match r.read_u8().await {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = usize::try_from(r.read_i32().await?).map_err(|_| bad("negative message length"))?;
    if !(4..=MAX_MESSAGE).contains(&len) {
        return Err(bad("invalid message length"));
    }
    let mut body = vec![0u8; len - 4];
    r.read_exact(&mut body).await?;
    Ok(Some(Message { tag, body }))
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

/// A cursor over a message body.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

fn proto(msg: &str) -> SqlError {
    SqlError::new(pgmem_types::code::PROTOCOL_VIOLATION, msg.to_owned())
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    pub fn cstr(&mut self) -> Result<String, SqlError> {
        let rest = &self.buf[self.pos.min(self.buf.len())..];
        let end = rest.iter().position(|b| *b == 0).ok_or_else(|| proto("invalid string in message"))?;
        let s = std::str::from_utf8(&rest[..end])
            .map_err(|_| SqlError::new(pgmem_types::code::INVALID_TEXT_REPRESENTATION, "invalid byte sequence for encoding \"UTF8\""))?
            .to_owned();
        self.pos += end + 1;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, SqlError> {
        let b = *self.buf.get(self.pos).ok_or_else(|| proto("insufficient data left in message"))?;
        self.pos += 1;
        Ok(b)
    }

    pub fn i16(&mut self) -> Result<i16, SqlError> {
        let b = self.buf.get(self.pos..self.pos + 2).ok_or_else(|| proto("insufficient data left in message"))?;
        self.pos += 2;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub fn i32(&mut self) -> Result<i32, SqlError> {
        let b = self.buf.get(self.pos..self.pos + 4).ok_or_else(|| proto("insufficient data left in message"))?;
        self.pos += 4;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], SqlError> {
        let b = self.buf.get(self.pos..self.pos + n).ok_or_else(|| proto("insufficient data left in message"))?;
        self.pos += n;
        Ok(b)
    }
}

/// Builds backend messages into an output buffer.
#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    /// Starts a message; returns the offset of its length field.
    fn begin(&mut self, tag: u8) -> usize {
        self.buf.push(tag);
        let at = self.buf.len();
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        at
    }

    fn finish(&mut self, at: usize) {
        let len = i32::try_from(self.buf.len() - at).unwrap_or(i32::MAX);
        self.buf[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }

    fn cstr(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    fn i16(&mut self, n: i16) {
        self.buf.extend_from_slice(&n.to_be_bytes());
    }

    fn i32(&mut self, n: i32) {
        self.buf.extend_from_slice(&n.to_be_bytes());
    }

    pub fn auth_ok(&mut self) {
        let at = self.begin(b'R');
        self.i32(0);
        self.finish(at);
    }

    pub fn parameter_status(&mut self, name: &str, value: &str) {
        let at = self.begin(b'S');
        self.cstr(name);
        self.cstr(value);
        self.finish(at);
    }

    pub fn backend_key_data(&mut self, pid: i32, key: i32) {
        let at = self.begin(b'K');
        self.i32(pid);
        self.i32(key);
        self.finish(at);
    }

    pub fn negotiate_protocol_version(&mut self, minor: i32, unrecognized: &[String]) {
        let at = self.begin(b'v');
        self.i32(minor);
        self.i32(i32::try_from(unrecognized.len()).unwrap_or(0));
        for o in unrecognized {
            self.cstr(o);
        }
        self.finish(at);
    }

    /// NotificationResponse (LISTEN / NOTIFY).
    pub fn notification(&mut self, pid: i32, channel: &str, payload: &str) {
        let at = self.begin(b'A');
        self.i32(pid);
        self.cstr(channel);
        self.cstr(payload);
        self.finish(at);
    }

    pub fn ready_for_query(&mut self, status: u8) {
        let at = self.begin(b'Z');
        self.buf.push(status);
        self.finish(at);
    }

    /// One field per column: (name, table oid, attnum, type oid, typlen, typmod, format).
    pub fn row_description(&mut self, fields: &[(String, u32, i16, u32, i16, i32, i16)]) {
        let at = self.begin(b'T');
        self.i16(i16::try_from(fields.len()).unwrap_or(i16::MAX));
        for (name, table, attnum, oid, len, typmod, format) in fields {
            self.cstr(name);
            self.i32(i32::try_from(*table).unwrap_or(0));
            self.i16(*attnum);
            self.i32(i32::try_from(*oid).unwrap_or(0));
            self.i16(*len);
            self.i32(*typmod);
            self.i16(*format);
        }
        self.finish(at);
    }

    pub fn data_row(&mut self, values: &[Option<Vec<u8>>]) {
        let at = self.begin(b'D');
        self.i16(i16::try_from(values.len()).unwrap_or(i16::MAX));
        for v in values {
            match v {
                None => self.i32(-1),
                Some(b) => {
                    self.i32(i32::try_from(b.len()).unwrap_or(i32::MAX));
                    self.buf.extend_from_slice(b);
                }
            }
        }
        self.finish(at);
    }

    pub fn command_complete(&mut self, tag: &str) {
        let at = self.begin(b'C');
        self.cstr(tag);
        self.finish(at);
    }

    pub fn empty_query(&mut self) {
        let at = self.begin(b'I');
        self.finish(at);
    }

    fn fields(&mut self, tag: u8, severity: &str, code: &str, message: &str, detail: Option<&str>, hint: Option<&str>) {
        self.fields_with(tag, severity, code, message, detail, hint, &[]);
    }

    #[allow(clippy::too_many_arguments)]
    fn fields_with(
        &mut self,
        tag: u8,
        severity: &str,
        code: &str,
        message: &str,
        detail: Option<&str>,
        hint: Option<&str>,
        extra: &[(u8, &str)],
    ) {
        let at = self.begin(tag);
        self.buf.push(b'S');
        self.cstr(severity);
        self.buf.push(b'V');
        self.cstr(severity);
        self.buf.push(b'C');
        self.cstr(code);
        self.buf.push(b'M');
        self.cstr(message);
        if let Some(d) = detail {
            self.buf.push(b'D');
            self.cstr(d);
        }
        if let Some(h) = hint {
            self.buf.push(b'H');
            self.cstr(h);
        }
        for (k, v) in extra {
            self.buf.push(*k);
            self.cstr(v);
        }
        self.buf.push(0);
        self.finish(at);
    }

    pub fn error(&mut self, e: &SqlError) {
        let severity = if e.code == "57P01" { "FATAL" } else { "ERROR" };
        let mut extra: Vec<(u8, &str)> = Vec::new();
        if let Some(f) = e.fields() {
            for (k, v) in [(b's', &f.schema), (b't', &f.table), (b'c', &f.column), (b'n', &f.constraint)] {
                if let Some(v) = v {
                    extra.push((k, v.as_str()));
                }
            }
        }
        self.fields_with(b'E', severity, e.code, &e.message, e.detail(), e.hint(), &extra);
    }

    pub fn fatal(&mut self, code: &str, message: &str) {
        self.fields(b'E', "FATAL", code, message, None, None);
    }

    pub fn notice(&mut self, severity: &str, code: &str, message: &str) {
        self.fields(b'N', severity, code, message, None, None);
    }

    pub fn parse_complete(&mut self) {
        let at = self.begin(b'1');
        self.finish(at);
    }

    pub fn bind_complete(&mut self) {
        let at = self.begin(b'2');
        self.finish(at);
    }

    pub fn close_complete(&mut self) {
        let at = self.begin(b'3');
        self.finish(at);
    }

    pub fn no_data(&mut self) {
        let at = self.begin(b'n');
        self.finish(at);
    }

    pub fn portal_suspended(&mut self) {
        let at = self.begin(b's');
        self.finish(at);
    }

    pub fn parameter_description(&mut self, oids: &[u32]) {
        let at = self.begin(b't');
        self.i16(i16::try_from(oids.len()).unwrap_or(i16::MAX));
        for o in oids {
            self.i32(i32::try_from(*o).unwrap_or(0));
        }
        self.finish(at);
    }
}
