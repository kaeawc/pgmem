//! Row-level security (`commands/policy.c`, `rewrite/rowsecurity.c`, `utils/misc/rls.c`):
//! CREATE / ALTER / DROP POLICY, `ALTER TABLE ... {ENABLE | DISABLE | FORCE | NO FORCE} ROW
//! LEVEL SECURITY`, whether policies apply to a role (`check_enable_rls`), which policies
//! apply to a command, and the WITH CHECK enforcement of new rows. The binder side (security
//! quals on scans and DML targets) is in `bind/rls.rs`.

use crate::bind::{Binder, Clause, Ctx, Level};
use crate::eval::{EvalCtx, eval_pred};
use crate::plan::{Expr, Row};
use crate::session::{Notice, QueryResult, SessionEnv};
use pgmem_catalog::{Catalog, Oid, PolicyDef, TableDef};
use pgmem_ir as ir;
use pgmem_storage::Txn;
use pgmem_types::{SqlError, SqlResult, code};
use std::sync::Arc;

/// `pg_class.oid` of `pg_policy` (`pg_description.classoid` of policy comments).
pub const POLICY_CLASS_OID: Oid = 3256;

/// Whose policies the statement being bound is subject to.
#[derive(Clone, Debug)]
pub struct RlsUser {
    /// The role checked (`current_user`, or a view's owner inside the view).
    pub role: String,
    /// The bootstrap superuser (the session user).
    pub bootstrap: String,
    /// The `row_security` setting.
    pub row_security: bool,
}

impl RlsUser {
    pub fn from_env(env: &SessionEnv) -> RlsUser {
        let row_security = env.settings.get("row_security").is_none_or(|v| crate::settings::parse_bool(v).unwrap_or(true));
        RlsUser { role: crate::roles::current_role(env), bootstrap: env.user.clone(), row_security }
    }
}

/// A WITH CHECK condition new rows must satisfy (Postgres' `WithCheckOption` of kind
/// `WCO_RLS_INSERT_CHECK` / `WCO_RLS_UPDATE_CHECK`), bound over the table's physical row.
#[derive(Clone, Debug, PartialEq)]
pub struct RlsCheck {
    /// The restrictive policy checked alone, named in the error; None for the permissive ones.
    pub policy: Option<String>,
    pub table: String,
    pub pred: Expr,
}

/// Fails like `ExecWithCheckOptions` when the new row does not satisfy every check (NULL
/// fails).
pub(crate) fn check_new_row(checks: &[RlsCheck], row: &Row, ctx: &EvalCtx) -> SqlResult<()> {
    for c in checks {
        if !eval_pred(&c.pred, row, ctx)? {
            let message = match &c.policy {
                Some(p) => format!("new row violates row-level security policy \"{p}\" for table \"{}\"", c.table),
                None => format!("new row violates row-level security policy for table \"{}\"", c.table),
            };
            return Err(SqlError::new(code::INSUFFICIENT_PRIVILEGE, message));
        }
    }
    Ok(())
}

fn is_superuser(catalog: &Catalog, bootstrap: &str, role: &str) -> bool {
    role == bootstrap || catalog.role(role).is_some_and(|r| r.superuser)
}

/// Whether `role` has the privileges of the owner of `oid` (`object_ownercheck`).
fn owns(catalog: &Catalog, bootstrap: &str, role: &str, oid: Oid) -> bool {
    let owner = catalog.owner(oid).unwrap_or(bootstrap);
    is_superuser(catalog, bootstrap, role) || catalog.roles_of(role).iter().any(|r| r == owner)
}

