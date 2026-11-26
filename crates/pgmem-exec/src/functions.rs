//! User-defined functions and procedures: CREATE / DROP, overload resolution, and running
//! their bodies (SQL here, PL/pgSQL in [`crate::plpgsql`]).
//!
//! A call is planned as an ordinary [`crate::plan::Expr::Call`] of [`call_scalar`] (or a
//! function scan of [`call_set`]) whose first argument is the function's OID; the body is
//! parsed and bound when the call runs. Volatile functions may modify data wherever they
//! are called (from a query they write through [`pgmem_storage::Txn::with_side`]); stable and
//! immutable ones run read-only, as in Postgres.

use crate::bind::{Binder, Ctx, Level, Scope};
use crate::cast::{CoercionContext, can_coerce};
use crate::eval::{EvalCtx, StatementState};
use crate::plan::Row;
use crate::session::{QueryResult, SessionEnv};
use pgmem_catalog::{ArgMode, Catalog, FuncArg, FuncReturn, FunctionDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, Type, Value, code};
use std::sync::Arc;

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

/// Postgres' limit on nested function calls (`max_stack_depth`, roughly).
const MAX_DEPTH: usize = 200;

thread_local! {
    static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts a nested call; errors past the stack depth limit.
pub(crate) fn enter_call() -> SqlResult<CallGuard> {
    let d = DEPTH.with(|d| {
        let n = d.get() + 1;
        d.set(n);
        n
    });
    let guard = CallGuard;
    if d > MAX_DEPTH {
        return Err(SqlError::new(code::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
            .with_hint("Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate."));
    }
    Ok(guard)
}

pub(crate) struct CallGuard;

impl Drop for CallGuard {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

// ---------------------------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------------------------

fn arg_mode(m: ir::FunctionArgMode) -> ArgMode {
    match m {
        ir::FunctionArgMode::In => ArgMode::In,
        ir::FunctionArgMode::Out => ArgMode::Out,
        ir::FunctionArgMode::InOut => ArgMode::InOut,
        ir::FunctionArgMode::Variadic => ArgMode::Variadic,
    }
}

fn resolve_type(catalog: &Catalog, t: &ir::TypeName) -> SqlResult<Type> {
    if t.array_dims == 0 && t.schema.is_none() {
        match t.name.as_str() {
            "trigger" => return Ok(Type::Unknown),
            "record" => return Ok(Type::Record),
            "void" => return Ok(Type::Void),
            _ => {}
        }
    }
    Ok(crate::bind::resolve_type_name(catalog, t)?.0)
}

/// `function_name(argtypes)` as Postgres names a function in messages.
pub fn signature(name: &str, types: &[Type]) -> String {
    format!("{name}({})", types.iter().map(Type::name).collect::<Vec<_>>().join(", "))
}

fn kind_word(procedure: bool) -> &'static str {
    if procedure { "procedure" } else { "function" }
}

/// CREATE [OR REPLACE] FUNCTION / PROCEDURE.
pub(crate) fn create_function(cf: &ir::CreateFunction, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let schema = catalog.creation_schema(&cf.name)?;
    let name = cf.name.base().to_owned();
    let language = cf.language.clone().ok_or_else(|| SqlError::new(code::INVALID_FUNCTION_DEFINITION, "no language specified"))?;
    if !matches!(language.as_str(), "sql" | "plpgsql") {
        return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("language \"{language}\" does not exist")));
    }
    let body = cf.body.clone().ok_or_else(|| SqlError::new(code::INVALID_FUNCTION_DEFINITION, "no function body specified"))?;
    let mut args = Vec::with_capacity(cf.args.len());
    for a in &cf.args {
        args.push(FuncArg { name: a.name.clone(), ty: resolve_type(&catalog, &a.ty)?, mode: arg_mode(a.mode), default: a.default.clone() });
    }
    let outs: Vec<(String, Type)> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| matches!(a.mode, ArgMode::Out | ArgMode::InOut))
        .map(|(i, a)| (a.name.clone().unwrap_or_else(|| format!("column{}", i + 1)), a.ty.clone()))
        .collect();
    let ret = match (&cf.returns, cf.procedure) {
        (_, true) => FuncReturn::None,
        (Some(ir::FunctionReturns::Type(t)), false) => FuncReturn::Scalar(resolve_type(&catalog, t)?),
        (Some(ir::FunctionReturns::SetOf(t)), false) if t.name == "record" && !outs.is_empty() => FuncReturn::Table(outs.clone()),
        (Some(ir::FunctionReturns::SetOf(t)), false) => FuncReturn::SetOf(resolve_type(&catalog, t)?),
        (Some(ir::FunctionReturns::Table(cols)), false) => {
            FuncReturn::Table(cols.iter().map(|(n, t)| Ok((n.clone(), resolve_type(&catalog, t)?))).collect::<SqlResult<Vec<_>>>()?)
        }
        // OUT parameters determine the result: one is a scalar, several a record.
        (None, false) => match outs.as_slice() {
            [] => return Err(SqlError::new(code::INVALID_FUNCTION_DEFINITION, "function result type must be specified")),
            [(_, t)] => FuncReturn::Scalar(t.clone()),
            _ => FuncReturn::Scalar(Type::Record),
        },
    };
    let mut def = FunctionDef {
        oid: 0,
        schema: schema.clone(),
        name: name.clone(),
        procedure: cf.procedure,
        args,
        ret,
        language,
        body,
        volatility: cf.volatility.unwrap_or('v'),
        strict: cf.strict,
        security_definer: cf.security_definer,
        leakproof: cf.leakproof,
        parallel: cf.parallel.unwrap_or('u'),
        cost: cf.cost.unwrap_or(100.0),
        rows: 0.0,
        config: Vec::new(),
        extensions: Vec::new(),
    };
    if cf.procedure
        && (cf.volatility.is_some() || cf.strict || cf.leakproof || cf.parallel.is_some() || cf.cost.is_some() || cf.rows.is_some())
    {
        return Err(procedure_attribute());
    }
    if cf.rows.is_some() && !def.returns_set() {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "ROWS is not applicable when function does not return a set"));
    }
    def.rows = cf.rows.unwrap_or(if def.returns_set() { 1000.0 } else { 0.0 });
    for c in &cf.config {
        apply_config(&mut def.config, c, txn.catalog(), env)?;
    }
    let inputs = def.input_types();
    let existing = catalog.functions().find(|f| f.schema == schema && f.name == name && f.input_types() == inputs).cloned();
    let new = existing.is_none();
    match existing {
        Some(old) if !cf.or_replace => {
            return Err(SqlError::new(
                code::DUPLICATE_FUNCTION,
                format!("{} \"{name}\" already exists with same argument types", kind_word(old.procedure)),
            ));
        }
        Some(old) => {
            crate::ownership::check_replace_function(&catalog, env, &old)?;
            if old.procedure != def.procedure {
                return Err(SqlError::new(code::WRONG_OBJECT_TYPE, "cannot change routine kind")
                    .with_detail(format!("\"{name}\" is a {}.", kind_word(old.procedure))));
            }
            if old.ret != def.ret {
                return Err(SqlError::new(code::INVALID_FUNCTION_DEFINITION, "cannot change return type of existing function")
                    .with_hint(format!("Use DROP FUNCTION {} first.", signature(&name, &inputs))));
            }
            let renamed = old.args.iter().zip(&def.args).find(|(o, n)| o.name.is_some() && o.name != n.name);
            if let Some((o, _)) = renamed {
                return Err(SqlError::new(
                    code::INVALID_FUNCTION_DEFINITION,
                    format!("cannot change name of input parameter \"{}\"", o.name.clone().unwrap_or_default()),
                )
                .with_hint(format!("Use DROP FUNCTION {} first.", signature(&name, &inputs))));
            }
            def.oid = old.oid;
            def.extensions.clone_from(&old.extensions);
        }
        None => def.oid = txn.catalog_mut().alloc_oid(),
    }
    // SQL bodies are checked when created (check_function_bodies): they must parse.
    // Positions inside the body are not positions in the CREATE FUNCTION text.
    if def.language == "sql" {
        pgmem_parse::parse(&def.body).map_err(SqlError::without_position)?;
    } else {
        crate::plpgsql::parse(&def.body).map_err(SqlError::without_position)?;
    }
    let oid = def.oid;
    txn.catalog_mut().put_function(def);
    // A new function is owned by the role that creates it.
    let role = crate::roles::current_role(env);
    if new && role != env.user {
        txn.catalog_mut().set_owner(oid, Some(&role));
    }
    Ok(tag(if cf.procedure { "CREATE PROCEDURE" } else { "CREATE FUNCTION" }))
}

