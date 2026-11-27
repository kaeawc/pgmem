//! Roles and privileges: CREATE / ALTER / DROP ROLE, GRANT / REVOKE (objects and roles),
//! default privileges, ownership, DROP / REASSIGN OWNED, privilege checks (`aclchk.c`), and
//! the `has_*_privilege` functions. The session user is a superuser; so are roles created
//! SUPERUSER.

use crate::privilege_uses::Need;
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, DefaultPrivilege, GrantObject, Oid, Relation, RoleDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

/// The role privileges are checked for: the owner of the SECURITY DEFINER function running,
/// else `SET ROLE`'s, else the session user.
pub fn current_role(env: &SessionEnv) -> String {
    if let Some(owner) = &env.security_definer {
        return owner.clone();
    }
    match env.settings.get("role") {
        Some(r) if !r.is_empty() && r != "none" => r.clone(),
        _ => env.user.clone(),
    }
}

pub fn role_exists(catalog: &Catalog, env: &SessionEnv, name: &str) -> bool {
    name == env.user || catalog.role(name).is_some() || crate::catalog_views::predefined_role(name).is_some()
}

fn missing_role(name: &str) -> SqlError {
    SqlError::new(code::UNDEFINED_OBJECT, format!("role \"{name}\" does not exist"))
}

/// Resolves CURRENT_USER / SESSION_USER / CURRENT_ROLE and checks the role exists.
fn resolve_role(catalog: &Catalog, env: &SessionEnv, name: &str) -> SqlResult<String> {
    let n = match name {
        "current_user" | "current_role" => current_role(env),
        "session_user" => env.user.clone(),
        other => other.to_owned(),
    };
    if !role_exists(catalog, env, &n) {
        return Err(missing_role(&n));
    }
    Ok(n)
}

pub fn is_superuser(catalog: &Catalog, env: &SessionEnv, role: &str) -> bool {
    role == env.user || catalog.role(role).is_some_and(|r| r.superuser)
}

/// The privilege letters an object kind has, for ALL and validation.
fn object_privileges(object: &GrantObject, sequence: bool) -> &'static str {
    match object {
        GrantObject::Relation(_) if sequence => "rwU",
        GrantObject::Relation(_) => "arwdDxtm",
        GrantObject::Schema(_) => "UC",
        GrantObject::Database => "CTc",
        GrantObject::Function(_) => "X",
        GrantObject::Type(_) => "U",
        GrantObject::Column(..) => "arwx",
    }
}

/// `aclitem` letter of a privilege keyword.
fn privilege_letter(word: &str) -> Option<char> {
    Some(match word {
        "select" => 'r',
        "insert" => 'a',
        "update" => 'w',
        "delete" => 'd',
        "truncate" => 'D',
        "references" => 'x',
        "trigger" => 't',
        "maintain" => 'm',
        "usage" => 'U',
        "create" => 'C',
        "connect" => 'c',
        "temporary" | "temp" => 'T',
        "execute" => 'X',
        _ => return None,
    })
}

pub fn privilege_name(c: char) -> &'static str {
    match c {
        'r' => "SELECT",
        'a' => "INSERT",
        'w' => "UPDATE",
        'd' => "DELETE",
        'D' => "TRUNCATE",
        'x' => "REFERENCES",
        't' => "TRIGGER",
        'm' => "MAINTAIN",
        'U' => "USAGE",
        'C' => "CREATE",
        'c' => "CONNECT",
        'T' => "TEMPORARY",
        _ => "EXECUTE",
    }
}

fn kind_name(object: &GrantObject, sequence: bool) -> &'static str {
    match object {
        GrantObject::Relation(_) if sequence => "sequence",
        GrantObject::Relation(_) => "table",
        GrantObject::Schema(_) => "schema",
        GrantObject::Database => "database",
        GrantObject::Function(_) => "function",
        GrantObject::Type(_) => "type",
        GrantObject::Column(..) => "column",
    }
}

/// The letters a GRANT names on an object (ALL: every privilege of the kind).
fn letters(privs: Option<&[String]>, object: &GrantObject, sequence: bool) -> SqlResult<Vec<char>> {
    let valid = object_privileges(object, sequence);
    match privs {
        None => Ok(valid.chars().collect()),
        Some(ws) => ws
            .iter()
            .map(|w| {
                let c = privilege_letter(w).ok_or_else(|| SqlError::syntax(format!("unrecognized privilege type \"{w}\"")))?;
                if !valid.contains(c) {
                    return Err(SqlError::new(
                        code::INVALID_GRANT_OPERATION,
                        format!("invalid privilege type {} for {}", w.to_uppercase(), kind_name(object, sequence)),
                    ));
                }
                Ok(c)
            })
            .collect(),
    }
}

/// Privilege letters in `aclitem` output order (Postgres' ACL bit order).
const ACL_ORDER: &str = "arwdDxtXUCcTsAm";

