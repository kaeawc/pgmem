//! The PL/pgSQL interpreter (`pl_exec.c`): variables, control flow, exceptions, RAISE, and
//! SQL run through [`crate::functions::Exec`] with variables passed as parameters.

use super::conditions::{condition_code, condition_matches, intern_code};
use super::parse::{Block, Decl, Query, Raise, Stmt};
use crate::functions::Exec;
use crate::plan::Row;
use crate::session::Notice;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

/// A variable: a scalar, or a row (record / `%ROWTYPE` / NEW / OLD) with named fields.
#[derive(Clone, Debug)]
pub struct Var {
    pub name: String,
    pub ty: Type,
    pub typmod: i32,
    pub value: Value,
    pub constant: bool,
    pub not_null: bool,
    /// Row variables: field names and types (empty until a record is assigned).
    pub fields: Option<Vec<(String, Type)>>,
}

impl Var {
    pub fn scalar(name: &str, ty: Type, value: Value) -> Var {
        Var { name: name.to_owned(), ty, typmod: -1, value, constant: false, not_null: false, fields: None }
    }

    pub fn row(name: &str, fields: Vec<(String, Type)>, values: Option<Row>) -> Var {
        let value = values.map_or(Value::Null, |v| Value::Record(pgmem_types::usertype::RECORD_OID, v));
        Var { name: name.to_owned(), ty: Type::Record, typmod: -1, value, constant: false, not_null: false, fields: Some(fields) }
    }

    fn row_values(&self) -> Option<&Vec<Value>> {
        match &self.value {
            Value::Record(_, v) => Some(v),
            _ => None,
        }
    }
}

pub enum Flow {
    Normal,
    Exit(Option<String>),
    Continue(Option<String>),
    Return,
}

pub struct Interp<'x, 'b, 'a> {
    exec: &'x mut Exec<'b, 'a>,
    vars: Vec<Var>,
    /// The function returns a set (RETURN NEXT / RETURN QUERY accumulate here).
    pub set: Vec<Row>,
    pub result: Option<Value>,
    /// The returned row of a trigger or record function.
    pub result_row: Option<Option<Row>>,
    row_count: i64,
    /// The exception a handler is running for (SQLSTATE / SQLERRM, re-raise).
    current_error: Vec<SqlError>,
    savepoints: usize,
}

fn tokens(text: &str) -> SqlResult<Vec<Token>> {
    Tokenizer::new(&PostgreSqlDialect {}, text).tokenize().map_err(|e| SqlError::syntax(e.to_string()))
}

impl<'x, 'b, 'a> Interp<'x, 'b, 'a> {
    pub fn new(exec: &'x mut Exec<'b, 'a>, vars: Vec<Var>) -> Self {
        let mut vars = vars;
        vars.push(Var::scalar("found", Type::Bool, Value::Bool(false)));
        Interp { exec, vars, set: Vec::new(), result: None, result_row: None, row_count: 0, current_error: Vec::new(), savepoints: 0 }
    }

    fn var_index(&self, name: &str) -> Option<usize> {
        self.vars.iter().rposition(|v| v.name == name)
    }

    pub fn var(&self, name: &str) -> Option<&Var> {
        self.var_index(name).map(|i| &self.vars[i])
    }