fn procedure_attribute() -> SqlError {
    SqlError::new(code::INVALID_FUNCTION_DEFINITION, "invalid attribute in procedure definition")
}

/// Parameters whose values are lists that `proconfig` stores identifier-quoted.
fn is_list_quote(name: &str) -> bool {
    matches!(
        name,
        "search_path" | "temp_tablespaces" | "session_preload_libraries" | "shared_preload_libraries" | "local_preload_libraries"
    )
}

/// A function `SET` parameter's name as `proconfig` spells it (`DateStyle`, `x.y`), erroring
/// for a parameter that does not exist.
fn config_name(name: &str) -> SqlResult<String> {
    let lower = name.to_ascii_lowercase();
    match crate::settings::lookup(&lower) {
        Some(def) => Ok(def.name.to_owned()),
        // `role` is not listed in pg_settings, but a function may set it.
        None if crate::settings::is_custom(&lower) || lower == "role" => Ok(lower),
        None => Err(crate::settings::unrecognized(&lower)),
    }
}

/// Applies one `SET` / `RESET` option to a `proconfig` array (`GUCArrayAdd` / `GUCArrayDelete`).
fn apply_config(config: &mut Vec<String>, c: &ir::FunctionConfig, catalog: &Catalog, env: &SessionEnv) -> SqlResult<()> {
    let (name, value) = match c {
        ir::FunctionConfig::ResetAll => {
            config.clear();
            return Ok(());
        }
        ir::FunctionConfig::Reset(n) => {
            let n = config_name(n)?;
            config.retain(|e| e.split_once('=').map(|(k, _)| k) != Some(n.as_str()));
            return Ok(());
        }
        ir::FunctionConfig::FromCurrent(n) => {
            let n = config_name(n)?;
            let lower = n.to_ascii_lowercase();
            let def = crate::settings::lookup(&lower);
            let stored = env.setting(&lower).or_else(|| def.and_then(|d| d.boot).map(str::to_owned)).unwrap_or_default();
            (n, crate::settings::show(def, &stored))
        }
        ir::FunctionConfig::Set(n, values) => {
            let n = config_name(n)?;
            let lower = n.to_ascii_lowercase();
            if lower == "role" {
                let target = values.join(", ");
                if !target.eq_ignore_ascii_case("none") && !crate::roles::role_exists(catalog, env, &target) {
                    return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("role \"{target}\" does not exist")));
                }
            } else {
                crate::session::validate_setting(&lower, &values.join(", "), env.setting(&lower).as_deref())?;
            }
            let value = if is_list_quote(&lower) {
                values.iter().map(|v| crate::deparse::pg::quote_ident(v)).collect::<Vec<_>>().join(", ")
            } else {
                values.join(", ")
            };
            (n, value)
        }
    };
    let entry = format!("{name}={value}");
    match config.iter_mut().find(|e| e.split_once('=').map(|(k, _)| k) == Some(name.as_str())) {
        Some(slot) => *slot = entry,
        None => config.push(entry),
    }
    Ok(())
}