/// A role name as `aclitem` output writes it (`putid`): quoted unless alphanumeric/underscore.
pub fn acl_name(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return name.to_owned();
    }
    let mut out = String::from("\"");
    for c in name.chars() {
        if c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// An object's ACL (`relacl`, ...) as `aclitem` texts: None until something was granted on
/// it (the default privileges), else the owner's entry then each grantee's (`role=rw*/grantor`).
pub fn acl_items(catalog: &Catalog, env: &SessionEnv, object: &GrantObject, oid: Oid, sequence: bool) -> Option<Vec<String>> {
    let grants: Vec<_> = catalog.grants().iter().filter(|g| g.object == *object).collect();
    if grants.is_empty() {
        return None;
    }
    let owner = owner_of(catalog, env, oid);
    let mut items = Vec::new();
    // An explicit owner entry (from default privileges) keeps its position; otherwise the
    // owner's implicit entry leads.
    if !grants.iter().any(|g| g.grantee == owner) {
        items.push(format!("{}={}/{}", acl_name(&owner), object_privileges(object, sequence), acl_name(&owner)));
    }
    items.extend(grants.iter().map(|g| acl_item(g)));
    Some(items)
}

/// One `aclitem` text: `grantee=letters/grantor` (`*` after a grantable letter; PUBLIC is an
/// empty grantee; names quoted as `putid` does).
fn acl_item(g: &pgmem_catalog::Grant) -> String {
    let letters: String = ACL_ORDER
        .chars()
        .filter(|c| g.privileges.contains(c))
        .map(|c| if g.grantable.contains(&c) { format!("{c}*") } else { c.to_string() })
        .collect();
    format!("{}={letters}/{}", acl_name(&g.grantee), acl_name(&g.grantor))
}

/// A column's `attacl`: None while nothing is granted on the column itself (column ACLs
/// carry no owner entry; the owner's rights come from the table's ACL).
pub fn column_acl_items(catalog: &Catalog, rel: Oid, attnum: i16) -> Option<Vec<String>> {
    let object = GrantObject::Column(rel, attnum);
    let items: Vec<String> = catalog.grants().iter().filter(|g| g.object == object).map(acl_item).collect();
    (!items.is_empty()).then_some(items)
}

/// The live columns of a table or view, as (attnum, name).
pub fn relation_columns(catalog: &Catalog, rel: Oid) -> Vec<(i16, String)> {
    let attnum = |i: usize| i16::try_from(i + 1).unwrap_or(i16::MAX);
    match catalog.relation_by_oid(rel) {
        Some(Relation::Table(t)) => t.live_columns().map(|(i, c)| (attnum(i), c.name.clone())).collect(),
        Some(Relation::View(v)) => v.columns.iter().enumerate().map(|(i, n)| (attnum(i), n.clone())).collect(),
        _ => Vec::new(),
    }
}

/// Whether `role` holds privilege `p` on the table `rel` or on at least one of its columns
/// (`has_any_column_privilege`, and the check for a statement that reads no column).
pub fn has_any_column_privilege(catalog: &Catalog, env: &SessionEnv, role: &str, rel: Oid, p: char) -> bool {
    has_privilege(catalog, env, role, &GrantObject::Relation(rel), p)
        || relation_columns(catalog, rel).iter().any(|(a, _)| column_granted(catalog, role, rel, *a, p))
}

/// Whether a column-level grant (to `role`, a role it belongs to, or PUBLIC) gives `p`.
fn column_granted(catalog: &Catalog, role: &str, rel: Oid, attnum: i16, p: char) -> bool {
    let roles = catalog.roles_of(role);
    let object = GrantObject::Column(rel, attnum);
    catalog.grants().iter().any(|g| g.object == object && (g.grantee.is_empty() || roles.contains(&g.grantee)) && g.privileges.contains(&p))
}

/// The owner of an object (the session user when none is recorded).
pub fn owner_of(catalog: &Catalog, env: &SessionEnv, oid: Oid) -> String {
    catalog.owner(oid).map_or_else(|| env.user.clone(), str::to_owned)
}

/// Whether `role` holds privilege `p` on `object`: superusers and owners hold everything;
/// otherwise a grant to the role, a role it belongs to, or PUBLIC.
pub fn has_privilege(catalog: &Catalog, env: &SessionEnv, role: &str, object: &GrantObject, p: char) -> bool {
    if is_superuser(catalog, env, role) {
        return true;
    }
    let roles = catalog.roles_of(role);
    let owner = match object {
        GrantObject::Relation(o) | GrantObject::Function(o) | GrantObject::Type(o) | GrantObject::Column(o, _) => {
            Some(owner_of(catalog, env, *o))
        }
        GrantObject::Schema(s) => Some(catalog.schema(s).map_or_else(|| env.user.clone(), |sc| sc.owner.clone())),
        GrantObject::Database => None,
    };
    if owner.as_ref().is_some_and(|o| roles.contains(o)) {
        return true;
    }
    // Postgres' defaults: PUBLIC may use the public schema and system schemas, execute
    // functions, use types, connect and create temporary tables.
    let public_default = match object {
        GrantObject::Schema(s) => p == 'U' && (s == "public" || s == "pg_catalog" || s == "information_schema" || s.starts_with("pg_temp")),
        GrantObject::Function(_) => p == 'X',
        GrantObject::Type(_) => p == 'U',
        GrantObject::Database => p == 'c' || p == 'T',
        GrantObject::Relation(_) | GrantObject::Column(..) => false,
    };
    let revoked_public = catalog.grants().iter().any(|g| g.object == *object && g.grantee == "-revoked:public");
    if public_default && !revoked_public {
        return true;
    }
    let held = |object: &GrantObject| {
        catalog
            .grants()
            .iter()
            .any(|g| g.object == *object && (g.grantee.is_empty() || roles.contains(&g.grantee)) && g.privileges.contains(&p))
    };
    match object {
        // A table-level privilege covers every column.
        GrantObject::Column(rel, _) => held(&GrantObject::Relation(*rel)) || held(object),
        _ => held(object),
    }
}

/// Whether `role` holds privilege `p` on `object` WITH GRANT OPTION (`... WITH GRANT OPTION`
/// in the privilege-inquiry functions): owners and superusers do; otherwise a grantable
/// grant, on the table itself also for a column.
pub fn has_grant_option(catalog: &Catalog, env: &SessionEnv, role: &str, object: &GrantObject, p: char) -> bool {
    if is_superuser(catalog, env, role) {
        return true;
    }
    let roles = catalog.roles_of(role);
    if object.relation_oid().is_some_and(|o| roles.contains(&owner_of(catalog, env, o))) {
        return true;
    }
    let held = |object: &GrantObject| {
        catalog.grants().iter().any(|g| {
            g.object == *object
                && (g.grantee.is_empty() || roles.contains(&g.grantee))
                && g.privileges.contains(&p)
                && g.grantable.contains(&p)
        })
    };
    match object {
        GrantObject::Column(rel, _) => held(&GrantObject::Relation(*rel)) || held(object),
        _ => held(object),
    }
}

// ---------------------------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------------------------

fn apply_options(r: &mut RoleDef, o: &ir::RoleOptions) {
    if let Some(v) = o.superuser {
        r.superuser = v;
    }
    if let Some(v) = o.createdb {
        r.createdb = v;
    }
    if let Some(v) = o.createrole {
        r.createrole = v;
    }
    if let Some(v) = o.inherit {
        r.inherit = v;
    }
    if let Some(v) = o.login {
        r.login = v;
    }
    if let Some(v) = o.replication {
        r.replication = v;
    }
    if let Some(v) = o.bypassrls {
        r.bypassrls = v;
    }
    if let Some(v) = o.connlimit {
        r.connlimit = v;
    }
    if let Some(p) = &o.password {
        r.password.clone_from(p);
    }
    if let Some(v) = &o.valid_until {
        r.valid_until = Some(v.clone());
    }
}

fn require_createrole(txn: &Txn, env: &SessionEnv) -> SqlResult<()> {
    let me = current_role(env);
    if is_superuser(txn.catalog(), env, &me) || txn.catalog().role(&me).is_some_and(|r| r.createrole) {
        return Ok(());
    }
    Err(SqlError::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to create role")
        .with_detail("Only roles with the CREATEROLE attribute may create roles."))
}

pub(crate) fn create_role(name: &str, o: &ir::RoleOptions, if_not_exists: bool, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    require_createrole(txn, env)?;
    if name.starts_with("pg_") {
        return Err(SqlError::new(code::RESERVED_NAME, format!("role name \"{name}\" is reserved"))
            .with_detail("Role names starting with \"pg_\" are reserved."));
    }
    if role_exists(txn.catalog(), env, name) {
        if if_not_exists {
            return Ok(QueryResult {
                tag: "CREATE ROLE".into(),
                notices: vec![Notice { severity: "NOTICE", code: "00000", message: format!("role \"{name}\" already exists, skipping") }],
                ..Default::default()
            });
        }
        return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("role \"{name}\" already exists")));
    }
    for r in o.in_roles.iter().chain(&o.members).chain(&o.admins) {
        if !role_exists(txn.catalog(), env, r) {
            return Err(missing_role(r));
        }
    }
    let oid = txn.catalog_mut().alloc_oid();
    let mut role = RoleDef::new(oid, name);
    apply_options(&mut role, o);
    let cat = txn.catalog_mut();
    cat.put_role(role);
    for r in &o.in_roles {
        cat.add_member(r, name, false);
    }
    for m in &o.members {
        cat.add_member(name, m, false);
    }
    for m in &o.admins {
        cat.add_member(name, m, true);
    }
    Ok(tag("CREATE ROLE"))
}