/// `check_enable_rls`: whether `table`'s policies apply to `user`. Superusers, BYPASSRLS
/// roles and (unless FORCE ROW LEVEL SECURITY) the owner are exempt. With `row_security`
/// off, a statement policies would apply to fails (unless `no_error`).
pub(crate) fn enabled_for(catalog: &Catalog, user: &RlsUser, table: &TableDef, no_error: bool) -> SqlResult<bool> {
    let (enabled, forced) = catalog.row_security(table.oid);
    if !enabled {
        return Ok(false);
    }
    let role = &user.role;
    if is_superuser(catalog, &user.bootstrap, role) || catalog.role(role).is_some_and(|r| r.bypassrls) {
        return Ok(false);
    }
    let owner = owns(catalog, &user.bootstrap, role, table.oid);
    if owner && !forced {
        return Ok(false);
    }
    if !user.row_security && !no_error {
        let e = SqlError::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("query would be affected by row-level security policy for table \"{}\"", table.name),
        );
        return Err(if owner {
            e.with_hint("To disable the policy for the table's owner, use ALTER TABLE NO FORCE ROW LEVEL SECURITY.")
        } else {
            e
        });
    }
    Ok(true)
}

/// The (permissive, restrictive) policies of `table` for command `cmd` that apply to
/// `role`, by name.
pub(crate) fn applicable(catalog: &Catalog, table: Oid, cmd: char, role: &str) -> (Vec<Arc<PolicyDef>>, Vec<Arc<PolicyDef>>) {
    let member_of = catalog.roles_of(role);
    catalog
        .table_policies(table)
        .into_iter()
        .filter(|p| p.applies_to(cmd) && (p.roles.is_empty() || p.roles.iter().any(|r| member_of.contains(r))))
        .partition(|p| p.permissive)
}

/// `polcmd` as `pg_policies.cmd` shows it.
pub fn command_name(cmd: char) -> &'static str {
    match cmd {
        'r' => "SELECT",
        'a' => "INSERT",
        'w' => "UPDATE",
        'd' => "DELETE",
        _ => "ALL",
    }
}

/// The OID of a role (the bootstrap superuser is 10).
pub fn role_oid(catalog: &Catalog, bootstrap: &str, role: &str) -> Oid {
    if role == bootstrap {
        return crate::catalog_views::SUPERUSER_OID;
    }
    catalog.role(role).map(|r| r.oid).or_else(|| crate::catalog_views::predefined_role(role)).unwrap_or(0)
}

fn tag(t: &str) -> QueryResult {
    QueryResult { tag: t.to_owned(), ..Default::default() }
}

fn not_owner(what: &str, name: &str) -> SqlError {
    SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("must be owner of {what} {name}"))
}

/// The table a policy statement names (42809 for other relations).
fn policy_table(catalog: &Catalog, name: &ir::ObjectName) -> SqlResult<Arc<TableDef>> {
    match catalog.find_relation(name)? {
        Some(pgmem_catalog::Relation::Table(t)) if t.matview.is_none() => Ok(t),
        Some(r) => Err(SqlError::new(code::WRONG_OBJECT_TYPE, format!("\"{}\" is not a table", r.name()))),
        None => Err(SqlError::undefined_table(&name.to_string())),
    }
}

fn missing_policy(name: &str, table: &str) -> SqlError {
    SqlError::new(code::UNDEFINED_OBJECT, format!("policy \"{name}\" for table \"{table}\" does not exist"))
}

fn duplicate_policy(name: &str, table: &str) -> SqlError {
    SqlError::new(code::DUPLICATE_OBJECT, format!("policy \"{name}\" for table \"{table}\" already exists"))
}

/// The role names a `TO` list stores (empty: PUBLIC), with the warning for roles listed
/// beside PUBLIC.
fn resolve_roles(catalog: &Catalog, env: &SessionEnv, roles: &[ir::PolicyRole], notices: &mut Vec<Notice>) -> SqlResult<Vec<String>> {
    let mut out = Vec::new();
    let mut public = false;
    for r in roles {
        let name = match r {
            ir::PolicyRole::Public => {
                public = true;
                continue;
            }
            ir::PolicyRole::CurrentUser => crate::roles::current_role(env),
            ir::PolicyRole::SessionUser => env.user.clone(),
            ir::PolicyRole::Name(n) => n.clone(),
        };
        if !crate::roles::role_exists(catalog, env, &name) {
            return Err(SqlError::new(code::UNDEFINED_OBJECT, format!("role \"{name}\" does not exist")));
        }
        out.push(name);
    }
    if public {
        if !out.is_empty() {
            notices.push(Notice { severity: "WARNING", code: "22023", message: "ignoring specified roles other than PUBLIC".into() });
        }
        return Ok(Vec::new());
    }
    Ok(out)
}