/// The values of a stored `proconfig` entry's list, split as `SplitGUCList` does.
pub(crate) fn split_guc_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut cur = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        cur.push('"');
                    } else {
                        break;
                    }
                } else {
                    cur.push(c);
                }
            }
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' || c.is_whitespace() {
                    break;
                }
                cur.push(c);
                chars.next();
            }
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
        }
        out.push(cur);
        if chars.peek() == Some(&',') {
            chars.next();
        } else {
            break;
        }
    }
    out
}

/// ALTER {FUNCTION | PROCEDURE | ROUTINE} name [(types)] action ....
pub(crate) fn alter_function(
    name: &ir::ObjectName,
    types: Option<&[ir::TypeName]>,
    kind: ir::RoutineKind,
    actions: &[ir::AlterFunctionAction],
    txn: &mut Txn,
    env: &SessionEnv,
) -> SqlResult<QueryResult> {
    use ir::AlterFunctionAction as A;
    let catalog = txn.catalog().clone();
    let word = match kind {
        ir::RoutineKind::Procedure => "procedure",
        ir::RoutineKind::Routine => "routine",
        ir::RoutineKind::Function => "function",
    };
    let Some(f) = find_for_drop(&catalog, name, types, kind)? else {
        let shown = match types {
            Some(ts) => signature(&name.to_string(), &ts.iter().map(|t| resolve_type(&catalog, t)).collect::<SqlResult<Vec<_>>>()?),
            None => name.to_string(),
        };
        return Err(SqlError::undefined_function(format!("{word} {shown} does not exist")));
    };
    let sig = signature(&f.name, &f.input_types());
    if kind == ir::RoutineKind::Function && f.procedure {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{sig} is not a function")));
    }
    if kind == ir::RoutineKind::Procedure && !f.procedure {
        return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{sig} is not a procedure")));
    }
    let mut def = (*f).clone();
    let mut owner = None;
    for a in actions {
        match a {
            A::RenameTo(new) | A::SetSchema(new) => {
                let (schema, base) =
                    if matches!(a, A::RenameTo(_)) { (def.schema.clone(), new.clone()) } else { (new.clone(), def.name.clone()) };
                if matches!(a, A::SetSchema(_)) && catalog.schema(&schema).is_none() {
                    return Err(pgmem_catalog::undefined_schema(&schema));
                }
                let clash = catalog
                    .functions()
                    .any(|g| g.oid != def.oid && g.schema == schema && g.name == base && g.input_types() == def.input_types());
                if clash {
                    return Err(SqlError::new(
                        code::DUPLICATE_FUNCTION,
                        format!("function {} already exists in schema \"{schema}\"", signature(&base, &def.input_types())),
                    ));
                }
                def.schema = schema;
                def.name = base;
            }
            A::Config(c) => apply_config(&mut def.config, c, &catalog, env)?,
            A::SecurityDefiner(v) => def.security_definer = *v,
            A::OwnerTo(role) => owner = Some(role),
            A::DependsOnExtension { extension, remove } => {
                if extension != "plpgsql" && catalog.extension(extension).is_none() {
                    return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("extension \"{extension}\" does not exist")));
                }
                def.extensions.retain(|e| e != extension);
                if !remove {
                    def.extensions.push(extension.clone());
                }
            }
            _ if def.procedure => return Err(procedure_attribute()),
            A::Volatility(v) => def.volatility = *v,
            A::Strict(v) => def.strict = *v,
            A::Leakproof(v) => def.leakproof = *v,
            A::Parallel(v) => def.parallel = *v,
            A::Cost(v) => def.cost = *v,
            A::Rows(v) => {
                if !def.returns_set() {
                    return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, "ROWS is not applicable when function does not return a set"));
                }
                def.rows = *v;
            }
        }
    }
    let oid = def.oid;
    txn.catalog_mut().put_function(def);
    if let Some(role) = owner {
        crate::roles::set_owner(txn, env, oid, role)?;
    }
    Ok(tag(match kind {
        ir::RoutineKind::Procedure => "ALTER PROCEDURE",
        ir::RoutineKind::Routine => "ALTER ROUTINE",
        ir::RoutineKind::Function => "ALTER FUNCTION",
    }))
}