pub(crate) fn alter_role(name: &str, action: &ir::AlterRoleAction, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    let name = resolve_role(txn.catalog(), env, name)?;
    let existing = txn.catalog().role(&name).cloned();
    let mut role = match existing {
        Some(r) => (*r).clone(),
        // The session user and predefined roles: altering them is accepted.
        None => return Ok(tag("ALTER ROLE")),
    };
    match action {
        ir::AlterRoleAction::Options(o) => apply_options(&mut role, o),
        ir::AlterRoleAction::Rename(new) => {
            if role_exists(txn.catalog(), env, new) {
                return Err(SqlError::new(code::DUPLICATE_OBJECT, format!("role \"{new}\" already exists")));
            }
            if current_role(env) == name {
                return Err(SqlError::new(code::FEATURE_NOT_SUPPORTED, "current user cannot be renamed"));
            }
            txn.catalog_mut().rename_role(&name, new);
            let notice = role.password.is_some().then(|| Notice {
                severity: "NOTICE",
                code: "00000",
                message: "MD5 password cleared because of role rename".into(),
            });
            return Ok(QueryResult { tag: "ALTER ROLE".into(), notices: notice.into_iter().collect(), ..Default::default() });
        }
        ir::AlterRoleAction::Set(param, value) => {
            role.config.retain(|c| !c.starts_with(&format!("{param}=")));
            if let Some(v) = value {
                role.config.push(format!("{param}={v}"));
            }
        }
        ir::AlterRoleAction::Reset(param) => match param {
            Some(p) => role.config.retain(|c| !c.starts_with(&format!("{p}="))),
            None => role.config.clear(),
        },
    }
    txn.catalog_mut().put_role(role);
    Ok(tag("ALTER ROLE"))
}

/// What depends on a role (DROP ROLE's DETAIL lines).
fn role_dependencies(catalog: &Catalog, role: &str) -> Vec<String> {
    let mut out = Vec::new();
    for oid in catalog.owned_by(role) {
        if let Some(r) = catalog.relation_by_oid(oid) {
            out.push(format!("owner of {} {}", r.kind_name(), r.name()));
        } else if let Some(f) = catalog.function_by_oid(oid) {
            out.push(format!("owner of function {}", crate::functions::signature(&f.name, &f.input_types())));
        }
    }
    for g in catalog.grants().iter().filter(|g| g.grantee == role) {
        match &g.object {
            // The owner's own ACL entry (from default privileges) is not a dependency.
            GrantObject::Relation(o) if catalog.owner(*o) == Some(role) => {}
            GrantObject::Relation(o) => {
                if let Some(r) = catalog.relation_by_oid(*o) {
                    out.push(format!("privileges for {} {}", r.kind_name(), r.name()));
                }
            }
            GrantObject::Schema(s) => out.push(format!("privileges for schema {s}")),
            GrantObject::Function(o) => {
                if let Some(f) = catalog.function_by_oid(*o) {
                    out.push(format!("privileges for function {}", crate::functions::signature(&f.name, &f.input_types())));
                }
            }
            GrantObject::Database => out.push("privileges for database".to_owned()),
            GrantObject::Type(_) => out.push("privileges for type".to_owned()),
            GrantObject::Column(o, attnum) => {
                let column = relation_columns(catalog, *o).into_iter().find(|(a, _)| a == attnum);
                if let (Some(r), Some((_, column))) = (catalog.relation_by_oid(*o), column) {
                    out.push(format!("privileges for column {column} of {} {}", relation_kind(&r), r.name()));
                }
            }
        }
    }
    for d in catalog.default_privileges() {
        let what = match d.kind {
            'S' => "sequences",
            'f' => "functions",
            'T' => "types",
            'n' => "schemas",
            _ => "relations",
        };
        let in_schema = d.schema.as_ref().map_or(String::new(), |s| format!(" in schema {s}"));
        let on = format!("default privileges on new {what} belonging to role {}{in_schema}", d.role);
        if d.role == role {
            out.push(format!("owner of {on}"));
        } else if d.items.iter().any(|x| x.grantee == role) {
            out.push(format!("privileges for {on}"));
        }
    }
    out
}

pub(crate) fn drop_role(names: &[String], if_exists: bool, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    require_createrole(txn, env)?;
    let mut notices = Vec::new();
    for name in names {
        if name == &env.user || name == &current_role(env) {
            return Err(SqlError::new(
                code::OBJECT_IN_USE,
                format!("{} user cannot be dropped", if name == &env.user { "session" } else { "current" }),
            ));
        }
        if crate::catalog_views::predefined_role(name).is_some() {
            return Err(SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("cannot drop role {name}"))
                .with_detail("Roles with the pg_ prefix are reserved."));
        }
        if txn.catalog().role(name).is_none() {
            if if_exists {
                notices.push(Notice { severity: "NOTICE", code: "00000", message: format!("role \"{name}\" does not exist, skipping") });
                continue;
            }
            return Err(missing_role(name));
        }
        let deps = role_dependencies(txn.catalog(), name);
        if !deps.is_empty() {
            return Err(SqlError::new(
                code::DEPENDENT_OBJECTS_STILL_EXIST,
                format!("role \"{name}\" cannot be dropped because some objects depend on it"),
            )
            .with_detail(deps.join("\n")));
        }
        txn.catalog_mut().drop_role(name);
    }
    Ok(QueryResult { tag: "DROP ROLE".into(), notices, ..Default::default() })
}