    fn set_found(&mut self, found: bool) {
        if let Some(i) = self.var_index("found") {
            self.vars[i].value = Value::Bool(found);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Expressions and SQL
    // -----------------------------------------------------------------------------------------

    /// Replaces variable references in `text` with `$n` parameters: plain names, `row.field`,
    /// and the function's own `$n`.
    fn substitute(&self, text: &str) -> SqlResult<(String, Vec<Type>, Vec<Value>)> {
        let toks = tokens(text)?;
        let sig: Vec<usize> = toks.iter().enumerate().filter(|(_, t)| !matches!(t, Token::Whitespace(_))).map(|(i, _)| i).collect();
        let mut out = String::new();
        let mut types: Vec<Type> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        let mut param = |key: String, ty: Type, v: Value, types: &mut Vec<Type>, values: &mut Vec<Value>| -> String {
            let n = match keys.iter().position(|k| *k == key) {
                Some(i) => i + 1,
                None => {
                    keys.push(key);
                    types.push(ty);
                    values.push(v);
                    keys.len()
                }
            };
            format!("${n}")
        };
        let mut skip_until = 0usize;
        for (k, &i) in sig.iter().enumerate() {
            // Emit the whitespace between tokens.
            let start = if k == 0 { 0 } else { sig[k - 1] + 1 };
            if i < skip_until {
                continue;
            }
            for t in &toks[start..i] {
                out.push_str(&t.to_string());
            }
            let prev_dot = k > 0 && toks[sig[k - 1]] == Token::Period;
            let next = |o: usize| sig.get(k + o).map(|&j| &toks[j]);
            match &toks[i] {
                Token::Placeholder(p) if p.starts_with('$') => {
                    let n: usize = p[1..].parse().unwrap_or(0);
                    let key = format!("${n}");
                    match self.var(&key) {
                        Some(v) => {
                            let s = param(key, v.ty.clone(), v.value.clone(), &mut types, &mut values);
                            out.push_str(&s);
                        }
                        None => out.push_str(p),
                    }
                }
                Token::Word(w) if !prev_dot && next(1) != Some(&Token::LParen) => {
                    let name = if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() };
                    match self.var(&name) {
                        // `row.field`.
                        Some(v) if v.fields.is_some() && next(1) == Some(&Token::Period) => {
                            let Some(Token::Word(f)) = next(2) else {
                                out.push_str(&toks[i].to_string());
                                continue;
                            };
                            let field = if f.quote_style.is_some() { f.value.clone() } else { f.value.to_lowercase() };
                            let fields = v.fields.clone().unwrap_or_default();
                            let Some(pos) = fields.iter().position(|(n, _)| *n == field) else {
                                return Err(SqlError::new(
                                    code::UNDEFINED_COLUMN,
                                    format!("record \"{}\" has no field \"{field}\"", v.name),
                                ));
                            };
                            let value = match v.row_values() {
                                Some(vals) => vals.get(pos).cloned().unwrap_or(Value::Null),
                                None if v.name == "new" || v.name == "old" => Value::Null,
                                None => {
                                    return Err(SqlError::new(
                                        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                        format!("record \"{}\" is not assigned yet", v.name),
                                    )
                                    .with_detail("The tuple structure of a not-yet-assigned record is indeterminate."));
                                }
                            };
                            let s = param(format!("{name}.{field}"), fields[pos].1.clone(), value, &mut types, &mut values);
                            out.push_str(&s);
                            skip_until = sig[k + 2] + 1;
                        }
                        Some(v) if v.fields.is_none() => {
                            let s = param(name, v.ty.clone(), v.value.clone(), &mut types, &mut values);
                            out.push_str(&s);
                        }
                        _ => out.push_str(&toks[i].to_string()),
                    }
                }
                t => out.push_str(&t.to_string()),
            }
        }
        Ok((out, types, values))
    }

    /// Runs one SQL statement (variables substituted).
    fn run_sql(&mut self, text: &str) -> SqlResult<crate::session::QueryResult> {
        let (sql, types, values) = self.substitute(text)?;
        let mut stmts = pgmem_parse::parse(&sql)?;
        if stmts.len() != 1 {
            return Err(SqlError::syntax(format!("syntax error at or near \"{sql}\"")));
        }
        let stmt = stmts.remove(0);
        let r = self.exec.statement(&stmt, &types, &values)?;
        self.row_count = row_count(&r);
        Ok(r)
    }

    /// Evaluates an expression: its value and type.
    fn eval_typed(&mut self, expr: &str) -> SqlResult<(Value, Type)> {
        let r = self.run_sql(&format!("SELECT {expr}"))?;
        let ty = r.columns.first().map_or(Type::Unknown, |c| c.ty.clone());
        if r.columns.len() > 1 {
            return Err(SqlError::syntax(format!("query \"SELECT {expr}\" returned {} columns", r.columns.len())));
        }
        Ok((r.rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null), ty))
    }

    fn eval(&mut self, expr: &str) -> SqlResult<Value> {
        Ok(self.eval_typed(expr)?.0)
    }

