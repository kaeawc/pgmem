//! User-defined casts: CREATE CAST / DROP CAST (`DefineCast` in `functioncmds.c`) and the
//! lookup the binder applies them through.

use crate::bind::resolve_type_name;
use crate::cast::CoercionContext;
use crate::session::{Notice, QueryResult};
use pgmem_catalog::{CastDef, Catalog, FuncReturn};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, Type, code};

/// `pg_cast`'s class OID (the `classoid` of comments on casts).
pub const CAST_CLASS_OID: u32 = 2605;

/// The coercion context a `castcontext` letter allows.
pub fn context_of(c: char) -> CoercionContext {
    match c {
        'i' => CoercionContext::Implicit,
        'a' => CoercionContext::Assignment,
        _ => CoercionContext::Explicit,
    }
}

/// The user cast from `from` to `to` usable in `context` (a domain converts like its base).
pub fn find(cat: &Catalog, from: &Type, to: &Type, context: CoercionContext) -> Option<CastDef> {
    if from == to || *from == Type::Unknown {
        return None;
    }
    cat.find_cast(&from.base(), to).filter(|c| context_of(c.context) <= context).map(|c| (**c).clone())
}

fn pseudo(t: &Type) -> bool {
    matches!(t, Type::Void | Type::Record | Type::Unknown)
}

/// `IsBinaryCoercible` for the types pgmem knows.
fn binary_coercible(a: &Type, b: &Type) -> bool {
    let (a, b) = (a.base(), b.base());
    a == b
        || matches!(
            (&a, &b),
            (Type::Varchar | Type::Bpchar | Type::Text, Type::Text | Type::Varchar)
                | (Type::Text, Type::Bpchar)
                | (Type::Int4 | Type::Oid | Type::Reg(_), Type::Int4 | Type::Oid | Type::Reg(_))
                | (Type::Cidr, Type::Inet)
        )
}

/// The type as an error message prints it, qualified (`format_type_be_qualified`).
fn qualified(cat: &Catalog, t: &Type) -> String {
    match t {
        Type::Array(e) => format!("{}[]", qualified(cat, e)),
        Type::Enum(_) | Type::Composite(_) | Type::Domain(_) => {
            cat.type_by_oid(t.oid()).map_or_else(|| t.name(), |d| format!("{}.{}", d.schema, d.name))
        }
        other => format!("pg_catalog.{}", other.typname()),
    }
}

fn already_exists(s: &Type, t: &Type) -> SqlError {
    SqlError::new(code::DUPLICATE_OBJECT, format!("cast from type {} to type {} already exists", s.name(), t.name()))
}