/// The objects a GRANT target names: (object, is sequence).
fn target_objects(catalog: &Catalog, target: &ir::GrantTarget) -> SqlResult<Vec<(GrantObject, bool)>> {
    let rel = |n: &ir::ObjectName, want_seq: Option<bool>| -> SqlResult<(GrantObject, bool)> {
        let r = catalog.lookup_relation(n)?.ok_or_else(|| SqlError::undefined_table(&n.to_string()))?;
        let seq = matches!(r, Relation::Sequence(_));
        if want_seq == Some(true) && !seq {
            return Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a sequence", n.base())));
        }
        Ok((GrantObject::Relation(r.oid()), seq))
    };
    let schema_exists = |s: &str| -> SqlResult<()> {
        if catalog.schema(s).is_none() && !pgmem_catalog::is_system_schema(s) {
            return Err(pgmem_catalog::undefined_schema(s));
        }
        Ok(())
    };
    Ok(match target {
        ir::GrantTarget::Tables(ns) => ns.iter().map(|n| rel(n, None)).collect::<SqlResult<_>>()?,
        ir::GrantTarget::Sequences(ns) => ns.iter().map(|n| rel(n, Some(true))).collect::<SqlResult<_>>()?,
        ir::GrantTarget::AllTablesInSchema(ss) => {
            let mut out = Vec::new();
            for s in ss {
                schema_exists(s)?;
                out.extend(catalog.tables().filter(|t| &t.schema == s).map(|t| (GrantObject::Relation(t.oid), false)));
                out.extend(catalog.views().filter(|v| &v.schema == s).map(|v| (GrantObject::Relation(v.oid), false)));
            }
            out
        }
        ir::GrantTarget::AllSequencesInSchema(ss) => {
            let mut out = Vec::new();
            for s in ss {
                schema_exists(s)?;
                out.extend(catalog.sequences().filter(|q| &q.schema == s).map(|q| (GrantObject::Relation(q.oid), true)));
            }
            out
        }
        ir::GrantTarget::Schemas(ss) => {
            for s in ss {
                schema_exists(s)?;
            }
            ss.iter().map(|s| (GrantObject::Schema(s.clone()), false)).collect()
        }
        ir::GrantTarget::Databases(_) => vec![(GrantObject::Database, false)],
        ir::GrantTarget::Functions(fs) => fs
            .iter()
            .map(|(n, ts)| Ok((GrantObject::Function(crate::functions::find_routine(catalog, n, ts.as_deref())?.oid), false)))
            .collect::<SqlResult<_>>()?,
        ir::GrantTarget::AllFunctionsInSchema(ss) => {
            let mut out = Vec::new();
            for s in ss {
                schema_exists(s)?;
                out.extend(catalog.functions().filter(|f| &f.schema == s).map(|f| (GrantObject::Function(f.oid), false)));
            }
            out
        }
        ir::GrantTarget::Types(ns) => ns
            .iter()
            .map(|n| {
                let t = catalog
                    .lookup_type(n)?
                    .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{}\" does not exist", n.base())))?;
                Ok((GrantObject::Type(t.oid), false))
            })
            .collect::<SqlResult<_>>()?,
        ir::GrantTarget::Other => Vec::new(),
    })
}

pub(crate) fn grant(g: &ir::GrantStmt, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let grantees: Vec<String> = g
        .grantees
        .iter()
        .map(|r| if r == "public" { Ok(String::new()) } else { resolve_role(&catalog, env, r) })
        .collect::<SqlResult<_>>()?;
    let me = current_role(env);
    if !g.columns.is_empty() && !matches!(g.target, ir::GrantTarget::Tables(_) | ir::GrantTarget::AllTablesInSchema(_)) {
        return Err(SqlError::new(code::INVALID_GRANT_OPERATION, "column privileges are only valid for relations"));
    }
    // Privileges with a column list: (letter, columns).
    let mut column_privs: Vec<(char, &[String])> = Vec::new();
    let table_words: Option<Vec<String>> = match (&g.privileges, g.columns.first()) {
        (None, Some(cols)) => {
            column_privs.extend(object_privileges(&GrantObject::Column(0, 0), false).chars().map(|c| (c, cols.as_slice())));
            Some(Vec::new())
        }
        (Some(ws), Some(_)) => {
            let mut table = Vec::new();
            for (w, cols) in ws.iter().zip(&g.columns) {
                if cols.is_empty() {
                    table.push(w.clone());
                    continue;
                }
                let c = privilege_letter(w).ok_or_else(|| SqlError::syntax(format!("unrecognized privilege type \"{w}\"")))?;
                if !object_privileges(&GrantObject::Column(0, 0), false).contains(c) {
                    return Err(SqlError::new(
                        code::INVALID_GRANT_OPERATION,
                        format!("invalid privilege type {} for column", w.to_uppercase()),
                    ));
                }
                column_privs.push((c, cols));
            }
            Some(table)
        }
        (privs, None) => privs.clone(),
    };
    for (object, sequence) in target_objects(&catalog, &g.target)? {
        let privs = match &table_words {
            Some(ws) if ws.is_empty() => Vec::new(),
            ws => letters(ws.as_deref(), &object, sequence)?,
        };
        let rel = object.relation_oid().unwrap_or(0);
        // Column grants: (column object, letters), the columns resolved by name.
        let mut column_grants: Vec<(GrantObject, Vec<char>)> = Vec::new();
        if !column_privs.is_empty() {
            let columns = relation_columns(&catalog, rel);
            for (c, names) in &column_privs {
                for n in *names {
                    let Some((attnum, _)) = columns.iter().find(|(_, cn)| cn == n) else {
                        let relname = catalog.relation_by_oid(rel).map(|r| r.name().to_owned()).unwrap_or_default();
                        return Err(SqlError::new(
                            code::UNDEFINED_COLUMN,
                            format!("column \"{n}\" of relation \"{relname}\" does not exist"),
                        ));
                    };
                    let object = GrantObject::Column(rel, *attnum);
                    match column_grants.iter_mut().find(|(o, _)| *o == object) {
                        Some((_, ls)) if !ls.contains(c) => ls.push(*c),
                        Some(_) => {}
                        None => column_grants.push((object, vec![*c])),
                    }
                }
            }
        }
        let cat = txn.catalog_mut();
        for grantee in &grantees {
            for (column, ls) in &column_grants {
                if g.revoke {
                    cat.revoke(column, grantee, ls, g.grant_option);
                } else {
                    cat.grant(column.clone(), grantee, &me, ls, g.grant_option);
                }
            }
            if privs.is_empty() {
                continue;
            }
            if g.revoke {
                cat.revoke(&object, grantee, &privs, g.grant_option);
                // Revoking a table privilege revokes it from every column too.
                let column_letters: Vec<char> = privs.iter().copied().filter(|c| "arwx".contains(*c)).collect();
                if !column_letters.is_empty() && matches!(object, GrantObject::Relation(_)) {
                    for (attnum, _) in relation_columns(&catalog, rel) {
                        cat.revoke(&GrantObject::Column(rel, attnum), grantee, &column_letters, g.grant_option);
                    }
                }
                if let Some(p) = public_default_letter(&object)
                    && grantee.is_empty()
                    && privs.contains(&p)
                {
                    // Revoking PUBLIC's default USAGE on a schema or EXECUTE on a function.
                    cat.grant(object.clone(), "-revoked:public", &me, &[p], false);
                }
            } else {
                if let Some(p) = public_default_letter(&object)
                    && grantee.is_empty()
                {
                    cat.revoke(&object, "-revoked:public", &[p], false);
                }
                cat.grant(object.clone(), grantee, &me, &privs, g.grant_option);
            }
        }
    }
    Ok(tag(if g.revoke { "REVOKE" } else { "GRANT" }))
}

