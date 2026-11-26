//! Roles and privileges: CREATE / ALTER / DROP ROLE, GRANT / REVOKE (objects and roles),
//! default privileges, ownership, DROP / REASSIGN OWNED, privilege checks (`aclchk.c`), and
//! the `has_*_privilege` functions. The session user is a superuser; so are roles created
//! SUPERUSER.

use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, DefaultPrivilege, GrantObject, Oid, Relation, RoleDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

/// The role privileges are checked for: `SET ROLE`'s, else the session user.
pub fn current_role(env: &SessionEnv) -> String {
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
    let letters = |privs: &[char], grantable: &[char]| -> String {
        ACL_ORDER
            .chars()
            .filter(|c| privs.contains(c))
            .map(|c| if grantable.contains(&c) { format!("{c}*") } else { c.to_string() })
            .collect()
    };
    let fmt =
        |g: &pgmem_catalog::Grant| format!("{}={}/{}", acl_name(&g.grantee), letters(&g.privileges, &g.grantable), acl_name(&g.grantor));
    let mut items = Vec::new();
    // An explicit owner entry (from default privileges) keeps its position; otherwise the
    // owner's implicit entry leads.
    if !grants.iter().any(|g| g.grantee == owner) {
        items.push(format!("{}={}/{}", acl_name(&owner), object_privileges(object, sequence), acl_name(&owner)));
    }
    items.extend(grants.iter().map(|g| fmt(g)));
    Some(items)
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
        GrantObject::Relation(o) | GrantObject::Function(o) | GrantObject::Type(o) => Some(owner_of(catalog, env, *o)),
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
        GrantObject::Relation(_) => false,
    };
    let revoked_public = catalog.grants().iter().any(|g| g.object == *object && g.grantee == "-revoked:public");
    if public_default && !revoked_public {
        return true;
    }
    catalog
        .grants()
        .iter()
        .any(|g| g.object == *object && (g.grantee.is_empty() || roles.contains(&g.grantee)) && g.privileges.contains(&p))
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
    for (object, sequence) in target_objects(&catalog, &g.target)? {
        let privs = letters(g.privileges.as_deref(), &object, sequence)?;
        let cat = txn.catalog_mut();
        for grantee in &grantees {
            if g.revoke {
                cat.revoke(&object, grantee, &privs, g.grant_option);
                if grantee.is_empty() && matches!(object, GrantObject::Schema(_)) && privs.contains(&'U') {
                    // Revoking PUBLIC's default USAGE on a schema.
                    cat.grant(object.clone(), "-revoked:public", &me, &['U'], false);
                }
            } else {
                if grantee.is_empty() && matches!(object, GrantObject::Schema(_)) {
                    cat.revoke(&object, "-revoked:public", &['U'], false);
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
    Ok(tag(what))
}

// ---------------------------------------------------------------------------------------------
// Enforcement
// ---------------------------------------------------------------------------------------------

fn permission_denied(kind: &str, name: &str) -> SqlError {
    SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied for {kind} {name}"))
}

/// Checks the current role may run `stmt`: SELECT on every relation it reads, the DML
/// privilege on its target. Superusers skip the checks.
pub(crate) fn check_statement(stmt: &ir::Statement, catalog: &Catalog, env: &SessionEnv) -> SqlResult<()> {
    let role = current_role(env);
    if is_superuser(catalog, env, &role) {
        return Ok(());
    }
    let check = |name: &ir::ObjectName, p: char| -> SqlResult<()> {
        let Ok(Some(rel)) = catalog.find_relation(name) else { return Ok(()) };
        let (kind, oid) = match &rel {
            Relation::Table(t) if t.matview.is_some() => ("materialized view", t.oid),
            Relation::Table(t) => ("table", t.oid),
            Relation::View(v) => ("view", v.oid),
            Relation::Sequence(s) => ("sequence", s.oid),
            Relation::Index { .. } => return Ok(()),
        };
        if !has_privilege(catalog, env, &role, &GrantObject::Relation(oid), p) {
            return Err(permission_denied(kind, rel.name()));
        }
        Ok(())
    };
    let reads = |q: &ir::Query| -> SqlResult<()> {
        for n in crate::views::referenced_relations(q) {
            check(&n, 'r')?;
        }
        Ok(())
    };
    match stmt {
        ir::Statement::Query(q) => reads(q),
        ir::Statement::Insert(i) => {
            check(&i.table, 'a')?;
            if let ir::InsertSource::Query(q) = &i.source {
                reads(q)?;
            }
            Ok(())
        }
        ir::Statement::Update(u) => check(&u.table, 'w'),
        ir::Statement::Delete(d) => check(&d.table, 'd'),
        ir::Statement::Truncate(t) => {
            for n in &t.tables {
                check(n, 'D')?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
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