    pub fn eval_bool(&mut self, expr: &str) -> SqlResult<bool> {
        let v = self.eval(expr)?;
        match v {
            Value::Bool(b) => Ok(b),
            Value::Null => Ok(false),
            other => crate::cast::cast_value(other, &Type::Bool, -1).map(|v| v.is_true()),
        }
    }

    /// Assigns `value` to `target` (`x`, `row.field`), with an assignment cast.
    fn assign(&mut self, target: &str, value: Value) -> SqlResult<()> {
        let (name, field) = match target.split_once('.') {
            Some((n, f)) => (n.to_owned(), Some(f.to_owned())),
            None => (target.to_owned(), None),
        };
        let i = self.var_index(&name).ok_or_else(|| SqlError::syntax(format!("\"{name}\" is not a known variable")))?;
        let v = &mut self.vars[i];
        if v.constant {
            return Err(SqlError::syntax(format!("variable \"{name}\" is declared CONSTANT")));
        }
        match field {
            Some(f) => {
                let fields = v.fields.clone().ok_or_else(|| SqlError::syntax(format!("\"{name}.{f}\" is not a known variable")))?;
                let pos = fields
                    .iter()
                    .position(|(n, _)| *n == f)
                    .ok_or_else(|| SqlError::new(code::UNDEFINED_COLUMN, format!("record \"{name}\" has no field \"{f}\"")))?;
                let value = crate::cast::cast_value_ctx(value, &fields[pos].1, -1, false)?;
                let mut vals = v.row_values().cloned().unwrap_or_else(|| vec![Value::Null; fields.len()]);
                vals[pos] = value;
                v.value = Value::Record(pgmem_types::usertype::RECORD_OID, vals);
            }
            None if v.fields.is_some() => {
                // A whole row: a record value.
                match value {
                    Value::Record(_, vals) => v.value = Value::Record(pgmem_types::usertype::RECORD_OID, vals),
                    Value::Null => v.value = Value::Null,
                    _ => return Err(SqlError::datatype_mismatch("cannot assign non-composite value to a record variable".to_owned())),
                }
            }
            None => {
                let value = crate::cast::cast_value_ctx(value, &v.ty, v.typmod, false)?;
                if v.not_null && value.is_null() {
                    return Err(SqlError::new(
                        code::NULL_VALUE_NOT_ALLOWED,
                        format!("null value cannot be assigned to variable \"{name}\" declared NOT NULL"),
                    ));
                }
                v.value = value;
            }
        }
        Ok(())
    }