/// Parse-analyzes a policy expression on `table` (`transformWhereClause` with
/// `EXPR_KIND_POLICY`): it must bind over the table's columns and be boolean.
fn validate_expr(catalog: &Catalog, table: &TableDef, e: &ir::Expr) -> SqlResult<()> {
    let b = Binder::new(catalog, &[]);
    let level = Level::new(crate::bind::dml::physical_scope(table, &table.name));
    b.bind_predicate(e, "POLICY", &Ctx::new(&level).with_clause(Clause::Policy)).map(drop)
}

/// CREATE POLICY.
pub(crate) fn create_policy(p: &ir::CreatePolicy, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    if p.with_check.is_some() && matches!(p.command, 'r' | 'd') {
        return Err(SqlError::syntax("WITH CHECK cannot be applied to SELECT or DELETE"));
    }
    if p.using.is_some() && p.command == 'a' {
        return Err(SqlError::syntax("only WITH CHECK expression allowed for INSERT"));
    }
    let mut notices = Vec::new();
    let roles = resolve_roles(&catalog, env, &p.roles, &mut notices)?;
    let table = policy_table(&catalog, &p.table)?;
    if !owns(&catalog, &env.user, &crate::roles::current_role(env), table.oid) {
        return Err(not_owner("table", &table.name));
    }
    for e in p.using.iter().chain(p.with_check.iter()) {
        validate_expr(&catalog, &table, e)?;
    }
    if catalog.table_policies(table.oid).iter().any(|x| x.name == p.name) {
        return Err(duplicate_policy(&p.name, &table.name));
    }
    let cat = txn.catalog_mut();
    let oid = cat.alloc_oid();
    cat.put_policy(PolicyDef {
        oid,
        name: p.name.clone(),
        table: table.oid,
        command: p.command,
        permissive: p.permissive,
        roles,
        using: p.using.clone(),
        with_check: p.with_check.clone(),
        search_path: catalog.resolved_search_path(),
        relations: expr_relations(&catalog, p.using.iter().chain(p.with_check.iter())),
    });
    Ok(QueryResult { tag: "CREATE POLICY".into(), notices, ..Default::default() })
}

/// ALTER POLICY.
pub(crate) fn alter_policy(p: &ir::AlterPolicy, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let table = policy_table(&catalog, &p.table)?;
    if !owns(&catalog, &env.user, &crate::roles::current_role(env), table.oid) {
        return Err(not_owner("table", &table.name));
    }
    let policies = catalog.table_policies(table.oid);
    if let Some(new) = &p.rename_to
        && policies.iter().any(|x| &x.name == new)
    {
        return Err(duplicate_policy(new, &table.name));
    }
    let Some(existing) = policies.iter().find(|x| x.name == p.name) else {
        return Err(missing_policy(&p.name, &table.name));
    };
    let mut def = (**existing).clone();
    let mut notices = Vec::new();
    if let Some(new) = &p.rename_to {
        def.name.clone_from(new);
    } else {
        if p.using.is_some() && def.command == 'a' {
            return Err(SqlError::syntax("only WITH CHECK expression allowed for INSERT"));
        }
        if p.with_check.is_some() && matches!(def.command, 'r' | 'd') {
            return Err(SqlError::syntax("only USING expression allowed for SELECT, DELETE"));
        }
        if let Some(roles) = &p.roles {
            def.roles = resolve_roles(&catalog, env, roles, &mut notices)?;
        }
        for e in p.using.iter().chain(p.with_check.iter()) {
            validate_expr(&catalog, &table, e)?;
        }
        if p.using.is_some() {
            def.using.clone_from(&p.using);
        }
        if p.with_check.is_some() {
            def.with_check.clone_from(&p.with_check);
        }
        if p.using.is_some() || p.with_check.is_some() {
            // Both expressions are re-analyzed under the current search_path, as the new
            // one is; the relations the old one reads keep their pins.
            let mut relations = expr_relations(&catalog, p.using.iter().chain(p.with_check.iter()));
            for (name, oid) in &def.relations {
                if !relations.iter().any(|(n, _)| n == name) {
                    relations.push((name.clone(), *oid));
                }
            }
            def.relations = relations;
            def.search_path = catalog.resolved_search_path();
        }
    }
    txn.catalog_mut().put_policy(def);
    Ok(QueryResult { tag: "ALTER POLICY".into(), notices, ..Default::default() })
}