/// The routine `name [(types)]` names (COMMENT ON FUNCTION, ...): exactly one.
pub fn find_routine(catalog: &Catalog, name: &ir::ObjectName, types: Option<&[ir::TypeName]>) -> SqlResult<Arc<FunctionDef>> {
    match find_for_drop(catalog, name, types, ir::RoutineKind::Routine)? {
        Some(f) => Ok(f),
        None => {
            let shown = match types {
                Some(ts) => signature(&name.to_string(), &ts.iter().map(|t| resolve_type(catalog, t)).collect::<SqlResult<Vec<_>>>()?),
                None => name.to_string(),
            };
            Err(SqlError::undefined_function(format!("function {shown} does not exist")))
        }
    }
}

/// The functions DROP FUNCTION `name(types)` names: exactly one, or an error.
fn find_for_drop(
    catalog: &Catalog,
    name: &ir::ObjectName,
    types: Option<&[ir::TypeName]>,
    kind: ir::RoutineKind,
) -> SqlResult<Option<Arc<FunctionDef>>> {
    let cands = catalog.find_functions(name);
    let word = match kind {
        ir::RoutineKind::Procedure => "procedure",
        ir::RoutineKind::Routine => "routine",
        ir::RoutineKind::Function => "function",
    };
    let base = name.base();
    let Some(types) = types else {
        return match cands.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(Arc::clone(one))),
            _ => Err(SqlError::new(code::AMBIGUOUS_FUNCTION, format!("{word} name \"{base}\" is not unique"))
                .with_hint(format!("Specify the argument list to select the {word} unambiguously."))),
        };
    };
    let want = types.iter().map(|t| resolve_type(catalog, t)).collect::<SqlResult<Vec<_>>>()?;
    Ok(cands.into_iter().find(|f| f.input_types() == want))
}

/// What depends on a function: column defaults that call it and triggers that run it.
#[derive(Default)]
pub(crate) struct FunctionDependents {
    /// DETAIL lines (`default value for column c of table t depends on function f()`).
    lines: Vec<String>,
    defaults: Vec<(pgmem_catalog::QualName, usize)>,
    triggers: Vec<Arc<pgmem_catalog::TriggerDef>>,
}

