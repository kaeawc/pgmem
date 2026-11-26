//! PL/pgSQL: functions, DO blocks and trigger functions.

mod conditions;
mod exec;
mod parse;

use crate::functions::Exec;
use crate::plan::Row;
use exec::{Interp, Var};
use pgmem_catalog::{ArgMode, FuncReturn, FunctionDef};
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

/// `1..3` lexes as two numbers in SQL; PL/pgSQL's integer FOR needs `1 .. 3`.
fn spread_ranges(src: &str) -> String {
    let Ok(toks) = Tokenizer::new(&PostgreSqlDialect {}, src).tokenize() else { return src.to_owned() };
    let mut out = String::with_capacity(src.len());
    for t in toks {
        match &t {
            Token::Number(n, _) if n.contains("..") => out.push_str(&n.replace("..", " .. ")),
            Token::Number(n, _) if n.ends_with('.') && n.len() > 1 => {
                out.push_str(&n[..n.len() - 1]);
                out.push_str(" .");
            }
            Token::Number(n, _) if n.starts_with('.') && n.len() > 1 => {
                out.push_str(". ");
                out.push_str(&n[1..]);
            }
            other => out.push_str(&other.to_string()),
        }
    }
    out
}

fn parse_src(body: &str) -> SqlResult<parse::Block> {
    parse::parse_body(&spread_ranges(body))
}

/// Checks a function body parses (`plpgsql_validator`).
pub fn parse(body: &str) -> SqlResult<()> {
    parse_src(body).map(|_| ())
}

/// Replaces references to `names` (function arguments) in a SQL function body with `$n`.
pub fn substitute(body: &str, names: &[String]) -> String {
    let Ok(toks) = Tokenizer::new(&PostgreSqlDialect {}, body).tokenize() else { return body.to_owned() };
    let sig: Vec<usize> = toks.iter().enumerate().filter(|(_, t)| !matches!(t, Token::Whitespace(_))).map(|(i, _)| i).collect();
    let mut out = String::new();
    let mut pos_of = std::collections::HashMap::new();
    for (k, &i) in sig.iter().enumerate() {
        pos_of.insert(i, k);
    }
    for (i, t) in toks.iter().enumerate() {
        let replaced = match (t, pos_of.get(&i)) {
            (Token::Word(w), Some(&k)) => {
                let prev_dot = k > 0 && toks[sig[k - 1]] == Token::Period;
                let next = sig.get(k + 1).map(|&j| &toks[j]);
                let name = if w.quote_style.is_some() { w.value.clone() } else { w.value.to_lowercase() };
                if prev_dot || next == Some(&Token::LParen) || next == Some(&Token::Period) {
                    None
                } else {
                    names.iter().position(|n| *n == name).map(|p| format!("${}", p + 1))
                }
            }
            _ => None,
        };
        out.push_str(&replaced.unwrap_or_else(|| t.to_string()));
    }
    out
}

