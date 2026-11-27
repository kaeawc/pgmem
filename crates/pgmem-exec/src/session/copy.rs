//! COPY (`copy.c` `DoCopy`): COPY TO runs a SELECT over the table (or the given query) and
//! formats its rows; COPY FROM parses the data and inserts it with one INSERT, so defaults,
//! constraints and triggers apply as for any insert. COPY FROM STDIN runs twice: first to
//! validate and answer CopyInResponse, then (`copy_from`) with the data the client sent.

use super::Session;
use crate::copy::{self, Field, Kind, Options, Verbosity};
use crate::session::{Notice, QueryResult};
use pgmem_catalog::{Relation, TableDef};
use pgmem_ir::{self as ir, Statement};
use pgmem_types::{Format, SqlError, SqlResult, Type, Value, code};

/// A COPY FROM STDIN waiting for its data.
#[derive(Clone, Debug)]
pub struct CopyIn {
    pub binary: bool,
    pub columns: usize,
    stmt: ir::Copy,
}

/// The output of COPY TO STDOUT, one CopyData message per chunk.
#[derive(Clone, Debug)]
pub struct CopyOut {
    pub binary: bool,
    pub columns: usize,
    pub data: Vec<Vec<u8>>,
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn wrong_type(msg: String) -> SqlError {
    SqlError::new(code::WRONG_OBJECT_TYPE, msg)
}

/// `strerror` wording for a file error, and its SQLSTATE.
fn io_error(e: &std::io::Error) -> (&'static str, String) {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => (code::UNDEFINED_FILE, "No such file or directory".into()),
        K::PermissionDenied => (code::INSUFFICIENT_PRIVILEGE, "Permission denied".into()),
        K::IsADirectory => (code::IO_ERROR, "Is a directory".into()),
        _ => {
            let s = e.to_string();
            (code::IO_ERROR, s.split(" (os error").next().unwrap_or(&s).to_owned())
        }
    }
}

/// The copied columns: the list given, or every column that is not generated.
fn columns(t: &TableDef, list: &[String]) -> SqlResult<Vec<(String, Type)>> {
    let param_type = |ty: &Type| match ty {
        Type::Domain(oid) => pgmem_types::usertype::domain_base(*oid).map_or_else(|| ty.clone(), |(b, _)| b),
        other => other.clone(),
    };
    if list.is_empty() {
        return Ok(t.live_columns().filter(|(_, c)| c.generated.is_none()).map(|(_, c)| (c.name.clone(), param_type(&c.ty))).collect());
    }
    let mut out: Vec<(String, Type)> = Vec::new();
    for name in list {
        let Some((_, c)) = t.live_columns().find(|(_, c)| c.name == *name) else {
            return Err(SqlError::new(code::UNDEFINED_COLUMN, format!("column \"{name}\" of relation \"{}\" does not exist", t.name)));
        };
        if out.iter().any(|(n, _)| n == name) {
            return Err(SqlError::new(code::DUPLICATE_COLUMN, format!("column \"{name}\" specified more than once")));
        }
        if c.generated.is_some() {
            return Err(SqlError::new(code::INVALID_COLUMN_REFERENCE, format!("column \"{name}\" is a generated column"))
                .with_detail("Generated columns cannot be used in COPY."));
        }
        out.push((name.clone(), param_type(&c.ty)));
    }
    Ok(out)
}

/// FORCE_QUOTE / FORCE_NOT_NULL / FORCE_NULL columns must be copied ones (and, copying a
/// table, columns of it).
fn check_force(o: &Options, table: Option<&TableDef>, names: &[String]) -> SqlResult<()> {
    for (force, what) in [(&o.force_quote, "FORCE_QUOTE"), (&o.force_not_null, "FORCE_NOT_NULL"), (&o.force_null, "FORCE_NULL")] {
        let copy::Force::Columns(cols) = force else { continue };
        if let Some(t) = table {
            columns(t, cols)?;
        }
        if let Some(c) = cols.iter().find(|c| !names.contains(c)) {
            return Err(SqlError::new(code::INVALID_COLUMN_REFERENCE, format!("{what} column \"{c}\" not referenced by COPY")));
        }
    }
    Ok(())
}

/// A decoded input row: a value or the column default per field.
enum Cell {
    Value(Value),
    Default,
}