/// DROP POLICY [IF EXISTS].
pub(crate) fn drop_policy(name: &str, table: &ir::ObjectName, if_exists: bool, txn: &mut Txn, env: &SessionEnv) -> SqlResult<QueryResult> {
    let catalog = txn.catalog().clone();
    let skip = |message: String| QueryResult {
        tag: "DROP POLICY".into(),
        notices: vec![Notice { severity: "NOTICE", code: "00000", message }],
        ..Default::default()
    };
    let t = match policy_table(&catalog, table) {
        Ok(t) => t,
        Err(e) if if_exists && e.code == code::UNDEFINED_TABLE => {
            return Ok(skip(format!("relation \"{}\" does not exist, skipping", table.base())));
        }
        Err(e) => return Err(e),
    };
    let Some(p) = catalog.table_policies(t.oid).into_iter().find(|x| x.name == name) else {
        if if_exists {
            return Ok(skip(format!("policy \"{name}\" for relation \"{}\" does not exist, skipping", t.name)));
        }
        return Err(missing_policy(name, &t.name));
    };
    if !owns(&catalog, &env.user, &crate::roles::current_role(env), t.oid) {
        return Err(not_owner("relation", &t.name));
    }
    txn.catalog_mut().drop_policy(p.oid);
    Ok(tag("DROP POLICY"))
}

/// `ALTER TABLE ... {ENABLE | DISABLE | [NO] FORCE} ROW LEVEL SECURITY`.
pub(crate) fn alter_row_security(txn: &mut Txn, env: &SessionEnv, table: &TableDef, action: &ir::AlterTableAction) -> SqlResult<bool> {
    let (enabled, forced) = match action {
        ir::AlterTableAction::RowSecurity(on) => (Some(*on), None),
        ir::AlterTableAction::ForceRowSecurity(on) => (None, Some(*on)),
        _ => return Ok(false),
    };
    if !owns(txn.catalog(), &env.user, &crate::roles::current_role(env), table.oid) {
        return Err(not_owner("table", &table.name));
    }
    txn.catalog_mut().set_row_security(table.oid, enabled, forced);
    Ok(true)
}

/// DROP ROLE's DETAIL lines for the policies naming `role`.
pub(crate) fn role_dependencies(catalog: &Catalog, role: &str) -> Vec<String> {
    catalog
        .policies()
        .filter(|p| p.roles.iter().any(|r| r == role))
        .filter_map(|p| catalog.table_by_oid(p.table).map(|t| format!("target of policy {} on table {}", p.name, t.name)))
        .collect()
}

/// DROP OWNED BY: `role` leaves every policy's role list; a policy left with no role goes.
pub(crate) fn drop_owned(catalog: &mut Catalog, role: &str) {
    let named: Vec<Arc<PolicyDef>> = catalog.policies().filter(|p| p.roles.iter().any(|r| r == role)).cloned().collect();
    for p in named {
        let mut def = (*p).clone();
        def.roles.retain(|r| r != role);
        if def.roles.is_empty() {
            catalog.drop_policy(def.oid);
        } else {
            catalog.put_policy(def);
        }
    }
}

/// The user relations the subqueries of `exprs` read, resolved with the current search_path.
fn expr_relations<'e>(catalog: &Catalog, exprs: impl Iterator<Item = &'e ir::Expr>) -> Vec<(ir::ObjectName, Oid)> {
    let projection = exprs.map(|e| ir::SelectItem::Expr { expr: e.clone(), alias: None }).collect();
    let q = ir::Query::from_body(ir::SetExpr::Select(Box::new(ir::Select { projection, ..Default::default() })));
    crate::views::referenced_relations(&q)
        .into_iter()
        .filter_map(|n| catalog.find_relation(&n).ok().flatten().map(|r| (n, r.oid())))
        .collect()
}