/// The variables a function body starts with: its arguments by name and as `$n`.
fn arg_vars(def: &FunctionDef, args: &[Value]) -> Vec<Var> {
    let mut vars = Vec::new();
    let mut inputs = args.iter();
    let mut n = 0;
    for a in &def.args {
        let value = if a.mode.is_input() {
            n += 1;
            inputs.next().cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        if a.mode.is_input() {
            vars.push(Var::scalar(&format!("${n}"), a.ty.clone(), value.clone()));
        }
        if let Some(name) = &a.name {
            vars.push(Var::scalar(name, a.ty.clone(), value));
        }
    }
    vars
}

/// Runs a PL/pgSQL function: its result rows (one row for a scalar result).
pub fn run_function(def: &FunctionDef, args: &[Value], exec: &mut Exec) -> SqlResult<Vec<Row>> {
    let block = parse_src(&def.body)?;
    let vars = arg_vars(def, args);
    let mut it = Interp::new(exec, vars);
    let flow = it.run_block(&block)?;
    let outs: Vec<&pgmem_catalog::FuncArg> = def.args.iter().filter(|a| matches!(a.mode, ArgMode::Out | ArgMode::InOut)).collect();
    if def.returns_set() {
        return Ok(std::mem::take(&mut it.set));
    }
    match &def.ret {
        FuncReturn::None => Ok(Vec::new()),
        _ if !outs.is_empty() && it.result.is_none() => {
            // OUT parameters are the result.
            let row: Row = outs.iter().map(|a| a.name.as_ref().and_then(|n| it.var(n)).map_or(Value::Null, |v| v.value.clone())).collect();
            Ok(vec![if row.len() == 1 { row } else { vec![Value::Record(pgmem_types::usertype::RECORD_OID, row)] }])
        }
        FuncReturn::Scalar(Type::Void) => Ok(vec![vec![Value::Text(String::new())]]),
        _ => match (flow, it.result.take()) {
            (_, Some(v)) => Ok(vec![vec![v]]),
            _ => Err(SqlError::new(code::FUNCTION_EXECUTED_NO_RETURN_STATEMENT, "control reached end of function without RETURN")),
        },
    }
}

/// Runs a DO block.
pub fn run_block(body: &str, exec: &mut Exec) -> SqlResult<()> {
    let block = parse_src(body)?;
    let mut it = Interp::new(exec, Vec::new());
    it.run_block(&block)?;
    Ok(())
}

/// What a trigger function is called with (`TriggerData`).
pub struct TriggerCall<'t> {
    pub name: &'t str,
    pub table: &'t str,
    pub schema: &'t str,
    /// `BEFORE` / `AFTER` / `INSTEAD OF`.
    pub when: &'static str,
    /// `INSERT` / `UPDATE` / `DELETE` / `TRUNCATE`.
    pub op: &'static str,
    /// `ROW` / `STATEMENT`.
    pub level: &'static str,
    pub columns: Vec<(String, Type)>,
    pub new: Option<Row>,
    pub old: Option<Row>,
    pub args: &'t [String],
}

fn trigger_vars(call: &TriggerCall) -> Vec<Var> {
    let text = |v: &str| Value::Text(v.to_owned());
    vec![
        Var::row("new", call.columns.clone(), call.new.clone()),
        Var::row("old", call.columns.clone(), call.old.clone()),
        Var::scalar("tg_name", Type::Name, text(call.name)),
        Var::scalar("tg_when", Type::Text, text(call.when)),
        Var::scalar("tg_level", Type::Text, text(call.level)),
        Var::scalar("tg_op", Type::Text, text(call.op)),
        Var::scalar("tg_table_name", Type::Name, text(call.table)),
        Var::scalar("tg_relname", Type::Name, text(call.table)),
        Var::scalar("tg_table_schema", Type::Name, text(call.schema)),
        Var::scalar("tg_nargs", Type::Int4, Value::Int4(i32::try_from(call.args.len()).unwrap_or(0))),
        Var::scalar("tg_argv", Type::array_of(Type::Text), Value::Array(call.args.iter().map(|a| text(a)).collect())),
    ]
}

/// A trigger's WHEN condition, with NEW / OLD bound.
pub fn eval_when(cond: &str, call: &TriggerCall, exec: &mut Exec) -> SqlResult<bool> {
    let mut it = Interp::new(exec, trigger_vars(call));
    it.eval_bool(cond)
}

/// Runs a trigger function: the row it returns (None for `RETURN NULL`).
pub fn run_trigger(def: &FunctionDef, call: &TriggerCall, exec: &mut Exec) -> SqlResult<Option<Row>> {
    let _guard = crate::functions::enter_call()?;
    let block = parse_src(&def.body)?;
    let mut vars = trigger_vars(call);
    vars.extend(arg_vars(def, &[]));
    exec.for_function(def, |exec| {
        let mut it = Interp::new(exec, vars);
        it.run_block(&block)?;
        match it.result_row.take() {
            Some(row) => Ok(row),
            None => {
                Err(SqlError::new(code::FUNCTION_EXECUTED_NO_RETURN_STATEMENT, "control reached end of trigger procedure without RETURN"))
            }
        }
    })
}