impl FunctionDependents {
    pub(crate) fn of(catalog: &Catalog, f: &FunctionDef) -> FunctionDependents {
        let defaults = crate::deps::defaults_using_function(catalog, f);
        let triggers: Vec<_> = catalog.triggers().filter(|t| t.function == f.oid).cloned().collect();
        let sig = signature(&f.name, &f.input_types());
        let mut lines: Vec<String> = defaults
            .iter()
            .map(|(t, slot)| {
                let column = catalog.table_q(t).map(|d| d.columns[*slot].name.clone()).unwrap_or_default();
                format!("default value for column {column} of table {} depends on function {sig}", t.name)
            })
            .collect();
        for t in &triggers {
            let table = catalog.relation_by_oid(t.table).map(|r| r.name().to_owned()).unwrap_or_default();
            lines.push(format!("trigger {} on table {table} depends on function {sig}", t.name));
        }
        FunctionDependents { lines, defaults, triggers }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Appends another function's dependents.
    pub(crate) fn extend(&mut self, other: FunctionDependents) {
        self.lines.extend(other.lines);
        self.defaults.extend(other.defaults);
        self.triggers.extend(other.triggers);
    }

    /// 2BP01 `cannot drop <what> because other objects depend on it`, the dependents as DETAIL.
    pub(crate) fn error(&self, what: &str) -> SqlError {
        SqlError::new(code::DEPENDENT_OBJECTS_STILL_EXIST, format!("cannot drop {what} because other objects depend on it"))
            .with_detail(self.lines.join("\n"))
            .with_hint("Use DROP ... CASCADE to drop the dependent objects too.")
    }

    /// DROP ... CASCADE: the defaults are removed and the triggers dropped, each reported.
    pub(crate) fn drop(self, txn: &mut Txn, notices: &mut Vec<crate::session::Notice>) {
        let notice = |message: String| crate::session::Notice { severity: "NOTICE", code: "00000", message };
        for (t, slot) in self.defaults {
            if let Some(def) = txn.catalog().table_q(&t).cloned() {
                let mut def = (*def).clone();
                notices.push(notice(format!("drop cascades to default value for column {} of table {}", def.columns[slot].name, t.name)));
                def.columns[slot].default = None;
                txn.catalog_mut().replace_table(&t, def);
            }
        }
        for t in self.triggers {
            let table = txn.catalog().relation_by_oid(t.table).map(|r| r.name().to_owned()).unwrap_or_default();
            notices.push(notice(format!("drop cascades to trigger {} on table {table}", t.name)));
            let cat = txn.catalog_mut();
            cat.drop_comments(t.oid);
            cat.drop_trigger(t.oid);
        }
    }
}

/// DROP {FUNCTION | PROCEDURE | ROUTINE} [IF EXISTS] name [(types)], ... [CASCADE].
pub(crate) fn drop_function(
    items: &[(ir::ObjectName, Option<Vec<ir::TypeName>>)],
    if_exists: bool,
    cascade: bool,
    kind: ir::RoutineKind,
    txn: &mut Txn,
) -> SqlResult<QueryResult> {
    let word = match kind {
        ir::RoutineKind::Procedure => "procedure",
        ir::RoutineKind::Routine => "routine",
        ir::RoutineKind::Function => "function",
    };
    let mut notices = Vec::new();
    for (name, types) in items {
        let catalog = txn.catalog().clone();
        let Some(f) = find_for_drop(&catalog, name, types.as_deref(), kind)? else {
            let shown = match types {
                Some(ts) => {
                    let ts = ts.iter().map(|t| resolve_type(&catalog, t)).collect::<SqlResult<Vec<_>>>()?;
                    signature(&name.to_string(), &ts)
                }
                None => name.to_string(),
            };
            if if_exists {
                notices.push(crate::session::Notice {
                    severity: "NOTICE",
                    code: "00000",
                    message: format!("{word} {shown} does not exist, skipping"),
                });
                continue;
            }
            return Err(SqlError::undefined_function(format!("{word} {shown} does not exist")));
        };
        if kind == ir::RoutineKind::Function && f.procedure {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{} is not a function", signature(&f.name, &f.input_types()))));
        }
        if kind == ir::RoutineKind::Procedure && !f.procedure {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{} is not a procedure", signature(&f.name, &f.input_types()))));
        }
        let deps = FunctionDependents::of(&catalog, &f);
        if !deps.is_empty() && !cascade {
            let sig = signature(&f.name, &f.input_types());
            return Err(deps.error(&format!("function {sig}")));
        }
        deps.drop(txn, &mut notices);
        let cat = txn.catalog_mut();
        cat.drop_comments(f.oid);
        cat.drop_function(f.oid);
    }
    let what = match kind {
        ir::RoutineKind::Procedure => "DROP PROCEDURE",
        ir::RoutineKind::Routine => "DROP ROUTINE",
        ir::RoutineKind::Function => "DROP FUNCTION",
    };
    Ok(QueryResult { tag: what.into(), notices, ..Default::default() })
}

// ---------------------------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------------------------

/// A resolved call: the function and the types its arguments are coerced to (defaults for
/// omitted trailing arguments are appended by the binder).
pub struct UserCall {
    pub def: Arc<FunctionDef>,
    pub targets: Vec<Type>,
}

/// Picks the overload of `cands` for argument types `types` (Postgres' func_select_candidate,
/// simplified: exact matches first, then string-category preference for unknown literals).
pub fn resolve(name: &str, cands: &[Arc<FunctionDef>], types: &[Type], procedure: bool) -> SqlResult<Option<UserCall>> {
    let mut best: Vec<(usize, usize, &Arc<FunctionDef>)> = Vec::new();
    for f in cands.iter().filter(|f| f.procedure == procedure) {
        let inputs: Vec<&FuncArg> = f.args.iter().filter(|a| a.mode.is_input()).collect();
        // Defaults are trailing: the rest must be passed.
        let required = inputs.iter().filter(|a| a.default.is_none()).count();
        if types.len() < required || types.len() > inputs.len() {
            continue;
        }
        let mut exact = 0;
        let mut preferred = 0;
        let mut ok = true;
        for (t, a) in types.iter().zip(&inputs) {
            if *t == a.ty {
                exact += 1;
            } else if *t == Type::Unknown {
                if a.ty == Type::Text {
                    preferred += 1;
                }
            } else if !can_coerce(t, &a.ty, CoercionContext::Implicit) {
                ok = false;
                break;
            }
        }
        if ok {
            best.push((exact, preferred, f));
        }
    }
    best.sort_by_key(|b| std::cmp::Reverse((b.0, b.1)));
    match best.as_slice() {
        [] => Ok(None),
        [(e, p, _), (e2, p2, _), ..] if (e, p) == (e2, p2) => {
            Err(SqlError::new(code::AMBIGUOUS_FUNCTION, format!("function {} is not unique", signature(name, types)))
                .with_hint("Could not choose a best candidate function. You might need to add explicit type casts."))
        }
        [(_, _, f), ..] => {
            let targets = f.args.iter().filter(|a| a.mode.is_input()).map(|a| a.ty.clone()).collect();
            Ok(Some(UserCall { def: Arc::clone(f), targets }))
        }
    }
}

/// The columns a set-returning (or table) function yields.
pub fn result_columns(def: &FunctionDef) -> Vec<(String, Type)> {
    match &def.ret {
        FuncReturn::Table(cols) => cols.clone(),
        FuncReturn::SetOf(t) | FuncReturn::Scalar(t) => vec![(def.name.clone(), t.clone())],
        FuncReturn::None => Vec::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------

/// Where a function body runs: read-only (a stable or immutable function), with the
/// transaction writable, or reading a query's transaction and writing through
/// [`pgmem_storage::Txn::with_side`] (a volatile function called from a query).
pub enum Exec<'b, 'a> {
    Read(&'b EvalCtx<'a>),
    Write {
        txn: &'b mut Txn,
        env: &'b SessionEnv,
        state: &'b StatementState,
    },
    /// Reads go to the query's transaction until the first write makes the writable copy, so
    /// a volatile function that writes nothing (a BEFORE trigger setting `NEW` columns) costs
    /// no copy.
    Volatile(&'b EvalCtx<'a>),
}

impl Exec<'_, '_> {
    pub fn txn(&self) -> &Txn {
        match self {
            Exec::Read(c) | Exec::Volatile(c) => c.txn,
            Exec::Write { txn, .. } => txn,
        }
    }

    pub fn env(&self) -> &SessionEnv {
        match self {
            Exec::Read(c) | Exec::Volatile(c) => c.env,
            Exec::Write { env, .. } => env,
        }
    }

    pub fn state(&self) -> &StatementState {
        match self {
            Exec::Read(c) | Exec::Volatile(c) => c.state,
            Exec::Write { state, .. } => state,
        }
    }

    /// Runs `f` (the body of `def`) writable if `def` is volatile, read-only otherwise.
    pub(crate) fn for_function<R>(&mut self, def: &FunctionDef, f: impl FnOnce(&mut Exec) -> SqlResult<R>) -> SqlResult<R> {
        let volatile = def.volatility == 'v';
        match self {
            Exec::Read(c) if volatile => f(&mut Exec::Volatile(c)),
            Exec::Volatile(c) if !volatile => f(&mut Exec::Read(c)),
            Exec::Write { txn, env, state } if !volatile => {
                let ctx = EvalCtx::new(txn, &[], env, state);
                f(&mut Exec::Read(&ctx))
            }
            _ => f(self),
        }
    }

    /// Runs one statement with `params` ($1, $2, ... of `types`). It caches its own CTEs and
    /// subqueries, and collects its own partition row movement: the calling statement's are
    /// set aside (CTE and subquery ids come from another binder).
    pub fn statement(&mut self, stmt: &ir::Statement, types: &[Type], params: &[Value]) -> SqlResult<QueryResult> {
        let state = self.state();
        let saved = (state.ctes.take(), state.subqueries.take(), state.moved_rows.take());
        let result = self.statement_inner(stmt, types, params);
        let state = self.state();
        *state.ctes.borrow_mut() = saved.0;
        *state.subqueries.borrow_mut() = saved.1;
        *state.moved_rows.borrow_mut() = saved.2;
        result
    }

    fn statement_inner(&mut self, stmt: &ir::Statement, types: &[Type], params: &[Value]) -> SqlResult<QueryResult> {
        let plain_query = matches!(stmt, ir::Statement::Query(_)) && crate::statement::leading_with(stmt).is_none();
        match self {
            Exec::Read(c) if plain_query => read_query(stmt, types, params, c),
            Exec::Read(_) => Err(match crate::session::command_name(stmt) {
                Some(what) => SqlError::feature_not_supported(format!("{what} is not allowed in a non-volatile function")),
                None => SqlError::feature_not_supported("pgmem runs this statement only in volatile functions"),
            }),
            Exec::Volatile(c) if plain_query && !c.txn.has_side() => read_query(stmt, types, params, c),
            Exec::Volatile(c) => {
                let (env, state) = (c.env, c.state);
                c.txn.with_side(|txn| Exec::Write { txn, env, state }.statement_inner(stmt, types, params))
            }
            Exec::Write { txn, env, state } => {
                if matches!(stmt, ir::Statement::Transaction(_)) {
                    return Err(SqlError::new(code::INVALID_TRANSACTION_TERMINATION, "invalid transaction termination"));
                }
                if env.setting("transaction_read_only").is_some_and(|v| v == "on")
                    && let Some(what) =
                        crate::session::read_only_violation(stmt, txn.catalog(), pgmem_types::context::search_path().temp_schema.as_deref())
                {
                    return Err(SqlError::new(
                        code::READ_ONLY_SQL_TRANSACTION,
                        format!("cannot execute {what} in a read-only transaction"),
                    ));
                }
                let catalog = txn.catalog().clone();
                // Statements in a function body are checked for the current user: the
                // caller, or the owner of a SECURITY DEFINER function.
                crate::roles::check_statement(stmt, &catalog, env, types)?;
                txn.begin_statement();
                let binder = Binder::new(&catalog, types);
                let top = Level::new(Scope::default());
                crate::statement::exec_statement(stmt, &binder, &Ctx::new(&top), txn, params, env, state)
            }
        }
    }
}

/// Runs a plain query of a function body on the calling statement's transaction.
fn read_query(stmt: &ir::Statement, types: &[Type], params: &[Value], c: &EvalCtx) -> SqlResult<QueryResult> {
    let ir::Statement::Query(q) = stmt else { return Err(SqlError::internal("not a query")) };
    let catalog = c.txn.catalog().clone();
    crate::roles::check_statement(stmt, &catalog, c.env, types)?;
    let binder = Binder::new(&catalog, types);
    let top = Level::new(Scope::default());
    let bound = binder.bind_query(q, &Ctx::new(&top))?;
    let ectx = EvalCtx::new(c.txn, params, c.env, c.state);
    let rows = crate::run::run(&bound.rel, &ectx)?;
    Ok(QueryResult { tag: format!("SELECT {}", rows.len()), columns: bound.columns, returns_rows: true, rows, ..Default::default() })
}

/// The names of the input arguments (`$n` for unnamed ones), with their types.
fn input_names(def: &FunctionDef) -> Vec<(String, Type)> {
    def.args
        .iter()
        .filter(|a| a.mode.is_input())
        .enumerate()
        .map(|(i, a)| (a.name.clone().unwrap_or_else(|| format!("${}", i + 1)), a.ty.clone()))
        .collect()
}

/// Runs a SQL function body; the rows of its last statement.
fn run_sql(def: &FunctionDef, args: &[Value], exec: &mut Exec) -> SqlResult<Vec<Row>> {
    let names = input_names(def);
    let text = crate::plpgsql::substitute(&def.body, &names.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>());
    let stmts = pgmem_parse::parse(&text)?;
    let types: Vec<Type> = names.iter().map(|(_, t)| t.clone()).collect();
    let mut last = Vec::new();
    for s in &stmts {
        if matches!(s, ir::Statement::Empty) {
            continue;
        }
        last = exec.statement(s, &types, args)?.rows;
    }
    Ok(last)
}

/// Runs function `def` on `args`: its result rows (one row for a scalar function).
pub fn run_function(def: &FunctionDef, args: &[Value], exec: &mut Exec) -> SqlResult<Vec<Row>> {
    in_function_env(def, exec, |exec| run_function_in(def, args, exec))
}

/// Runs `f` (the body of `def`) in the function's environment (`fmgr_security_definer`):
/// a SECURITY DEFINER function runs as its owner, and its `SET` options apply to the call
/// (`search_path` to the names its statements resolve). All of it ends with the call,
/// whether it returns or fails.
pub fn in_function_env<R>(def: &FunctionDef, exec: &mut Exec, f: impl FnOnce(&mut Exec) -> SqlResult<R>) -> SqlResult<R> {
    if def.config.is_empty() && !def.security_definer {
        return f(exec);
    }
    let mut env = exec.env().clone();
    let mut path = pgmem_types::context::search_path();
    if def.security_definer {
        let owner = crate::roles::owner_of(exec.txn().catalog(), &env, def.oid);
        path.user.clone_from(&owner);
        env.security_definer = Some(owner);
    }
    for entry in &def.config {
        let (name, value) = entry.split_once('=').unwrap_or((entry, ""));
        let lower = name.to_ascii_lowercase();
        if lower == "role" && env.security_definer.is_some() {
            return Err(SqlError::new(code::INSUFFICIENT_PRIVILEGE, "cannot set parameter \"role\" within security-definer function"));
        }
        if lower == "search_path" {
            path.entries = pgmem_types::context::SearchPath::parse(value);
        }
        // Stored in the form the session keeps it (units resolved), as `SET` would.
        let stored = crate::session::validate_setting(&lower, value, env.setting(&lower).as_deref()).unwrap_or_else(|_| value.to_owned());
        env.settings.insert(lower, stored);
    }
    let _path = pgmem_types::context::enter(pgmem_types::context::Context { search_path: path, ..pgmem_types::context::current() });
    match exec {
        Exec::Read(c) => {
            let ctx = EvalCtx { txn: c.txn, params: c.params, env: &env, state: c.state, outer: c.outer.clone() };
            f(&mut Exec::Read(&ctx))
        }
        Exec::Volatile(c) => {
            let ctx = EvalCtx { txn: c.txn, params: c.params, env: &env, state: c.state, outer: c.outer.clone() };
            f(&mut Exec::Volatile(&ctx))
        }
        Exec::Write { txn, state, .. } => f(&mut Exec::Write { txn, env: &env, state }),
    }
}

fn run_function_in(def: &FunctionDef, args: &[Value], exec: &mut Exec) -> SqlResult<Vec<Row>> {
    let _guard = enter_call()?;
    if def.strict && args.iter().any(Value::is_null) {
        return Ok(if def.returns_set() { Vec::new() } else { vec![vec![Value::Null]] });
    }
    // Errors inside the body point into the body, not into the calling query.
    let rows = exec
        .for_function(def, |exec| match def.language.as_str() {
            "plpgsql" => crate::plpgsql::run_function(def, args, exec),
            _ => run_sql(def, args, exec),
        })
        .map_err(SqlError::without_position)?;
    let coerce_row = |row: Row, types: &[Type]| -> SqlResult<Row> {
        row.into_iter().zip(types).map(|(v, t)| crate::cast::cast_value(v, t, -1)).collect()
    };
    Ok(match &def.ret {
        FuncReturn::Scalar(Type::Void) => vec![vec![Value::Text(String::new())]],
        FuncReturn::Scalar(Type::Record) => {
            vec![vec![rows.into_iter().next().map_or(Value::Null, |r| match r.as_slice() {
                [v @ Value::Record(..)] => v.clone(),
                _ => Value::Record(pgmem_types::usertype::RECORD_OID, r),
            })]]
        }
        FuncReturn::Scalar(t) => {
            let v = rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null);
            vec![vec![crate::cast::cast_value(v, t, -1)?]]
        }
        FuncReturn::SetOf(t) => {
            rows.into_iter().map(|r| coerce_row(r.into_iter().take(1).collect(), std::slice::from_ref(t))).collect::<SqlResult<_>>()?
        }
        FuncReturn::Table(cols) => {
            let types: Vec<Type> = cols.iter().map(|(_, t)| t.clone()).collect();
            rows.into_iter().map(|r| coerce_row(r, &types)).collect::<SqlResult<_>>()?
        }
        FuncReturn::None => Vec::new(),
    })
}

fn function_of(ctx: &EvalCtx, a: &[Value]) -> SqlResult<Arc<FunctionDef>> {
    let Some(Value::Oid(oid)) = a.first() else { return Err(SqlError::internal("user function call without its OID")) };
    ctx.txn
        .catalog()
        .function_by_oid(*oid)
        .cloned()
        .ok_or_else(|| SqlError::new(code::UNDEFINED_FUNCTION, format!("cache lookup failed for function {oid}")))
}

/// `Expr::Call` implementation of a scalar user function (argument 0 is its OID).
pub fn call_scalar(a: &[Value], ctx: &EvalCtx) -> SqlResult<Value> {
    let def = function_of(ctx, a)?;
    let mut exec = Exec::Read(ctx);
    let rows = run_function(&def, &a[1..], &mut exec)?;
    Ok(rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
}

/// Function-scan implementation of a set-returning user function (argument 0 is its OID).
pub fn call_set(a: &[Value], ctx: &EvalCtx) -> SqlResult<Vec<Row>> {
    let def = function_of(ctx, a)?;
    let mut exec = Exec::Read(ctx);
    run_function(&def, &a[1..], &mut exec)
}

/// CALL name(args).
pub(crate) fn call(
    name: &ir::ObjectName,
    args: &[ir::Expr],
    txn: &mut Txn,
    env: &SessionEnv,
    state: &StatementState,
) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let binder = Binder::new(&catalog, &[]);
    let top = Level::new(Scope::default());
    let ctx = Ctx::new(&top);
    let bound = args.iter().map(|a| binder.bind_expr(a, &ctx)).collect::<SqlResult<Vec<_>>>()?;
    let types: Vec<Type> = bound.iter().map(|(_, t)| t.clone()).collect();
    let cands = catalog.find_functions(name);
    let Some(call) = resolve(name.base(), &cands, &types, true)? else {
        return Err(SqlError::undefined_function(format!("procedure {} does not exist", signature(&name.to_string(), &types)))
            .with_hint("No procedure matches the given name and argument types. You might need to add explicit type casts."));
    };
    crate::roles::check_execute(&catalog, env, &call.def)?;
    let mut values = Vec::with_capacity(call.targets.len());
    {
        let ectx = EvalCtx::new(txn, &[], env, state);
        for ((e, t), target) in bound.into_iter().zip(&call.targets) {
            let e = crate::bind::coerce(&binder, e, &t, target, CoercionContext::Implicit)?;
            values.push(crate::eval::eval(&e, &[], &ectx)?);
        }
        // Defaults of omitted arguments.
        for a in call.def.args.iter().filter(|a| a.mode.is_input()).skip(values.len()) {
            let d = a.default.as_ref().ok_or_else(|| SqlError::internal("missing argument"))?;
            let (e, t) = binder.bind_expr(d, &ctx)?;
            let e = crate::bind::coerce(&binder, e, &t, &a.ty, CoercionContext::Assignment)?;
            values.push(crate::eval::eval(&e, &[], &ectx)?);
        }
    }
    let mut exec = Exec::Write { txn, env, state };
    run_function(&call.def, &values, &mut exec)?;
    Ok(tag("CALL"))
}

/// DO [LANGUAGE plpgsql] body.
pub(crate) fn do_block(language: &str, body: &str, txn: &mut Txn, env: &SessionEnv, state: &StatementState) -> SqlResult<QueryResult> {
    if language != "plpgsql" {
        return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("language \"{language}\" does not exist")));
    }
    let mut exec = Exec::Write { txn, env, state };
    let _guard = enter_call()?;
    crate::plpgsql::run_block(body, &mut exec).map_err(SqlError::without_position)?;
    Ok(tag("DO"))
}