pub(crate) fn grant_role(
    roles: &[String],
    members: &[String],
    admin: bool,
    revoke: bool,
    txn: &mut Txn,
    env: &SessionEnv,
) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let roles: Vec<String> = roles.iter().map(|r| resolve_role(&catalog, env, r)).collect::<SqlResult<_>>()?;
    let members: Vec<String> = members.iter().map(|r| resolve_role(&catalog, env, r)).collect::<SqlResult<_>>()?;
    let mut notices = Vec::new();
    for r in &roles {
        for m in &members {
            if revoke {
                if !txn.catalog_mut().remove_member(r, m) {
                    notices.push(Notice {
                        severity: "WARNING",
                        code: "01000",
                        message: format!("role \"{m}\" has not been granted membership in role \"{r}\" by role \"{}\"", current_role(env)),
                    });
                }
                continue;
            }
            if r == m || catalog.roles_of(r).contains(m) {
                return Err(SqlError::new(code::INVALID_GRANT_OPERATION, format!("role \"{r}\" is a member of role \"{m}\"")));
            }
            if catalog.memberships().iter().any(|(gr, gm, _)| gr == r && gm == m) {
                notices.push(Notice {
                    severity: "NOTICE",
                    code: "00000",
                    message: format!("role \"{m}\" has already been granted membership in role \"{r}\" by role \"{}\"", current_role(env)),
                });
            }
            txn.catalog_mut().add_member(r, m, admin);
        }
    }
    Ok(QueryResult { tag: if revoke { "REVOKE ROLE" } else { "GRANT ROLE" }.into(), notices, ..Default::default() })
}

pub(crate) fn alter_default_privileges(
    roles: &[String],
    schemas: &[String],
    kind: char,
    g: &ir::GrantStmt,
    txn: &mut Txn,
    env: &SessionEnv,
) -> SqlResult<QueryResult> {
    if g.columns.iter().any(|c| !c.is_empty()) {
        return Err(SqlError::new(code::INVALID_GRANT_OPERATION, "default privileges cannot be set for columns"));
    }
    let catalog = txn.catalog().clone();
    let probe = match kind {
        'S' => (GrantObject::Relation(0), true),
        'f' => (GrantObject::Function(0), false),
        'T' => (GrantObject::Type(0), false),
        'n' => (GrantObject::Schema(String::new()), false),
        _ => (GrantObject::Relation(0), false),
    };
    let privs = letters(g.privileges.as_deref(), &probe.0, probe.1)?;
    let schema_list: Vec<Option<String>> = if schemas.is_empty() { vec![None] } else { schemas.iter().cloned().map(Some).collect() };
    let me = current_role(env);
    let owners: Vec<String> =
        if roles.is_empty() { vec![me.clone()] } else { roles.iter().map(|r| resolve_role(&catalog, env, r)).collect::<SqlResult<_>>()? };
    for owner in &owners {
        if !is_superuser(&catalog, env, &me) && !catalog.roles_of(&me).contains(owner) {
            return Err(SqlError::new(code::INSUFFICIENT_PRIVILEGE, "permission denied to change default privileges"));
        }
    }
    for grantee in &g.grantees {
        let grantee = if grantee == "public" { String::new() } else { resolve_role(&catalog, env, grantee)? };
        for owner in &owners {
            for s in &schema_list {
                let mut entry = txn
                    .catalog()
                    .default_privilege(owner, s.as_deref(), kind)
                    .cloned()
                    .unwrap_or_else(|| DefaultPrivilege::new(owner, s.as_deref(), kind));
                if g.revoke {
                    entry.revoke(&grantee, &privs, g.grant_option);
                } else {
                    entry.grant(&grantee, &privs, g.grant_option);
                }
                txn.catalog_mut().put_default_privilege(entry);
            }
        }
    }
    Ok(tag("ALTER DEFAULT PRIVILEGES"))
}