/// CREATE CAST.
pub(crate) fn create_cast(cc: &ir::CreateCast, txn: &mut Txn) -> SqlResult<QueryResult> {
    let cat = txn.catalog().clone();
    let (source, _) = resolve_type_name(&cat, &cc.source)?;
    let (target, _) = resolve_type_name(&cat, &cc.target)?;
    for (t, what) in [(&source, "source"), (&target, "target")] {
        if pseudo(t) {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("{what} data type {} is a pseudo-type", t.name())));
        }
    }
    let bad = |m: &str| SqlError::new(code::INVALID_OBJECT_DEFINITION, m.to_owned());
    let (function, method) = if let Some((name, args)) = &cc.function {
        let f = crate::functions::find_routine(&cat, name, Some(args))?;
        let inputs = f.input_types();
        if inputs.is_empty() || inputs.len() > 3 {
            return Err(bad("cast function must take one to three arguments"));
        }
        if !binary_coercible(&source, &inputs[0]) {
            return Err(bad("argument of cast function must match or be binary-coercible from source data type"));
        }
        if inputs.len() > 1 && inputs[1] != Type::Int4 {
            return Err(bad("second argument of cast function must be type integer"));
        }
        if inputs.len() > 2 && inputs[2] != Type::Bool {
            return Err(bad("third argument of cast function must be type boolean"));
        }
        let ret = match &f.ret {
            FuncReturn::Scalar(t) | FuncReturn::SetOf(t) => t.clone(),
            _ => Type::Record,
        };
        if !binary_coercible(&ret, &target) {
            return Err(bad("return data type of cast function must match or be binary-coercible to target data type"));
        }
        if f.returns_set() {
            return Err(bad("cast function must not return a set"));
        }
        if source == target && inputs.len() < 2 {
            return Err(bad("source data type and target data type are the same"));
        }
        (Some(f), 'f')
    } else {
        (None, if cc.inout { 'i' } else { 'b' })
    };
    let mut result = QueryResult { tag: "CREATE CAST".into(), ..Default::default() };
    let warn = |m: &str| Notice { severity: "WARNING", code: "01000", message: m.to_owned() };
    if matches!(source, Type::Domain(_)) {
        result.notices.push(warn("cast will be ignored because the source data type is a domain"));
    } else if matches!(target, Type::Domain(_)) {
        result.notices.push(warn("cast will be ignored because the target data type is a domain"));
    }
    if method == 'b' {
        // Composite values are 8-byte aligned, other varlena values 4-byte aligned.
        if source.size() != target.size() || matches!(source, Type::Composite(_)) != matches!(target, Type::Composite(_)) {
            return Err(bad("source and target data types are not physically compatible"));
        }
        if matches!(source, Type::Enum(_)) || matches!(target, Type::Enum(_)) {
            return Err(bad("enum data types are not binary-compatible"));
        }
        if matches!(source, Type::Domain(_)) || matches!(target, Type::Domain(_)) {
            return Err(bad("domain data types must not be marked binary-compatible"));
        }
        if source == target {
            return Err(bad("source data type and target data type are the same"));
        }
    }
    if cat.find_cast(&source, &target).is_some() || crate::catalog_views::builtin_cast_exists(source.oid(), target.oid()) {
        return Err(already_exists(&source, &target));
    }
    let oid = txn.catalog_mut().alloc_oid();
    let function_args = function.as_ref().map_or(0, |f| f.input_types().len());
    let function = function.map(|f| f.oid);
    txn.catalog_mut().put_cast(CastDef { oid, source, target, function, context: cc.context, method, function_args });
    Ok(result)
}

/// DROP CAST.
pub(crate) fn drop_cast(source: &ir::TypeName, target: &ir::TypeName, if_exists: bool, txn: &mut Txn) -> SqlResult<QueryResult> {
    let cat = txn.catalog().clone();
    let (s, t) = match (resolve_type_name(&cat, source), resolve_type_name(&cat, target)) {
        (Ok((s, _)), Ok((t, _))) => (s, t),
        (Err(e), _) | (_, Err(e)) if if_exists && e.code == code::UNDEFINED_OBJECT => {
            return Ok(QueryResult {
                tag: "DROP CAST".into(),
                notices: vec![Notice { severity: "NOTICE", code: "00000", message: format!("{}, skipping", e.message) }],
                ..Default::default()
            });
        }
        (Err(e), _) | (_, Err(e)) => return Err(e),
    };
    match cat.find_cast(&s, &t) {
        Some(c) => {
            txn.catalog_mut().drop_cast(c.oid);
            Ok(QueryResult { tag: "DROP CAST".into(), ..Default::default() })
        }
        None if crate::catalog_views::builtin_cast_exists(s.oid(), t.oid()) => Err(SqlError::new(
            code::DEPENDENT_OBJECTS_STILL_EXIST,
            format!("cannot drop cast from {} to {} because it is required by the database system", s.name(), t.name()),
        )),
        None if if_exists => Ok(QueryResult {
            tag: "DROP CAST".into(),
            notices: vec![Notice {
                severity: "NOTICE",
                code: "00000",
                message: format!("cast from type {} to type {} does not exist, skipping", qualified(&cat, &s), qualified(&cat, &t)),
            }],
            ..Default::default()
        }),
        None => Err(SqlError::new(code::UNDEFINED_OBJECT, format!("cast from type {} to type {} does not exist", s.name(), t.name()))),
    }
}

/// `cast from integer to mood`, how `pg_describe_object` and dependency messages name a cast.
pub fn describe(c: &CastDef) -> String {
    format!("cast from {} to {}", c.source.name(), c.target.name())
}
