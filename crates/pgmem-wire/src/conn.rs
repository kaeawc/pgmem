//! Per-connection protocol state machine.

use crate::Shared;
use crate::codec::{Message, Reader, StartupMessage, Writer, read_message, read_startup};
use pgmem_exec::portal::PreparedStatement;
use pgmem_exec::session::{is_reported, setting_display_name};
use pgmem_exec::{CopyIn, CopyOut, OutCol, QueryResult, Session};
use pgmem_ir::Statement;
use pgmem_types::{Format, SourceRefs, SqlError, SqlResult, Type, Value, code};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::{Notify, mpsc};

struct Portal {
    stmt: Arc<PreparedStatement>,
    params: Vec<Value>,
    result_formats: Vec<i16>,
    result: Option<QueryResult>,
    sent: usize,
}

struct Conn {
    session: Session,
    out: Writer,
    portals: HashMap<String, Portal>,
    /// After an error in extended-query mode, messages are discarded until Sync.
    skipping: bool,
    /// A COPY FROM STDIN receiving CopyData.
    copy: Option<CopyState>,
}

struct CopyState {
    pending: Box<CopyIn>,
    data: Vec<u8>,
    /// From a simple Query: the statements after the COPY, run once it completes.
    rest: Option<VecDeque<(Statement, SourceRefs)>>,
}

fn format_for(codes: &[i16], i: usize) -> Format {
    match codes.len() {
        0 => Format::Text,
        1 => Format::from_code(codes[0]),
        _ => Format::from_code(codes.get(i).copied().unwrap_or(0)),
    }
}