    /// Assigns a result row to INTO targets: one row variable takes the whole row, scalars
    /// take the columns in order.
    fn assign_row(&mut self, targets: &[String], cols: &[(String, Type)], row: Option<Row>) -> SqlResult<()> {
        if let [t] = targets
            && let Some(i) = self.var_index(t)
            && self.vars[i].fields.is_some()
        {
            let v = &mut self.vars[i];
            if v.fields.as_ref().is_some_and(Vec::is_empty) || v.ty == Type::Record {
                v.fields = Some(cols.to_vec());
            }
            v.value = row.map_or(Value::Null, |r| Value::Record(pgmem_types::usertype::RECORD_OID, r));
            return Ok(());
        }
        let row = row.unwrap_or_else(|| vec![Value::Null; targets.len()]);
        for (t, v) in targets.iter().zip(row.into_iter().chain(std::iter::repeat(Value::Null))) {
            self.assign(t, v)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Statements
    // -----------------------------------------------------------------------------------------

    pub fn declare(&mut self, d: &Decl) -> SqlResult<()> {
        if let Some(target) = &d.alias {
            let src = self.var(target.trim()).cloned().ok_or_else(|| SqlError::syntax(format!("\"{target}\" is not a known variable")))?;
            self.vars.push(Var { name: d.name.clone(), ..src });
            return Ok(());
        }
        let lower = d.ty.to_lowercase().replace(' ', "");
        let mut var = if lower == "record" {
            Var::row(&d.name, Vec::new(), None)
        } else if let Some(table) = lower.strip_suffix("%rowtype") {
            let t = self.exec.txn().catalog().lookup_table(&ir_name(table))?;
            let fields = t.live_columns().map(|(_, c)| (c.name.clone(), c.ty.clone())).collect();
            Var::row(&d.name, fields, None)
        } else if let Some(src) = lower.strip_suffix("%type") {
            let (ty, typmod) = self.type_of_reference(src)?;
            Var { typmod, ..Var::scalar(&d.name, ty, Value::Null) }
        } else {
            let r = self.exec.statement(&parse_one(&format!("SELECT NULL::{}", d.ty))?, &[], &[])?;
            let c = r.columns.first().ok_or_else(|| SqlError::internal("type probe"))?;
            Var { typmod: c.typmod, ..Var::scalar(&d.name, c.ty.clone(), Value::Null) }
        };
        var.constant = d.constant;
        var.not_null = d.not_null;
        let value = match &d.default {
            Some(e) => {
                let v = self.eval(e)?;
                if var.fields.is_some() { v } else { crate::cast::cast_value_ctx(v, &var.ty, var.typmod, false)? }
            }
            None => Value::Null,
        };
        if var.not_null && value.is_null() {
            return Err(SqlError::new(
                code::NULL_VALUE_NOT_ALLOWED,
                format!("variable \"{}\" must have a default value, since it's declared NOT NULL", d.name),
            ));
        }
        var.value = value;
        self.vars.push(var);
        Ok(())
    }

    /// `var%TYPE` / `table.column%TYPE`.
    fn type_of_reference(&self, src: &str) -> SqlResult<(Type, i32)> {
        if let Some(v) = self.var(src) {
            return Ok((v.ty.clone(), v.typmod));
        }
        let (table, column) = src.rsplit_once('.').ok_or_else(|| SqlError::syntax(format!("invalid type name \"{src}%TYPE\"")))?;
        let t = self.exec.txn().catalog().lookup_table(&ir_name(table))?;
        let c = t.column(column).ok_or_else(|| SqlError::undefined_column(&format!("\"{column}\" of relation \"{table}\"")))?;
        Ok((c.ty.clone(), c.typmod))
    }

    pub fn run_block(&mut self, b: &Block) -> SqlResult<Flow> {
        let mark = self.vars.len();
        let result = self.run_block_inner(b);
        self.vars.truncate(mark);
        result
    }

    fn run_block_inner(&mut self, b: &Block) -> SqlResult<Flow> {
        if b.handlers.is_empty() {
            for d in &b.decls {
                self.declare(d)?;
            }
            return self.run_stmts(&b.body);
        }
        // A block with handlers runs in a subtransaction: an exception undoes its changes.
        let sp = format!("__pgmem_plpgsql_{}", self.savepoints);
        self.savepoints += 1;
        // Reading a query's transaction, the block's writes are those of functions it calls,
        // made in the transaction's writable copy.
        let side = match self.exec {
            Exec::Write { txn, .. } => {
                txn.savepoint(&sp);
                false
            }
            Exec::Read(c) | Exec::Volatile(c) => c.txn.side_savepoint(&sp)?,
        };
        let vars_mark = self.vars.len();
        let outcome = (|| {
            for d in &b.decls {
                self.declare(d)?;
            }
            self.run_stmts(&b.body)
        })();
        match outcome {
            Ok(flow) => {
                match self.exec {
                    Exec::Write { txn, .. } => txn.release(&sp)?,
                    Exec::Read(c) | Exec::Volatile(c) => c.txn.side_end(&sp, side, false)?,
                }
                self.savepoints -= 1;
                Ok(flow)
            }
            Err(e) => {
                match self.exec {
                    Exec::Write { txn, .. } => {
                        txn.rollback_to(&sp)?;
                        txn.release(&sp)?;
                    }
                    Exec::Read(c) | Exec::Volatile(c) => c.txn.side_end(&sp, side, true)?,
                }
                self.savepoints -= 1;
                self.vars.truncate(vars_mark);
                let Some(h) = b.handlers.iter().find(|h| h.conditions.iter().any(|c| condition_matches(c, e.code))) else {
                    return Err(e);
                };
                self.current_error.push(e.clone());
                let mark = self.vars.len();
                self.vars.push(Var::scalar("sqlstate", Type::Text, Value::Text(e.code.to_owned())));
                self.vars.push(Var::scalar("sqlerrm", Type::Text, Value::Text(e.message.clone())));
                let r = self.run_stmts(&h.body);
                self.vars.truncate(mark);
                self.current_error.pop();
                r
            }
        }
    }

    fn run_stmts(&mut self, stmts: &[Stmt]) -> SqlResult<Flow> {
        for s in stmts {
            match self.run_stmt(s)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    /// Runs a loop body once; Some(flow) when the loop must stop.
    fn loop_iteration(&mut self, label: Option<&String>, body: &[Stmt]) -> SqlResult<Option<Flow>> {
        Ok(match self.run_stmts(body)? {
            Flow::Normal => None,
            Flow::Exit(None) => Some(Flow::Normal),
            Flow::Exit(Some(l)) if Some(&l) == label => Some(Flow::Normal),
            Flow::Continue(None) => None,
            Flow::Continue(Some(l)) if Some(&l) == label => None,
            other => Some(other),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn run_stmt(&mut self, s: &Stmt) -> SqlResult<Flow> {
        match s {
            Stmt::Null => {}
            Stmt::Assign { target, expr } => {
                let v = self.eval(expr)?;
                self.assign(target, v)?;
            }
            Stmt::If { branches, otherwise } => {
                for (cond, body) in branches {
                    if self.eval_bool(cond)? {
                        return self.run_stmts(body);
                    }
                }
                return self.run_stmts(otherwise);
            }
            Stmt::Case { subject, whens, otherwise } => {
                let subject = subject.as_ref().map(|e| self.eval_typed(e)).transpose()?;
                for (e, body) in whens {
                    let hit = match &subject {
                        Some((v, _)) => {
                            // `WHEN a, b`: any of the listed values.
                            let mut hit = false;
                            for part in split_top_level(e) {
                                let w = self.eval(&part)?;
                                if !w.is_null() && !v.is_null() && pgmem_types::compare_values(v, &w)?.is_eq() {
                                    hit = true;
                                    break;
                                }
                            }
                            hit
                        }
                        None => self.eval_bool(e)?,
                    };
                    if hit {
                        return self.run_stmts(body);
                    }
                }
                match otherwise {
                    Some(body) => return self.run_stmts(body),
                    None => {
                        return Err(SqlError::new(code::CASE_NOT_FOUND, "case not found").with_hint("CASE statement is missing ELSE part."));
                    }
                }
            }
            Stmt::Loop { label, body } => loop {
                if let Some(f) = self.loop_iteration(label.as_ref(), body)? {
                    return Ok(f);
                }
            },
            Stmt::While { label, cond, body } => {
                while self.eval_bool(cond)? {
                    if let Some(f) = self.loop_iteration(label.as_ref(), body)? {
                        return Ok(f);
                    }
                }
            }
            Stmt::ForInt { label, var, reverse, lo, hi, step, body } => {
                let int = |v: Value, what: &str| -> SqlResult<i64> {
                    crate::cast::cast_value(v, &Type::Int8, -1)?
                        .as_i64()
                        .ok_or_else(|| SqlError::new(code::NULL_VALUE_NOT_ALLOWED, format!("{what} of FOR loop cannot be null")))
                };
                let lo = int(self.eval(lo)?, "lower bound")?;
                let hi = int(self.eval(hi)?, "upper bound")?;
                let step = match step {
                    Some(s) => int(self.eval(s)?, "BY value")?,
                    None => 1,
                };
                if step <= 0 {
                    return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "BY value of FOR loop must be greater than zero"));
                }
                let mark = self.vars.len();
                self.vars.push(Var::scalar(var, Type::Int4, Value::Null));
                // `FOR i IN REVERSE hi..lo` counts down from the first bound.
                let mut i = lo;
                let mut found = false;
                let flow = loop {
                    if (*reverse && i < hi) || (!*reverse && i > hi) {
                        break Flow::Normal;
                    }
                    found = true;
                    let idx = self.vars.len() - 1;
                    self.vars[idx].value = Value::Int4(i32::try_from(i).unwrap_or(i32::MAX));
                    if let Some(f) = self.loop_iteration(label.as_ref(), body)? {
                        break f;
                    }
                    i = if *reverse { i - step } else { i + step };
                };
                self.vars.truncate(mark);
                self.set_found(found);
                return Ok(flow);
            }
            Stmt::ForQuery { label, targets, query, body } => {
                let r = self.run_query(query)?;
                let cols: Vec<(String, Type)> = r.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
                let mut found = false;
                for row in r.rows {
                    found = true;
                    self.loop_targets(targets, &cols, row)?;
                    if let Some(f) = self.loop_iteration(label.as_ref(), body)? {
                        self.set_found(true);
                        return Ok(f);
                    }
                }
                self.set_found(found);
            }
            Stmt::ForEach { label, var, array, body } => {
                let arr = self.eval(array)?;
                let items = match arr {
                    Value::Array(items) | Value::BoundedArray(_, items) => {
                        pgmem_types::binary::array_flatten(&items).into_iter().cloned().collect::<Vec<_>>()
                    }
                    Value::Null => return Err(SqlError::new(code::NULL_VALUE_NOT_ALLOWED, "FOREACH expression must not be null")),
                    _ => return Err(SqlError::datatype_mismatch("FOREACH expression must yield an array, not type".to_owned())),
                };
                for item in items {
                    self.assign(var, item)?;
                    if let Some(f) = self.loop_iteration(label.as_ref(), body)? {
                        return Ok(f);
                    }
                }
            }
            Stmt::Exit { label, when, cont } => {
                if let Some(w) = when
                    && !self.eval_bool(w)?
                {
                    return Ok(Flow::Normal);
                }
                return Ok(if *cont { Flow::Continue(label.clone()) } else { Flow::Exit(label.clone()) });
            }
            Stmt::Return(e) => {
                match e {
                    // `RETURN NEW` / `RETURN OLD` / `RETURN NULL` from a trigger, or a row.
                    Some(name) if self.var(&name.to_lowercase()).is_some_and(|v| v.fields.is_some()) => {
                        let v = self.var(&name.to_lowercase()).cloned().unwrap_or_else(|| Var::scalar("", Type::Unknown, Value::Null));
                        self.result_row = Some(v.row_values().cloned());
                        self.result = Some(v.value);
                    }
                    Some(e) => {
                        let v = self.eval(e)?;
                        self.result_row = Some(match &v {
                            Value::Record(_, vals) => Some(vals.clone()),
                            Value::Null => None,
                            other => Some(vec![other.clone()]),
                        });
                        self.result = Some(v);
                    }
                    None => {}
                }
                return Ok(Flow::Return);
            }
            Stmt::ReturnNext(e) => {
                let row = match e {
                    Some(name) if self.var(&name.to_lowercase()).is_some_and(|v| v.fields.is_some()) => {
                        self.var(&name.to_lowercase()).and_then(|v| v.row_values().cloned()).unwrap_or_default()
                    }
                    Some(e) => match self.eval(e)? {
                        Value::Record(_, vals) => vals,
                        v => vec![v],
                    },
                    // OUT parameters form the row.
                    None => self.vars.iter().filter(|v| v.name.starts_with("out:")).map(|v| v.value.clone()).collect(),
                };
                self.set.push(row);
            }
            Stmt::ReturnQuery(q) => {
                let r = self.run_query(q)?;
                self.set_found(!r.rows.is_empty());
                self.set.extend(r.rows);
            }
            Stmt::Raise(r) => self.raise(r)?,
            Stmt::Assert { cond, message } => {
                if !self.eval_bool(cond)? {
                    let msg = match message {
                        Some(m) => display(&self.eval(m)?),
                        None => "assertion failed".to_owned(),
                    };
                    return Err(SqlError::new(code::ASSERT_FAILURE, msg));
                }
            }
            Stmt::Perform(q) => {
                let r = self.run_sql(&format!("SELECT {q}"))?;
                self.set_found(!r.rows.is_empty());
            }
            Stmt::Sql { text, into } => {
                let r = self.run_sql(text)?;
                match into {
                    Some((targets, strict)) => {
                        let cols: Vec<(String, Type)> = r.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
                        let n = r.rows.len();
                        if *strict && n == 0 {
                            return Err(SqlError::new(code::NO_DATA_FOUND, "query returned no rows"));
                        }
                        if *strict && n > 1 {
                            return Err(SqlError::new(code::TOO_MANY_ROWS, "query returned more than one row")
                                .with_hint("Make sure the query returns a single row, or use LIMIT 1."));
                        }
                        self.set_found(n > 0);
                        self.assign_row(targets, &cols, r.rows.into_iter().next())?;
                    }
                    None => {
                        let affected = row_count(&r) > 0 || !r.rows.is_empty();
                        if r.tag.starts_with("INSERT")
                            || r.tag.starts_with("UPDATE")
                            || r.tag.starts_with("DELETE")
                            || r.tag.starts_with("MERGE")
                        {
                            self.set_found(affected);
                        } else if r.returns_rows {
                            return Err(SqlError::syntax("query has no destination for result data")
                                .with_hint("If you want to discard the results of a SELECT, use PERFORM instead."));
                        }
                    }
                }
            }
            Stmt::Execute { sql, into, using } => {
                let text = display(&self.eval(sql)?);
                let r = self.run_dynamic(&text, using)?;
                if let Some((targets, strict)) = into {
                    let cols: Vec<(String, Type)> = r.columns.iter().map(|c| (c.name.clone(), c.ty.clone())).collect();
                    if *strict && r.rows.len() != 1 {
                        return Err(if r.rows.is_empty() {
                            SqlError::new(code::NO_DATA_FOUND, "query returned no rows")
                        } else {
                            SqlError::new(code::TOO_MANY_ROWS, "query returned more than one row")
                        });
                    }
                    self.assign_row(targets, &cols, r.rows.into_iter().next())?;
                }
            }
            Stmt::GetDiagnostics { stacked, items } => {
                for (var, item) in items {
                    let v = if *stacked {
                        let e = self.current_error.last().cloned().ok_or_else(|| {
                            SqlError::new(
                                code::STACKED_DIAGNOSTICS_ACCESSED_WITHOUT_ACTIVE_HANDLER,
                                "GET STACKED DIAGNOSTICS cannot be used outside an exception handler",
                            )
                        })?;
                        match item.as_str() {
                            "returned_sqlstate" => Value::Text(e.code.to_owned()),
                            "message_text" => Value::Text(e.message.clone()),
                            "pg_exception_detail" => Value::Text(e.detail().unwrap_or_default().to_owned()),
                            "pg_exception_hint" => Value::Text(e.hint().unwrap_or_default().to_owned()),
                            _ => Value::Text(String::new()),
                        }
                    } else {
                        match item.as_str() {
                            "row_count" => Value::Int8(self.row_count),
                            _ => Value::Null,
                        }
                    };
                    self.assign(var, v)?;
                }
            }
            Stmt::Block(b) => {
                return match self.run_block(b)? {
                    Flow::Exit(Some(l)) if Some(&l) == b.label.as_ref() => Ok(Flow::Normal),
                    other => Ok(other),
                };
            }
        }
        Ok(Flow::Normal)
    }

    fn loop_targets(&mut self, targets: &[String], cols: &[(String, Type)], row: Row) -> SqlResult<()> {
        // An undeclared single loop variable is an implicit record.
        if let [t] = targets
            && self.var(t).is_none()
        {
            self.vars.push(Var::row(t, cols.to_vec(), None));
        }
        self.assign_row(targets, cols, Some(row))
    }

    fn run_query(&mut self, q: &Query) -> SqlResult<crate::session::QueryResult> {
        match q {
            Query::Static(sql) => self.run_sql(sql),
            Query::Dynamic { sql, using } => {
                let text = display(&self.eval(sql)?);
                self.run_dynamic(&text, using)
            }
        }
    }

    /// `EXECUTE text USING args`: the text runs as is, `$n` are the USING values.
    fn run_dynamic(&mut self, text: &str, using: &[String]) -> SqlResult<crate::session::QueryResult> {
        let mut types = Vec::new();
        let mut values = Vec::new();
        for u in using {
            let (v, t) = self.eval_typed(u)?;
            types.push(t);
            values.push(v);
        }
        let mut stmts = pgmem_parse::parse(text)?;
        if stmts.len() != 1 {
            return Err(SqlError::syntax("cannot insert multiple commands into a prepared statement"));
        }
        let r = self.exec.statement(&stmts.remove(0), &types, &values)?;
        self.row_count = row_count(&r);
        Ok(r)
    }

    fn raise(&mut self, r: &Raise) -> SqlResult<()> {
        if r.reraise {
            return Err(self.current_error.last().cloned().ok_or_else(|| {
                SqlError::new(code::RAISE_EXCEPTION, "RAISE without parameters cannot be used outside an exception handler")
            })?);
        }
        let mut message = match &r.message {
            Some(fmt) => {
                let mut args = Vec::new();
                for a in &r.args {
                    let v = self.eval(a)?;
                    args.push(if v.is_null() { "<NULL>".to_owned() } else { display(&v) });
                }
                format_raise(fmt, &args)
            }
            None => r.condition.clone().unwrap_or_default().trim_start_matches("sqlstate:").to_owned(),
        };
        let mut errcode: &'static str = match &r.condition {
            Some(c) => condition_code(c)
                .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("unrecognized exception condition \"{c}\"")))?,
            None => code::RAISE_EXCEPTION,
        };
        let (mut detail, mut hint) = (None, None);
        for (opt, e) in &r.options {
            let v = display(&self.eval(e)?);
            match opt.as_str() {
                "message" => message = v,
                "detail" => detail = Some(v),
                "hint" => hint = Some(v),
                "errcode" => {
                    errcode = if v.len() == 5 && v.chars().all(|c| c.is_ascii_alphanumeric()) {
                        intern_code(&v.to_uppercase())
                    } else {
                        condition_code(&v.to_lowercase())
                            .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("unrecognized exception condition \"{v}\"")))?
                    };
                }
                "column" | "constraint" | "datatype" | "table" | "schema" => {}
                other => return Err(SqlError::syntax(format!("unrecognized RAISE statement option \"{other}\""))),
            }
        }
        if r.level == "EXCEPTION" {
            let mut e = SqlError::new(errcode, message);
            if let Some(d) = detail {
                e = e.with_detail(d);
            }
            if let Some(h) = hint {
                e = e.with_hint(h);
            }
            return Err(e);
        }
        let (severity, code): (&'static str, &'static str) = match r.level.as_str() {
            "WARNING" => ("WARNING", "01000"),
            "NOTICE" => ("NOTICE", "00000"),
            "INFO" => ("INFO", "00000"),
            // LOG and DEBUG are below client_min_messages (notice) by default.
            _ => return Ok(()),
        };
        self.exec.state().notices.borrow_mut().push(Notice { severity, code, message });
        Ok(())
    }
}