/// Why a row could not be decoded: a value its type rejects (skipped by ON_ERROR IGNORE:
/// field index, text, error) or a malformed row.
enum RowError {
    Value(usize, String, SqlError),
    Row(SqlError),
}

fn decode_row(fields: Vec<Field>, types: &[(String, Type)]) -> Result<Vec<Cell>, RowError> {
    let n = fields.len();
    let mut out = Vec::with_capacity(n);
    for (i, (f, (_, ty))) in fields.into_iter().zip(types).enumerate() {
        out.push(match f {
            Field::Null => Cell::Value(Value::Null),
            Field::Default => Cell::Default,
            Field::Text(s) => match pgmem_types::decode(s.as_bytes(), ty, Format::Text) {
                Ok(v) => Cell::Value(v),
                Err(e) => return Err(RowError::Value(i, s, e)),
            },
        });
    }
    match types.get(n) {
        Some((c, _)) => Err(RowError::Row(copy::missing_column(c))),
        None => Ok(out),
    }
}

fn count_tag(tag: &str) -> u64 {
    tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

impl Session {
    pub(super) fn copy(&mut self, c: &ir::Copy) -> SqlResult<QueryResult> {
        if let ir::CopyTarget::Program(_) = c.target {
            return Err(SqlError::feature_not_supported("COPY ... PROGRAM is not supported by pgmem"));
        }
        if c.from && c.query.is_some() {
            return Err(SqlError::syntax("syntax error at or near \"FROM\""));
        }
        if !c.from && c.where_clause.is_some() {
            return Err(SqlError::syntax("WHERE clause not allowed with COPY TO"));
        }
        if let ir::CopyTarget::File(path) = &c.target {
            self.check_file_access(c.from)?;
            if !c.from && !std::path::Path::new(path).is_absolute() {
                return Err(SqlError::new(code::INVALID_NAME, "relative path not allowed for COPY to file"));
            }
        }
        let Some(name) = &c.table else {
            let opts = copy::options(&c.options, false)?;
            return self.copy_to(c, None, &opts);
        };
        {
            let catalog = self.catalog_snapshot();
            if let Ok(Some(rel)) = catalog.find_relation(name) {
                crate::roles::check_copy(&catalog, &self.env(), &rel, &c.columns, c.from)?;
            }
        }
        let table = self.copy_table(name, c.from)?;
        if !c.from {
            let t = table?;
            let opts = copy::options(&c.options, false)?;
            return self.copy_to(c, Some(&t), &opts);
        }
        let opts = copy::options(&c.options, true)?;
        match table {
            Ok(t) => self.copy_into(c, &t, &opts),
            // Like Postgres, a relation COPY FROM cannot fill fails once the copy has begun.
            Err(e) => match (&c.target, self.copy_data.take()) {
                (ir::CopyTarget::Stdio, None) => Ok(QueryResult {
                    copy_in: Some(Box::new(CopyIn { binary: opts.kind == Kind::Binary, columns: c.columns.len(), stmt: c.clone() })),
                    ..Default::default()
                }),
                _ => Err(e),
            },
        }
    }

    /// Finishes a COPY FROM STDIN with the data the client sent (or its CopyFail message).
    pub fn copy_from(&mut self, pending: &CopyIn, data: Result<Vec<u8>, String>) -> SqlResult<QueryResult> {
        self.copy_data = Some(data);
        let r = self.execute(&Statement::Copy(Box::new(pending.stmt.clone())), &[], &[]);
        self.copy_data = None;
        r
    }

    fn check_file_access(&self, from: bool) -> SqlResult<()> {
        let env = self.env();
        let catalog = self.catalog_snapshot();
        let role = crate::roles::current_role(&env);
        if crate::roles::is_superuser(&catalog, &env, &role) {
            return Ok(());
        }
        let (what, granted) = if from { ("from", "pg_read_server_files") } else { ("to", "pg_write_server_files") };
        if catalog.roles_of(&role).iter().any(|r| r == granted) {
            return Ok(());
        }
        Err(SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied to COPY {what} a file"))
            .with_detail(format!("Only roles with privileges of the \"{granted}\" role may COPY {what} a file."))
            .with_hint("Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone."))
    }

    /// The table to copy (outer error: no such relation; inner: one COPY cannot use).
    fn copy_table(&self, name: &ir::ObjectName, from: bool) -> SqlResult<SqlResult<TableDef>> {
        let catalog = self.catalog_snapshot();
        let rel = catalog.find_relation(name)?.ok_or_else(|| SqlError::undefined_table(&name.to_string()))?;
        let base = name.base();
        Ok(match rel {
            Relation::Table(t) if !from && catalog.is_partitioned(t.oid) => {
                Err(wrong_type(format!("cannot copy from partitioned table \"{base}\"")).with_hint("Try the COPY (SELECT ...) TO variant."))
            }
            Relation::Table(t) => match &t.matview {
                None => Ok((*t).clone()),
                Some(_) if from => Err(wrong_type(format!("cannot copy to materialized view \"{base}\""))),
                Some(m) if !m.populated => Err(SqlError::new(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("cannot copy from unpopulated materialized view \"{base}\""),
                )
                .with_hint("Use the REFRESH MATERIALIZED VIEW command.")),
                Some(_) => Ok((*t).clone()),
            },
            Relation::View(_) if from => Err(wrong_type(format!("cannot copy to view \"{base}\""))
                .with_hint("To enable copying to a view, provide an INSTEAD OF INSERT trigger.")),
            Relation::View(_) => {
                Err(wrong_type(format!("cannot copy from view \"{base}\"")).with_hint("Try the COPY (SELECT ...) TO variant."))
            }
            Relation::Sequence(_) => Err(wrong_type(format!("cannot copy {} sequence \"{base}\"", if from { "to" } else { "from" }))),
            Relation::Index { .. } => {
                Err(wrong_type(format!("cannot copy {} non-table relation \"{base}\"", if from { "to" } else { "from" })))
            }
        })
    }

    fn copy_to(&mut self, c: &ir::Copy, table: Option<&TableDef>, o: &Options) -> SqlResult<QueryResult> {
        let query = match (table, &c.query) {
            (Some(t), _) => {
                let cols = columns(t, &c.columns)?;
                let list: Vec<String> = cols.iter().map(|(n, _)| quote_ident(n)).collect();
                let sql = format!("SELECT {} FROM {}.{}", list.join(", "), quote_ident(&t.schema), quote_ident(&t.name));
                pgmem_parse::parse_one(&sql)?
            }
            (None, Some(q)) => {
                let returning = match q.as_ref() {
                    Statement::Insert(i) => Some(i.returning.is_empty()),
                    Statement::Update(u) => Some(u.returning.is_empty()),
                    Statement::Delete(d) => Some(d.returning.is_empty()),
                    Statement::Merge(m) => Some(m.returning.is_empty()),
                    _ => None,
                };
                if returning == Some(true) {
                    return Err(SqlError::feature_not_supported("COPY query must have a RETURNING clause"));
                }
                (**q).clone()
            }
            (None, None) => return Err(SqlError::internal("COPY without a source")),
        };
        let r = self.run_in_txn(&query, &[], &[])?;
        let names: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
        check_force(o, table, &names)?;
        let data = format_rows(o, &r, &names)?;
        let n = r.rows.len();
        match &c.target {
            ir::CopyTarget::File(path) => {
                std::fs::write(path, data.concat()).map_err(|e| {
                    let (code, msg) = io_error(&e);
                    SqlError::new(code, format!("could not open file \"{path}\" for writing: {msg}")).with_hint(
                        "COPY TO instructs the PostgreSQL server process to write a file. You may want a client-side facility such as psql's \\copy.",
                    )
                })?;
                Ok(QueryResult { tag: format!("COPY {n}"), ..Default::default() })
            }
            _ => Ok(QueryResult {
                tag: format!("COPY {n}"),
                copy_out: Some(Box::new(CopyOut { binary: o.kind == Kind::Binary, columns: names.len(), data })),
                ..Default::default()
            }),
        }
    }

    fn copy_into(&mut self, c: &ir::Copy, t: &TableDef, o: &Options) -> SqlResult<QueryResult> {
        crate::rls::check_copy_from(&self.catalog_snapshot(), &self.env(), t)?;
        let cols = columns(t, &c.columns)?;
        let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
        check_force(o, Some(t), &names)?;
        if let Some(w) = &c.where_clause {
            let mut sub = false;
            w.walk(&mut |e| {
                sub |=
                    matches!(e, ir::Expr::Subquery(_) | ir::Expr::Exists { .. } | ir::Expr::InSubquery { .. } | ir::Expr::ArraySubquery(_))
            });
            if sub {
                return Err(SqlError::feature_not_supported("cannot use subquery in COPY FROM WHERE condition"));
            }
        }
        if o.freeze {
            return Err(SqlError::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot perform COPY FREEZE because the table was not created or truncated in the current subtransaction",
            ));
        }
        let data = match &c.target {
            ir::CopyTarget::File(path) => std::fs::read(path).map_err(|e| {
                let (code, msg) = io_error(&e);
                SqlError::new(code, format!("could not open file \"{path}\" for reading: {msg}")).with_hint(
                    "COPY FROM instructs the PostgreSQL server process to read a file. You may want a client-side facility such as psql's \\copy.",
                )
            })?,
            _ => match self.copy_data.take() {
                None => {
                    return Ok(QueryResult {
                        copy_in: Some(Box::new(CopyIn { binary: o.kind == Kind::Binary, columns: cols.len(), stmt: c.clone() })),
                        ..Default::default()
                    });
                }
                Some(Err(msg)) => return Err(SqlError::new(code::QUERY_CANCELED, format!("COPY from stdin failed: {msg}"))),
                Some(Ok(d)) => d,
            },
        };
        let mut notices = Vec::new();
        let mut rows = self.decode_input(o, &data, &cols, &mut notices)?;
        if let Some(w) = &c.where_clause {
            rows = self.filter_rows(rows, w, t, &cols)?;
        }
        let mut result = self.insert_rows(c, t, &cols, rows)?;
        result.notices.splice(0..0, notices);
        Ok(result)
    }

    fn decode_input(&self, o: &Options, data: &[u8], cols: &[(String, Type)], notices: &mut Vec<Notice>) -> SqlResult<Vec<Vec<Cell>>> {
        if o.kind == Kind::Binary {
            let mut rows = Vec::new();
            for raw in copy::read_binary(data, cols.len())? {
                let mut row = Vec::with_capacity(raw.len());
                for (f, (_, ty)) in raw.iter().zip(cols) {
                    row.push(Cell::Value(match f {
                        None => Value::Null,
                        Some(b) => pgmem_types::decode(b, ty, Format::Binary)?,
                    }));
                }
                rows.push(row);
            }
            return Ok(rows);
        }
        let names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
        let mut rows = Vec::new();
        let mut skipped = 0i64;
        for r in copy::read_text(o, data, &names)? {
            match decode_row(r.fields, cols) {
                Ok(row) => rows.push(row),
                Err(RowError::Row(e)) => return Err(e),
                Err(RowError::Value(_, _, e)) if !o.ignore_errors => return Err(e),
                Err(RowError::Value(i, value, _)) => {
                    skipped += 1;
                    if o.reject_limit.is_some_and(|l| skipped > l) {
                        return Err(SqlError::new(
                            code::INVALID_TEXT_REPRESENTATION,
                            format!(
                                "skipped more than REJECT_LIMIT ({}) rows due to data type incompatibility",
                                o.reject_limit.unwrap_or(0)
                            ),
                        ));
                    }
                    if o.verbosity == Verbosity::Verbose {
                        notices.push(notice(format!(
                            "skipping row due to data type incompatibility at line {} for column \"{}\": \"{value}\"",
                            r.line, cols[i].0
                        )));
                    }
                }
            }
        }
        if skipped > 0 && o.verbosity != Verbosity::Silent {
            notices.push(notice(if skipped == 1 {
                "1 row was skipped due to data type incompatibility".to_owned()
            } else {
                format!("{skipped} rows were skipped due to data type incompatibility")
            }));
        }
        Ok(rows)
    }

    /// Keeps the rows the WHERE condition accepts (evaluated over the copied columns).
    fn filter_rows(&mut self, rows: Vec<Vec<Cell>>, cond: &ir::Expr, t: &TableDef, cols: &[(String, Type)]) -> SqlResult<Vec<Vec<Cell>>> {
        if rows.is_empty() {
            return Ok(rows);
        }
        let list: Vec<String> = cols.iter().map(|(n, _)| quote_ident(n)).collect();
        let nulls = vec!["NULL"; cols.len()].join(", ");
        let sql = format!("SELECT true FROM (VALUES ({nulls})) AS {}({})", quote_ident(&t.name), list.join(", "));
        let Statement::Query(mut q) = pgmem_parse::parse_one(&sql)? else { return Err(SqlError::internal("COPY WHERE query")) };
        let (types, params, values) = values_list(&rows, cols, false);
        if let ir::SetExpr::Select(s) = &mut q.body {
            s.projection = vec![ir::SelectItem::Expr { expr: cond.clone(), alias: None }];
            if let Some(ir::TableRef::Subquery { query, .. }) = s.from.first_mut() {
                query.body = ir::SetExpr::Values(values);
            }
        }
        let r = self.run_in_txn(&Statement::Query(q), &types, &params)?;
        Ok(rows.into_iter().zip(r.rows).filter(|(_, keep)| keep.first() == Some(&Value::Bool(true))).map(|(row, _)| row).collect())
    }

    fn insert_rows(&mut self, c: &ir::Copy, t: &TableDef, cols: &[(String, Type)], rows: Vec<Vec<Cell>>) -> SqlResult<QueryResult> {
        if rows.is_empty() {
            return Ok(QueryResult { tag: "COPY 0".into(), ..Default::default() });
        }
        let (types, params, values) = values_list(&rows, cols, true);
        let insert = ir::Insert {
            with: None,
            table: c.table.clone().unwrap_or_else(|| ir::ObjectName(vec![t.schema.clone(), t.name.clone()])),
            alias: None,
            columns: cols.iter().map(|(n, _)| n.clone()).collect(),
            source: ir::InsertSource::Values(values),
            overriding_system_value: true,
            overriding_user_value: false,
            on_conflict: None,
            returning: Vec::new(),
        };
        let r = self.run_in_txn(&Statement::Insert(Box::new(insert)), &types, &params)?;
        Ok(QueryResult { tag: format!("COPY {}", count_tag(&r.tag)), notices: r.notices, ..Default::default() })
    }
}