/// Default privileges for a new relation (`r` table or `S` sequence) in `schema`, created by
/// the current role: the role's global entry and its entry for the schema, merged.
pub(crate) fn apply_defaults(txn: &mut Txn, env: &SessionEnv, oid: Oid, schema: &str, kind: char) {
    let me = current_role(env);
    let cat = txn.catalog();
    let mut merged = DefaultPrivilege::new(&me, None, kind);
    if let Some(global) = cat.default_privilege(&me, None, kind) {
        merged = global.clone();
    }
    if let Some(local) = cat.default_privilege(&me, Some(schema), kind) {
        for x in &local.items {
            let plain: Vec<char> = x.privileges.iter().filter(|p| !x.grantable.contains(p)).copied().collect();
            merged.grant(&x.grantee, &plain, false);
            merged.grant(&x.grantee, &x.grantable, true);
        }
    }
    if merged.items.iter().all(|x| x.grantee == me) {
        return;
    }
    // Postgres sorts a merged default ACL by grantee OID (PUBLIC first).
    let catalog = txn.catalog().clone();
    merged
        .items
        .sort_by_key(|x| if x.grantee.is_empty() { 0 } else { crate::reg::role_oid(&catalog, &env.user, &x.grantee).unwrap_or(u32::MAX) });
    for x in &merged.items {
        let object = GrantObject::Relation(oid);
        let plain: Vec<char> = x.privileges.iter().filter(|p| !x.grantable.contains(p)).copied().collect();
        if !plain.is_empty() {
            txn.catalog_mut().grant(object.clone(), &x.grantee, &me, &plain, false);
        }
        if !x.grantable.is_empty() {
            txn.catalog_mut().grant(object, &x.grantee, &me, &x.grantable, true);
        }
    }
}

pub(crate) fn drop_owned(roles: &[String], txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let roles: Vec<String> = roles.iter().map(|r| resolve_role(&catalog, env, r)).collect::<SqlResult<_>>()?;
    for r in &roles {
        let cat = txn.catalog_mut();
        cat.drop_grants(|g| &g.grantee == r);
        cat.drop_default_privileges_of(r);
        // Owned objects are dropped.
        for oid in catalog.owned_by(r) {
            match catalog.relation_by_oid(oid) {
                Some(Relation::Table(t)) => {
                    cat.drop_table(&t.qualified());
                }
                Some(Relation::View(v)) => {
                    cat.drop_view(&pgmem_catalog::QualName::new(&v.schema, &v.name));
                }
                Some(Relation::Sequence(s)) => {
                    cat.drop_sequence(&pgmem_catalog::QualName::new(&s.schema, &s.name));
                }
                _ => {
                    cat.drop_function(oid);
                }
            }
            cat.set_owner(oid, None);
        }
    }
    Ok(tag("DROP OWNED"))
}

pub(crate) fn reassign_owned(roles: &[String], to: &str, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let to = resolve_role(&catalog, env, to)?;
    for r in roles {
        let r = resolve_role(&catalog, env, r)?;
        for oid in catalog.owned_by(&r) {
            txn.catalog_mut().set_owner(oid, (to != env.user).then_some(to.as_str()));
        }
    }
    Ok(tag("REASSIGN OWNED"))
}

/// Records `owner` for object `oid` (the session user is the implicit owner).
pub(crate) fn set_owner(txn: &mut Txn, env: &SessionEnv, oid: Oid, owner: &str) -> SqlResult<()> {
    let owner = resolve_role(txn.catalog(), env, owner)?;
    txn.catalog_mut().set_owner(oid, (owner != env.user).then_some(owner.as_str()));
    Ok(())
}

pub(crate) fn alter_owner(
    object: ir::OwnedObject,
    name: &ir::ObjectName,
    owner: &str,
    txn: &mut Txn,
    env: &SessionEnv,
) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let (oid, what) = match object {
        ir::OwnedObject::Schema => {
            let s = catalog.schema(name.base()).ok_or_else(|| pgmem_catalog::undefined_schema(name.base()))?;
            (s.oid, "ALTER SCHEMA")
        }
        ir::OwnedObject::Sequence | ir::OwnedObject::View | ir::OwnedObject::MaterializedView => {
            let r = catalog.lookup_relation(name)?.ok_or_else(|| SqlError::undefined_table(&name.to_string()))?;
            (r.oid(), if object == ir::OwnedObject::Sequence { "ALTER SEQUENCE" } else { "ALTER VIEW" })
        }
        ir::OwnedObject::Function | ir::OwnedObject::Procedure => {
            let f = crate::functions::find_routine(&catalog, name, None)?;
            (f.oid, if f.procedure { "ALTER PROCEDURE" } else { "ALTER FUNCTION" })
        }
        ir::OwnedObject::Type | ir::OwnedObject::Domain => {
            let t = catalog
                .lookup_type(name)?
                .ok_or_else(|| SqlError::new(code::UNDEFINED_OBJECT, format!("type \"{}\" does not exist", name.base())))?;
            (t.oid, if object == ir::OwnedObject::Type { "ALTER TYPE" } else { "ALTER DOMAIN" })
        }
        ir::OwnedObject::Database => {
            resolve_role(&catalog, env, owner)?;
            return Ok(tag("ALTER DATABASE"));
        }
    };
    set_owner(txn, env, oid, owner)?;
    if object == ir::OwnedObject::Schema
        && let Some(mut def) = txn.catalog_mut().drop_schema(name.base())
    {
        // The schema's own owner (nspowner, privilege checks) changes too.
        def.owner = resolve_role(&catalog, env, owner)?;
        txn.catalog_mut().create_schema(def)?;
    }
    Ok(tag(what))
}

// ---------------------------------------------------------------------------------------------
// Enforcement
// ---------------------------------------------------------------------------------------------

fn permission_denied(kind: &str, name: &str) -> SqlError {
    SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied for {kind} {name}"))
}

/// `table`, `view`, `materialized view` or `sequence`, as privilege errors name a relation.
fn relation_kind(rel: &Relation) -> &'static str {
    match rel {
        Relation::Table(t) if t.matview.is_some() => "materialized view",
        other => other.kind_name(),
    }
}

/// A relation privilege the role lacks at table level but may hold on the columns the
/// statement uses (Postgres' `ExecCheckOneRelPerms`).
struct ColumnCheck {
    rel: Oid,
    kind: &'static str,
    name: String,
    privilege: char,
    /// The DML target's own columns read by WHERE / RETURNING / ON CONFLICT: SELECT is
    /// needed only when some column is read.
    only_if_used: bool,
}