/// `%` placeholders replaced by the arguments; `%%` is a literal `%`.
fn format_raise(fmt: &str, args: &[String]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if chars.peek() == Some(&'%') {
                chars.next();
                out.push('%');
            } else if let Some(a) = args.next() {
                out.push_str(a);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A value's text output (`::text`).
pub fn display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => (if *b { "t" } else { "f" }).to_owned(),
        other => pgmem_types::text::encode(other, &other.natural_type()).unwrap_or_default(),
    }
}

fn row_count(r: &crate::session::QueryResult) -> i64 {
    r.tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

fn parse_one(sql: &str) -> SqlResult<pgmem_ir::Statement> {
    let mut s = pgmem_parse::parse(sql)?;
    if s.len() != 1 {
        return Err(SqlError::syntax(format!("syntax error at or near \"{sql}\"")));
    }
    Ok(s.remove(0))
}

fn ir_name(s: &str) -> pgmem_ir::ObjectName {
    pgmem_ir::ObjectName(s.split('.').map(|p| p.trim_matches('"').to_owned()).collect())
}

/// `WHEN a, b, c` of a simple CASE: the comma-separated expressions.
fn split_top_level(e: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut quote = false;
    for c in e.chars() {
        match c {
            '\'' => quote = !quote,
            '(' if !quote => depth += 1,
            ')' if !quote => depth -= 1,
            ',' if !quote && depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push(cur);
    out
}