fn notice(message: String) -> Notice {
    Notice { severity: "NOTICE", code: "00000", message }
}

/// The rows as a VALUES list of parameters: (parameter types, parameters, rows). Defaults
/// become DEFAULT when `defaults` is set, else NULL.
fn values_list(rows: &[Vec<Cell>], cols: &[(String, Type)], defaults: bool) -> (Vec<Type>, Vec<Value>, Vec<Vec<ir::Expr>>) {
    let mut types = Vec::new();
    let mut params = Vec::new();
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        let mut exprs = Vec::with_capacity(row.len());
        for (cell, (_, ty)) in row.iter().zip(cols) {
            match cell {
                Cell::Default if defaults => exprs.push(ir::Expr::Default),
                Cell::Default => {
                    types.push(ty.clone());
                    params.push(Value::Null);
                    exprs.push(ir::Expr::Param(params.len()));
                }
                Cell::Value(v) => {
                    types.push(ty.clone());
                    params.push(v.clone());
                    exprs.push(ir::Expr::Param(params.len()));
                }
            }
        }
        values.push(exprs);
    }
    (types, params, values)
}

/// COPY TO's data: the header, then one chunk per row.
fn format_rows(o: &Options, r: &QueryResult, names: &[String]) -> SqlResult<Vec<Vec<u8>>> {
    let mut data = Vec::with_capacity(r.rows.len() + 2);
    if o.kind == Kind::Binary {
        data.push(copy::binary_header());
        for row in &r.rows {
            let mut fields = Vec::with_capacity(row.len());
            for (v, col) in row.iter().zip(&r.columns) {
                fields.push(if v.is_null() { None } else { Some(pgmem_types::encode(v, &col.ty, Format::Binary)?) });
            }
            data.push(copy::binary_row(&fields));
        }
        data.push(copy::binary_trailer());
        return Ok(data);
    }
    if o.header != copy::Header::No {
        data.push(copy::header_line(o, names));
    }
    let force: Vec<bool> = names.iter().map(|n| o.force_quote.applies(n)).collect();
    for row in &r.rows {
        let mut fields = Vec::with_capacity(row.len());
        for (v, col) in row.iter().zip(&r.columns) {
            fields.push(if v.is_null() {
                None
            } else {
                Some(String::from_utf8_lossy(&pgmem_types::encode(v, &col.ty, Format::Text)?).into_owned())
            });
        }
        data.push(copy::text_row(o, &fields, &force));
    }
    Ok(data)
}