/// Checks the current role may run `stmt`: SELECT on every relation it reads (FROM / USING,
/// subqueries, the INSERT or MERGE source, WITH queries), the DML privileges on each target.
/// Superusers skip the checks. Without the table-level privilege, column privileges decide:
/// the statement is bound (not run) to find the columns it reads, inserts and updates, and
/// each needs the privilege (a statement that reads no column needs it on some column).
pub(crate) fn check_statement(
    stmt: &ir::Statement,
    catalog: &Catalog,
    env: &SessionEnv,
    param_types: &[pgmem_types::Type],
) -> SqlResult<()> {
    crate::ownership::check_ddl(stmt, catalog, env)?;
    let role = current_role(env);
    if is_superuser(catalog, env, &role) {
        return Ok(());
    }
    let uses = crate::privilege_uses::statement_uses(stmt);
    // A view target written by INSTEAD OF triggers (or not automatically updatable) is not
    // bound onto a base table: its columns come from the statement's column names.
    let trigger_view = target_view_columns(stmt, catalog);
    let mut columns: Vec<ColumnCheck> = Vec::new();
    for (name, need) in &uses {
        let Ok(Some(rel)) = catalog.find_relation(name) else { continue };
        if let Some(c) = relation_check(catalog, env, &role, &rel, *need)? {
            match columns.iter_mut().find(|x| x.rel == c.rel && x.privilege == c.privilege) {
                Some(x) => x.only_if_used &= c.only_if_used,
                None => columns.push(c),
            }
        }
    }
    let binder = crate::bind::Binder::new(catalog, param_types);
    binder.record_column_use();
    let bound = bind_for_columns(stmt, &binder, trigger_view.as_deref());
    if columns.is_empty() {
        // Only EXECUTE remains to check; a statement that does not bind reports that when it
        // runs.
        return if bound.is_ok() { check_function_use(&binder, catalog, env) } else { Ok(()) };
    }
    // A statement that fails to bind reports that, as Postgres reports parse-analysis errors
    // before permission errors.
    bound?;
    let used = binder.column_use();
    for c in &columns {
        let mut cols: Vec<i16> = used.iter().filter(|(rel, _, p)| *rel == c.rel && *p == c.privilege).map(|(_, a, _)| *a).collect();
        cols.sort_unstable();
        cols.dedup();
        let granted = if cols.is_empty() {
            c.only_if_used || has_any_column_privilege(catalog, env, &role, c.rel, c.privilege)
        } else {
            // System columns (attnum < 0) have no column grants here: table-level only.
            cols.iter().all(|&a| a > 0 && column_granted(catalog, &role, c.rel, a, c.privilege))
        };
        if !granted {
            return Err(permission_denied(c.kind, &c.name));
        }
    }
    check_function_use(&binder, catalog, env)
}

/// What `need` on `rel` requires of `role`: nothing (Ok(None)), a check of the columns the
/// statement uses, or a permission error.
fn relation_check(catalog: &Catalog, env: &SessionEnv, role: &str, rel: &Relation, need: Need) -> SqlResult<Option<ColumnCheck>> {
    if matches!(rel, Relation::Index { .. }) {
        return Ok(None);
    }
    let (kind, oid) = (relation_kind(rel), rel.oid());
    let column_check =
        |privilege: char, only_if_used: bool| ColumnCheck { rel: oid, kind, name: rel.name().to_owned(), privilege, only_if_used };
    match need {
        Need::TargetRead => Ok((matches!(rel, Relation::Table(_) | Relation::View(_))
            && !has_privilege(catalog, env, role, &GrantObject::Relation(oid), 'r'))
        .then(|| column_check('r', true))),
        Need::Privilege(p) => {
            if has_privilege(catalog, env, role, &GrantObject::Relation(oid), p) {
                return Ok(None);
            }
            if "rawx".contains(p) && !matches!(rel, Relation::Sequence(_)) && has_any_column_privilege(catalog, env, role, oid, p) {
                return Ok(Some(column_check(p, false)));
            }
            Err(permission_denied(kind, rel.name()))
        }
    }
}

/// Binds `stmt` to record the columns it uses. A view target with INSTEAD OF triggers
/// (`trigger_view`: the view columns inserted or updated) is bound as the query its trigger
/// rows come from.
fn bind_for_columns(stmt: &ir::Statement, binder: &crate::bind::Binder, trigger_view: Option<&[(Oid, i16, char)]>) -> SqlResult<()> {
    let top = crate::bind::Level::new(crate::bind::Scope::default());
    let ctx = crate::bind::Ctx::new(&top);
    if let Some(uses) = trigger_view {
        for &(rel, attnum, p) in uses {
            binder.note_use(rel, attnum, p);
        }
        if let ir::Statement::Insert(i) = stmt
            && let ir::InsertSource::Query(q) = &i.source
        {
            binder.bind_query(q, &ctx)?;
        }
        return match trigger_view_reads(stmt) {
            Some(q) => binder.bind_query(&q, &ctx).map(drop),
            None => Ok(()),
        };
    }
    if crate::statement::leading_with(stmt).is_some() {
        return crate::statement::describe_with(stmt, binder, &ctx).map(drop);
    }
    match stmt {
        ir::Statement::Query(q) => binder.bind_query(q, &ctx).map(drop),
        ir::Statement::Insert(i) => crate::bind::bind_insert(binder, i, &ctx).map(drop),
        ir::Statement::Update(u) => crate::bind::bind_update(binder, u, &ctx).map(drop),
        ir::Statement::Delete(d) => crate::bind::bind_delete(binder, d, &ctx).map(drop),
        ir::Statement::Merge(m) => crate::bind::merge::bind_merge(binder, m, &ctx).map(drop),
        _ => Ok(()),
    }
}

/// What an INSERT / UPDATE / DELETE on a view with INSTEAD OF triggers reads of the view, as
/// a query over it: SET expressions, WHERE and RETURNING.
fn trigger_view_reads(stmt: &ir::Statement) -> Option<ir::Query> {
    let (table, alias, from, values, selection, returning) = match stmt {
        ir::Statement::Insert(i) if !i.returning.is_empty() => (&i.table, &i.alias, &[][..], Vec::new(), None, &i.returning),
        ir::Statement::Update(u) => (&u.table, &u.alias, &u.from[..], set_sources(&u.assignments), u.selection.clone(), &u.returning),
        ir::Statement::Delete(d) => (&d.table, &d.alias, &d.using[..], Vec::new(), d.selection.clone(), &d.returning),
        _ => return None,
    };
    let alias = alias.as_ref().map(|a| ir::TableAlias { name: a.clone(), columns: Vec::new(), column_types: Vec::new() });
    let mut projection: Vec<ir::SelectItem> = values.into_iter().map(|expr| ir::SelectItem::Expr { expr, alias: None }).collect();
    projection.extend(returning.iter().cloned());
    let mut tables = vec![ir::TableRef::Table { name: table.clone(), alias }];
    tables.extend(from.iter().cloned());
    Some(ir::Query::from_body(ir::SetExpr::Select(Box::new(ir::Select { projection, from: tables, selection, ..Default::default() }))))
}