pub async fn handle(stream: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let (rd, wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let mut wr = BufWriter::new(wr);
    // Startup: refuse SSL/GSS encryption, then accept the startup packet (trust auth).
    let (minor, params) = loop {
        match read_startup(&mut rd).await? {
            StartupMessage::Ssl | StartupMessage::GssEnc => {
                wr.write_all(b"N").await?;
                wr.flush().await?;
            }
            StartupMessage::Cancel => return Ok(()),
            StartupMessage::Startup { minor, params } => break (minor, params),
        }
    };
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let Some(user) = get("user") else {
        let mut w = Writer::default();
        w.fatal("28000", "no PostgreSQL user name specified in startup packet");
        wr.write_all(&w.buf).await?;
        wr.flush().await?;
        return Ok(());
    };
    let database = get("database").unwrap_or_else(|| user.clone());
    let pid = shared.next_pid();
    let mut session = Session::new(Arc::clone(&shared.db), shared.clock(), &user, &database, pid);
    let mut out = Writer::default();
    // Protocol 3.x with x > 0, or `_pq_.` options: negotiate down to 3.0.
    let unrecognized: Vec<String> = params.iter().filter(|(k, _)| k.starts_with("_pq_.")).map(|(k, _)| k.clone()).collect();
    if minor > 0 || !unrecognized.is_empty() {
        out.negotiate_protocol_version(0, &unrecognized);
    }
    for (k, v) in &params {
        let key = k.to_ascii_lowercase();
        match key.as_str() {
            "user" | "database" | "replication" | "options" => {}
            // pgmem's session time zone is fixed at UTC; other zones are accepted at startup
            // (drivers like pgjdbc send the JVM's zone) but not applied.
            "timezone" => {}
            "client_encoding" => {}
            _ if key.starts_with("_pq_.") => {}
            _ => {
                session.settings.insert(key, v.clone());
            }
        }
    }
    out.auth_ok();
    for (k, v) in session.settings.clone() {
        if is_reported(&k) {
            out.parameter_status(setting_display_name(&k), &v);
        }
    }
    out.backend_key_data(pid, pid.wrapping_mul(7919));
    out.ready_for_query(b'I');
    wr.write_all(&out.buf).await?;
    wr.flush().await?;
    // Notifications for this session wake the connection while it waits for a message.
    let wake = Arc::new(Notify::new());
    let waker = Arc::clone(&wake);
    shared.db.notifications().set_waker(pid, Arc::new(move || waker.notify_one()));
    // Messages are read by their own task, so waiting for one can be interrupted safely.
    let (tx, mut rx) = mpsc::channel::<io::Result<Option<crate::codec::Message>>>(16);
    let reader = tokio::spawn(async move {
        loop {
            let msg = read_message(&mut rd).await;
            let done = !matches!(msg, Ok(Some(_)));
            if tx.send(msg).await.is_err() || done {
                return;
            }
        }
    });
    let mut conn = Conn { session, out: Writer::default(), portals: HashMap::new(), skipping: false, copy: None };
    let result = conn.serve(&mut rx, &wake, &mut wr).await;
    reader.abort();
    result
}

impl Conn {
    async fn serve(
        &mut self,
        rx: &mut mpsc::Receiver<io::Result<Option<crate::codec::Message>>>,
        wake: &Notify,
        wr: &mut BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    ) -> io::Result<()> {
        loop {
            let msg = tokio::select! {
                msg = rx.recv() => msg,
                () = wake.notified() => {
                    // Delivered right away only between transactions.
                    if self.session.is_idle() {
                        self.deliver_notifications();
                        wr.write_all(&self.out.buf).await?;
                        self.out.buf.clear();
                        wr.flush().await?;
                    }
                    continue;
                }
            };
            let Some(msg) = msg.transpose()?.flatten() else { return Ok(()) };
            if self.handle_message(msg, wr).await? {
                return Ok(());
            }
        }
    }

    /// Dispatches one message and writes its output; true when the connection ends.
    async fn handle_message(
        &mut self,
        msg: crate::codec::Message,
        wr: &mut BufWriter<tokio::net::tcp::OwnedWriteHalf>,
    ) -> io::Result<bool> {
        let conn = self;
        // A statement may wait for a lock (`pg_advisory_lock`): let the runtime move other
        // connections off this worker meanwhile.
        let keep_going = if tokio::runtime::Handle::current().runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
            tokio::task::block_in_place(|| conn.dispatch(msg))
        } else {
            conn.dispatch(msg)
        };
        if !conn.out.buf.is_empty() {
            wr.write_all(&conn.out.buf).await?;
            conn.out.buf.clear();
        }
        match keep_going {
            Flow::Continue => {}
            Flow::Flush => wr.flush().await?,
            Flow::Terminate => {
                wr.flush().await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Writes the notifications queued for this session.
    fn deliver_notifications(&mut self) {
        for n in self.session.take_notifications() {
            self.out.notification(n.sender_pid, &n.channel, &n.payload);
        }
    }

    /// ReadyForQuery, preceded by any notifications that arrived (between transactions).
    fn ready(&mut self) {
        if self.session.is_idle() {
            self.deliver_notifications();
        }
        self.out.ready_for_query(self.session.status().byte());
    }
}

enum Flow {
    Continue,
    Flush,
    Terminate,
}

impl Conn {
    fn dispatch(&mut self, msg: Message) -> Flow {
        if self.copy.is_some() {
            return self.copy_message(msg);
        }
        let extended = matches!(msg.tag, b'P' | b'B' | b'D' | b'E' | b'C');
        if self.skipping && extended {
            return Flow::Continue;
        }
        let result = match msg.tag {
            b'Q' => {
                self.simple_query(&msg.body);
                return Flow::Flush;
            }
            b'P' => self.parse(&msg.body),
            b'B' => self.bind(&msg.body),
            b'D' => self.describe(&msg.body),
            b'E' => self.execute(&msg.body),
            b'C' => self.close(&msg.body),
            b'S' => {
                if let Err(e) = self.session.end_pipeline(self.skipping) {
                    self.out.error(&e);
                }
                self.skipping = false;
                self.portals.remove("");
                self.session.close_portal("");
                self.ready();
                return Flow::Flush;
            }
            b'H' => return Flow::Flush,
            b'X' => return Flow::Terminate,
            // Data for a COPY that already failed: ignored, as Postgres does.
            b'd' | b'c' | b'f' => return Flow::Continue,
            b'F' => Err(SqlError::feature_not_supported("the fastpath function call protocol is not supported by pgmem")),
            other => {
                self.out.fatal(code::PROTOCOL_VIOLATION, &format!("invalid frontend message type {other}"));
                return Flow::Terminate;
            }
        };
        if let Err(e) = result {
            self.out.error(&e);
            if extended {
                self.skipping = true;
            }
        }
        Flow::Continue
    }

    fn send_result_preamble(&mut self, r: &QueryResult) {
        for n in &r.notices {
            self.out.notice(n.severity, n.code, &n.message);
        }
        for (k, v) in &r.changed_settings {
            self.out.parameter_status(k, v);
        }
    }

    fn row_description(&mut self, cols: &[OutCol], formats: &[i16]) {
        // User types' OIDs (array types, domains' base types) come from the session's types.
        let _ctx = pgmem_types::context::enter(self.session.format_context());
        let fields: Vec<(String, u32, i16, u32, i16, i32, i16)> = cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                // Like printtup, a domain column is described by its base type and typmod.
                let (ty, typmod) = match &c.ty {
                    Type::Domain(oid) => {
                        let (base, dmod) = pgmem_types::usertype::domain_base(*oid).unwrap_or((Type::Unknown, -1));
                        (base, if c.typmod >= 0 { c.typmod } else { dmod })
                    }
                    other => (other.clone(), c.typmod),
                };
                (c.name.clone(), c.table_oid, c.attnum, ty.oid(), ty.size(), typmod, format_for(formats, i).code())
            })
            .collect();
        self.out.row_description(&fields);
    }

    fn data_row(&mut self, cols: &[OutCol], row: &[Value], formats: &[i16]) -> SqlResult<()> {
        // Output depends on the session's TimeZone and DateStyle.
        let _ctx = pgmem_types::context::enter(self.session.format_context());
        let mut values = Vec::with_capacity(row.len());
        for (i, v) in row.iter().enumerate() {
            if v.is_null() {
                values.push(None);
            } else {
                let ty = cols.get(i).map_or_else(|| v.natural_type(), |c| c.ty.clone());
                values.push(Some(pgmem_types::encode(v, &ty, format_for(formats, i))?));
            }
        }
        self.out.data_row(&values);
        Ok(())
    }

    fn simple_query(&mut self, body: &[u8]) {
        // A simple Query abandons any unnamed portal / extended-protocol error state.
        self.skipping = false;
        let sql = match Reader::new(body).cstr() {
            Ok(s) => s,
            Err(e) => {
                self.out.error(&e);
                self.ready();
                return;
            }
        };
        let stmts = match pgmem_parse::parse_located(&sql) {
            Ok(s) => s,
            Err(e) => {
                self.out.error(&e);
                self.ready();
                return;
            }
        };
        self.session.begin_implicit_block(stmts.len());
        self.run_statements(stmts.into_iter().collect());
    }

    /// Runs a simple Query's statements. A COPY FROM STDIN suspends the batch until the
    /// client's CopyDone (`finish_copy` resumes it).
    fn run_statements(&mut self, mut stmts: VecDeque<(Statement, SourceRefs)>) {
        let mut failed = false;
        while let Some((stmt, refs)) = stmts.pop_front() {
            match self.session.execute(&stmt, &[], &[]) {
                Ok(mut r) => {
                    self.send_result_preamble(&r);
                    if let Some(pending) = r.copy_in.take() {
                        self.out.copy_response(b'G', pending.binary, pending.columns);
                        self.copy = Some(CopyState { pending, data: Vec::new(), rest: Some(stmts) });
                        return;
                    }
                    if let Err(e) = self.send_simple_result(&r) {
                        self.out.error(&e);
                        failed = true;
                        break;
                    }
                }
                Err(e) => {
                    self.out.error(&refs.resolve(e));
                    failed = true;
                    break;
                }
            }
        }
        if let Err(e) = self.session.end_implicit_block(failed) {
            self.out.error(&e);
        }
        self.ready();
    }

    fn send_simple_result(&mut self, r: &QueryResult) -> SqlResult<()> {
        if r.empty {
            self.out.empty_query();
            return Ok(());
        }
        if let Some(out) = &r.copy_out {
            self.send_copy_out(out);
        } else if r.returns_rows {
            let formats: &[i16] = if r.binary { &[1] } else { &[] };
            self.row_description(&r.columns, formats);
            for row in &r.rows {
                self.data_row(&r.columns, row, formats)?;
            }
        }
        self.out.command_complete(&r.tag);
        Ok(())
    }

    fn send_copy_out(&mut self, out: &CopyOut) {
        self.out.copy_response(b'H', out.binary, out.columns);
        for chunk in &out.data {
            self.out.copy_data(chunk);
        }
        self.out.copy_done();
    }

    /// A message during COPY FROM STDIN.
    fn copy_message(&mut self, msg: Message) -> Flow {
        match msg.tag {
            b'd' => {
                if let Some(c) = self.copy.as_mut() {
                    c.data.extend_from_slice(&msg.body);
                }
                Flow::Continue
            }
            b'c' => {
                let data = self.copy.as_mut().map(|c| std::mem::take(&mut c.data)).unwrap_or_default();
                self.finish_copy(Ok(data))
            }
            b'f' => {
                let reason = Reader::new(&msg.body).cstr().unwrap_or_default();
                self.finish_copy(Err(reason))
            }
            // Flush and Sync are ignored during COPY IN (clients send them right after the
            // COPY statement), but the CopyInResponse must reach the client.
            b'H' | b'S' => Flow::Flush,
            b'X' => Flow::Terminate,
            other => {
                let e = SqlError::new(code::PROTOCOL_VIOLATION, format!("unexpected message type 0x{other:02X} during COPY from stdin"));
                self.out.error(&e);
                self.finish_copy(Err(String::new()));
                Flow::Flush
            }
        }
    }

    fn finish_copy(&mut self, data: Result<Vec<u8>, String>) -> Flow {
        let Some(state) = self.copy.take() else { return Flow::Continue };
        let result = self.session.copy_from(&state.pending, data);
        let ok = match result {
            Ok(r) => {
                self.send_result_preamble(&r);
                self.out.command_complete(&r.tag);
                true
            }
            Err(e) => {
                self.out.error(&e);
                false
            }
        };
        match state.rest {
            Some(rest) if ok => self.run_statements(rest),
            Some(_) => {
                if let Err(e) = self.session.end_implicit_block(true) {
                    self.out.error(&e);
                }
                self.ready();
            }
            // Extended protocol: the client's Sync ends the transaction.
            None => self.skipping = !ok,
        }
        Flow::Flush
    }

    fn parse(&mut self, body: &[u8]) -> SqlResult<()> {
        // Declared parameter types may be user types (enums, domains, composites).
        let _ctx = pgmem_types::context::enter(self.session.format_context());
        let mut r = Reader::new(body);
        let name = r.cstr()?;
        let sql = r.cstr()?;
        let n = usize::try_from(r.i16()?).unwrap_or(0);
        let mut declared = Vec::with_capacity(n);
        for _ in 0..n {
            let oid = u32::try_from(r.i32()?).unwrap_or(0);
            declared.push(if oid == 0 {
                Type::Unknown
            } else {
                Type::from_oid(oid)
                    .or_else(|| pgmem_types::usertype::type_for_oid(oid))
                    .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type with OID {oid} does not exist")))?
            });
        }
        if !name.is_empty() && self.session.prepared(&name).is_some() {
            return Err(SqlError::new(code::DUPLICATE_PREPARED_STATEMENT, format!("prepared statement \"{name}\" already exists")));
        }
        let (stmt, refs) = pgmem_parse::parse_one_located(&sql)?;
        self.session.prepare_protocol(&name, &sql, stmt, &declared).map_err(|e| refs.resolve(e))?;
        self.out.parse_complete();
        Ok(())
    }

    fn bind(&mut self, body: &[u8]) -> SqlResult<()> {
        let _ctx = pgmem_types::context::enter(self.session.format_context());
        let mut r = Reader::new(body);
        let portal = r.cstr()?;
        let stmt_name = r.cstr()?;
        let nfmt = usize::try_from(r.i16()?).unwrap_or(0);
        let mut param_formats = Vec::with_capacity(nfmt);
        for _ in 0..nfmt {
            param_formats.push(r.i16()?);
        }
        let nparams = usize::try_from(r.i16()?).unwrap_or(0);
        let stmt = self
            .session
            .prepared(&stmt_name)
            .ok_or_else(|| SqlError::new(code::INVALID_SQL_STATEMENT_NAME, format!("prepared statement \"{stmt_name}\" does not exist")))?;
        if nparams != stmt.param_types.len() {
            return Err(SqlError::new(
                code::PROTOCOL_VIOLATION,
                format!(
                    "bind message supplies {nparams} parameters, but prepared statement \"{stmt_name}\" requires {}",
                    stmt.param_types.len()
                ),
            ));
        }
        if param_formats.len() > 1 && param_formats.len() != nparams {
            return Err(SqlError::new(
                code::PROTOCOL_VIOLATION,
                format!("bind message has {} parameter formats but {nparams} parameters", param_formats.len()),
            ));
        }
        let mut params = Vec::with_capacity(nparams);
        for (i, ty) in stmt.param_types.iter().enumerate() {
            let len = r.i32()?;
            if len < 0 {
                params.push(Value::Null);
                continue;
            }
            let raw = r.bytes(usize::try_from(len).unwrap_or(0))?;
            params.push(pgmem_types::decode(raw, ty, format_for(&param_formats, i))?);
        }
        let nres = usize::try_from(r.i16()?).unwrap_or(0);
        let mut result_formats = Vec::with_capacity(nres);
        for _ in 0..nres {
            result_formats.push(r.i16()?);
        }
        if !portal.is_empty() && self.portals.contains_key(&portal) {
            return Err(SqlError::new(code::DUPLICATE_CURSOR, format!("cursor \"{portal}\" already exists")));
        }
        self.session.open_portal(&portal, &stmt.text);
        self.portals.insert(portal, Portal { stmt, params, result_formats, result: None, sent: 0 });
        self.out.bind_complete();
        Ok(())
    }

    fn describe(&mut self, body: &[u8]) -> SqlResult<()> {
        let mut r = Reader::new(body);
        let kind = r.u8()?;
        let name = r.cstr()?;
        match kind {
            b'S' => {
                let stmt = self.session.prepared(&name).ok_or_else(|| {
                    SqlError::new(code::INVALID_SQL_STATEMENT_NAME, format!("prepared statement \"{name}\" does not exist"))
                })?;
                let oids: Vec<u32> = stmt.param_types.iter().map(Type::oid).collect();
                self.out.parameter_description(&oids);
                match &stmt.columns {
                    Some(cols) => self.row_description(cols, &[]),
                    None => self.out.no_data(),
                }
            }
            b'P' => {
                let portal = self
                    .portals
                    .get(&name)
                    .ok_or_else(|| SqlError::new(code::INVALID_CURSOR_NAME, format!("portal \"{name}\" does not exist")))?;
                let formats = portal.result_formats.clone();
                match portal.stmt.columns.clone() {
                    Some(cols) => {
                        check_formats(&formats, cols.len())?;
                        self.row_description(&cols, &formats);
                    }
                    None => self.out.no_data(),
                }
            }
            _ => return Err(SqlError::new(code::PROTOCOL_VIOLATION, format!("invalid DESCRIBE message subtype {kind}"))),
        }
        Ok(())
    }

    fn execute(&mut self, body: &[u8]) -> SqlResult<()> {
        let mut r = Reader::new(body);
        let name = r.cstr()?;
        let max_rows = usize::try_from(r.i32()?.max(0)).unwrap_or(0);
        let mut portal = self
            .portals
            .remove(&name)
            .ok_or_else(|| SqlError::new(code::INVALID_CURSOR_NAME, format!("portal \"{name}\" does not exist")))?;
        if portal.result.is_none() {
            {
                self.session.begin_pipeline();
                let res = self.session.execute(&portal.stmt.stmt, &portal.stmt.param_types, &portal.params)?;
                self.send_result_preamble(&res);
                portal.result = Some(res);
            }
        }
        if let Some(pending) = portal.result.as_mut().and_then(|r| r.copy_in.take()) {
            self.out.copy_response(b'G', pending.binary, pending.columns);
            self.copy = Some(CopyState { pending, data: Vec::new(), rest: None });
            self.portals.insert(name, portal);
            return Ok(());
        }
        if let Some(out) = portal.result.as_mut().and_then(|r| r.copy_out.take()) {
            let tag = portal.result.as_ref().map(|r| r.tag.clone()).unwrap_or_default();
            self.send_copy_out(&out);
            self.out.command_complete(&tag);
            self.portals.insert(name, portal);
            return Ok(());
        }
        let res = portal.result.as_ref().ok_or_else(|| SqlError::internal("portal without result"))?;
        if res.empty {
            self.out.empty_query();
            return Ok(());
        }
        if res.returns_rows {
            check_formats(&portal.result_formats, res.columns.len())?;
            let end = if max_rows == 0 { res.rows.len() } else { (portal.sent + max_rows).min(res.rows.len()) };
            let cols = res.columns.clone();
            let rows: Vec<Vec<Value>> = res.rows[portal.sent..end].to_vec();
            for row in &rows {
                self.data_row(&cols, row, &portal.result_formats)?;
            }
            portal.sent = end;
            if portal.sent < res.rows.len() {
                self.out.portal_suspended();
                self.portals.insert(name, portal);
                return Ok(());
            }
        }
        let tag = res.tag.clone();
        self.out.command_complete(&tag);
        // A completed portal can be executed again only to report completion; keep it so a
        // repeated Execute does not fail (Postgres returns an empty result).
        self.portals.insert(name, portal);
        Ok(())
    }

    fn close(&mut self, body: &[u8]) -> SqlResult<()> {
        let mut r = Reader::new(body);
        let kind = r.u8()?;
        let name = r.cstr()?;
        match kind {
            b'S' => {
                self.session.close_prepared(&name);
            }
            b'P' => {
                self.portals.remove(&name);
                self.session.close_portal(&name);
            }
            _ => return Err(SqlError::new(code::PROTOCOL_VIOLATION, format!("invalid CLOSE message subtype {kind}"))),
        }
        self.out.close_complete();
        Ok(())
    }
}

fn check_formats(formats: &[i16], columns: usize) -> SqlResult<()> {
    if formats.len() > 1 && formats.len() != columns {
        return Err(SqlError::new(
            code::PROTOCOL_VIOLATION,
            format!("bind message has {} result formats but query has {columns} columns", formats.len()),
        ));
    }
    if let Some(f) = formats.iter().find(|f| **f != 0 && **f != 1) {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("unsupported format code: {f}")));
    }
    Ok(())
}