/// Keeps a renamed table's policies working: qualified references to the table by its old
/// name follow it (relations read by subqueries are pinned by OID).
pub(crate) fn rename_table(catalog: &mut Catalog, oid: Oid, old: &str, new: &str) {
    for p in catalog.table_policies(oid) {
        let mut def = (*p).clone();
        for e in def.using.iter_mut().chain(def.with_check.iter_mut()) {
            e.walk_mut(&mut |x| {
                if let ir::Expr::Column(parts) = x
                    && parts.len() >= 2
                    && parts[parts.len() - 2] == old
                {
                    let i = parts.len() - 2;
                    new.clone_into(&mut parts[i]);
                }
            });
        }
        catalog.put_policy(def);
    }
}

/// DROP of relation `oid` (named `what` `name`): the policies reading it block the drop, or
/// go with it under CASCADE.
pub(crate) fn drop_dependents(txn: &mut Txn, oid: Oid, what: &str, name: &str, cascade: bool, notices: &mut Vec<Notice>) -> SqlResult<()> {
    let policies = txn.catalog().policies_reading(oid);
    let describe =
        |p: &PolicyDef, cat: &Catalog| format!("policy {} on table {}", p.name, cat.table_by_oid(p.table).map_or("?", |t| t.name.as_str()));
    if policies.is_empty() {
        return Ok(());
    }
    if !cascade {
        let lines: Vec<String> = policies.iter().map(|p| format!("{} depends on {what} {name}", describe(p, txn.catalog()))).collect();
        return Err(SqlError::new(
            code::DEPENDENT_OBJECTS_STILL_EXIST,
            format!("cannot drop {what} {name} because other objects depend on it"),
        )
        .with_detail(lines.join("\n"))
        .with_hint("Use DROP ... CASCADE to drop the dependent objects too."));
    }
    for p in policies {
        let message = format!("drop cascades to {}", describe(&p, txn.catalog()));
        notices.push(Notice { severity: "NOTICE", code: "00000", message });
        txn.catalog_mut().drop_policy(p.oid);
    }
    Ok(())
}

/// Keeps policy expressions pointing at a renamed column (Postgres stores attnums).
pub(crate) fn rename_column(catalog: &mut Catalog, table: &TableDef, old: &str, new: &str) {
    for p in catalog.table_policies(table.oid) {
        let mut def = (*p).clone();
        for e in def.using.iter_mut().chain(def.with_check.iter_mut()) {
            crate::ddl::rename_in_expr(e, &table.name, old, new);
        }
        catalog.put_policy(def);
    }
}

/// The policies reading `column` of `table` (ALTER TABLE DROP COLUMN's dependents).
pub(crate) fn column_dependents(catalog: &Catalog, table: &TableDef, column: &str) -> Vec<Arc<PolicyDef>> {
    let mut out: Vec<Arc<PolicyDef>> = catalog
        .policies()
        .filter(|p| p.table == table.oid)
        .filter(|p| p.using.iter().chain(p.with_check.iter()).any(|e| e.referenced_columns().iter().any(|c| c == column)))
        .cloned()
        .collect();
    out.sort_by_key(|p| p.oid);
    out
}

/// `row_security_active(table)`: whether policies apply to the current role.
pub(crate) fn row_security_active(ctx: &EvalCtx, table: Oid) -> SqlResult<bool> {
    let catalog = ctx.txn.catalog();
    let Some(t) = catalog.table_by_oid(table) else {
        return Err(SqlError::new(code::UNDEFINED_TABLE, format!("relation with OID {table} does not exist")));
    };
    enabled_for(catalog, &RlsUser::from_env(ctx.env), t, true)
}

/// COPY FROM into a table whose policies apply to the current role is refused.
pub(crate) fn check_copy_from(catalog: &Catalog, env: &SessionEnv, table: &TableDef) -> SqlResult<()> {
    if enabled_for(catalog, &RlsUser::from_env(env), table, false)? {
        return Err(
            SqlError::feature_not_supported("COPY FROM not supported with row-level security").with_hint("Use INSERT statements instead.")
        );
    }
    Ok(())
}