/// The expressions an UPDATE's SET list reads: DEFAULT (alone or in a row) reads nothing.
fn set_sources(assignments: &[ir::Assignment]) -> Vec<ir::Expr> {
    let mut out = Vec::new();
    for a in assignments {
        match &a.value {
            ir::Expr::Default => {}
            ir::Expr::Row(items) => out.extend(items.iter().filter(|e| !matches!(e, ir::Expr::Default)).cloned()),
            e => out.push(e.clone()),
        }
    }
    out
}

/// COPY of relation `rel`: SELECT (COPY TO) or INSERT (COPY FROM) on the table, or on each
/// copied column (`columns`, or every column but generated ones). Checked before the copy
/// starts and before the relation kind; an unknown column is left to the caller to report.
pub(crate) fn check_copy(catalog: &Catalog, env: &SessionEnv, rel: &Relation, columns: &[String], from: bool) -> SqlResult<()> {
    let role = current_role(env);
    let p = if from { 'a' } else { 'r' };
    let oid = rel.oid();
    if has_privilege(catalog, env, &role, &GrantObject::Relation(oid), p) {
        return Ok(());
    }
    let generated = |a: i16| match rel {
        Relation::Table(t) => usize::try_from(a - 1).ok().and_then(|i| t.columns.get(i)).is_some_and(|c| c.generated.is_some()),
        _ => false,
    };
    let all = relation_columns(catalog, oid);
    let mut copied: Vec<i16> = Vec::new();
    if columns.is_empty() {
        copied.extend(all.iter().map(|(a, _)| *a).filter(|a| !generated(*a)));
    } else {
        for name in columns {
            let Some((a, _)) = all.iter().find(|(_, n)| n == name) else { return Ok(()) };
            copied.push(*a);
        }
    }
    let columnar = matches!(rel, Relation::Table(_) | Relation::View(_));
    if columnar && !copied.is_empty() && copied.iter().all(|&a| column_granted(catalog, &role, oid, a, p)) {
        return Ok(());
    }
    Err(permission_denied(relation_kind(rel), rel.name()))
}

/// For INSERT / UPDATE / DELETE on a view with an INSTEAD OF trigger for it, or one that is
/// not automatically updatable: the view columns it inserts or updates, by name (Postgres' insertedCols / updatedCols on the view).
fn target_view_columns(stmt: &ir::Statement, catalog: &Catalog) -> Option<Vec<(Oid, i16, char)>> {
    let name = match stmt {
        ir::Statement::Insert(i) => &i.table,
        ir::Statement::Update(u) => &u.table,
        ir::Statement::Delete(d) => &d.table,
        _ => return None,
    };
    let Ok(Some(Relation::View(v))) = catalog.find_relation(name) else { return None };
    // The executor runs INSTEAD OF row triggers rather than writing the base table.
    let instead = catalog.table_triggers(v.oid).iter().any(|t| {
        t.enabled != 'D'
            && t.timing == pgmem_catalog::TriggerTiming::InsteadOf
            && match stmt {
                ir::Statement::Insert(_) => t.insert,
                ir::Statement::Update(_) => t.update.is_some(),
                _ => t.delete,
            }
    });
    if !instead && !matches!(crate::views::resolve(catalog, &v), Ok(Err(_))) {
        return None;
    }
    let columns = relation_columns(catalog, v.oid);
    let attnum = |n: &str| columns.iter().find(|(_, c)| c == n).map(|(a, _)| *a);
    let uses = match stmt {
        ir::Statement::Insert(i) => {
            let names: Vec<i16> = if i.columns.is_empty() {
                let n = match &i.source {
                    ir::InsertSource::Values(rows) => rows.first().map_or(0, Vec::len),
                    ir::InsertSource::Query(_) => columns.len(),
                    ir::InsertSource::DefaultValues => 0,
                };
                columns.iter().take(n).map(|(a, _)| *a).collect()
            } else {
                i.columns.iter().filter_map(|c| attnum(c)).collect()
            };
            names.into_iter().map(|a| (v.oid, a, 'a')).collect()
        }
        ir::Statement::Update(u) => u
            .assignments
            .iter()
            .flat_map(|a| a.columns.iter())
            .filter_map(|c| c.first().and_then(|n| attnum(n)))
            .map(|a| (v.oid, a, 'w'))
            .collect(),
        _ => Vec::new(),
    };
    Some(uses)
}

/// The privilege PUBLIC holds by default that REVOKE ... FROM PUBLIC can take away.
fn public_default_letter(object: &GrantObject) -> Option<char> {
    match object {
        GrantObject::Schema(_) => Some('U'),
        GrantObject::Function(_) => Some('X'),
        _ => None,
    }
}

/// Checks the current role may execute user function `def` (42501 `permission denied for
/// function f`, or `procedure p`).
pub(crate) fn check_execute(catalog: &Catalog, env: &SessionEnv, def: &pgmem_catalog::FunctionDef) -> SqlResult<()> {
    let role = current_role(env);
    // A function in a schema the role may not use was named qualified (the search path
    // skips such schemas): USAGE is checked first, as name lookup does.
    if catalog.schema(&def.schema).is_some() && !has_privilege(catalog, env, &role, &GrantObject::Schema(def.schema.clone()), 'U') {
        return Err(permission_denied("schema", &def.schema));
    }
    if has_privilege(catalog, env, &role, &GrantObject::Function(def.oid), 'X') {
        return Ok(());
    }
    Err(permission_denied(if def.procedure { "procedure" } else { "function" }, &def.name))
}

/// EXECUTE on every user function a bound statement calls, checked before it runs (as
/// Postgres checks when the executor starts, whether or not a row reaches the call).
fn check_function_use(binder: &crate::bind::Binder, catalog: &Catalog, env: &SessionEnv) -> SqlResult<()> {
    for oid in binder.function_use() {
        if let Some(def) = catalog.function_by_oid(oid) {
            check_execute(catalog, env, def)?;
        }
    }
    Ok(())
}

/// Validates `SET ROLE name` (22023 for an unknown role, 42501 without membership).
pub(crate) fn check_set_role(catalog: &Catalog, env: &SessionEnv, name: &str) -> SqlResult<()> {
    if name.eq_ignore_ascii_case("none") || name.eq_ignore_ascii_case("default") {
        return Ok(());
    }
    if !role_exists(catalog, env, name) {
        return Err(SqlError::new(code::INVALID_PARAMETER_VALUE, format!("role \"{name}\" does not exist")));
    }
    if is_superuser(catalog, env, &env.user) || catalog.roles_of(&env.user).iter().any(|r| r == name) {
        return Ok(());
    }
    Err(SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied to set role \"{name}\"")))
}
