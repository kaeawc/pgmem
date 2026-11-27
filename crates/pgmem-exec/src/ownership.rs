//! Ownership checks for DDL (`pg_*_ownercheck` / `aclcheck_error(ACLCHECK_NOT_OWNER)`):
//! ALTER, DROP, COMMENT ON and CREATE OR REPLACE of an object need the current role to be its
//! owner or a member of the owning role; superusers skip the checks. Also the privilege
//! checks DDL makes on other objects (TRIGGER and EXECUTE for CREATE TRIGGER, MAINTAIN for
//! REFRESH MATERIALIZED VIEW, CREATE on the schema and membership in the new owner for
//! OWNER TO) and USAGE on the schema a qualified name names.

use crate::roles::{current_role, has_privilege, is_superuser, owner_of};
use crate::session::SessionEnv;
use pgmem_catalog::{Catalog, GrantObject, Oid, Relation};
use pgmem_ir::{self as ir, Statement};
use pgmem_types::{SqlError, SqlResult, code};

fn not_owner(kind: &str, name: &str) -> SqlError {
    SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("must be owner of {kind} {name}"))
}

fn denied(kind: &str, name: &str) -> SqlError {
    SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied for {kind} {name}"))
}

/// How an ownership error names a relation: its own kind, or `relation` (COMMENT ON
/// COLUMN / CONSTRAINT / TRIGGER, DROP TRIGGER).
#[derive(Clone, Copy, PartialEq)]
enum Label {
    Kind,
    Relation,
}

/// `table`, `view`, `materialized view`, `sequence` or `index`.
fn relation_kind(rel: &Relation) -> &'static str {
    match rel {
        Relation::Table(t) if t.matview.is_some() => "materialized view",
        other => other.kind_name(),
    }
}

/// The checks for one statement, run as `role`.
struct Checker<'a> {
    catalog: &'a Catalog,
    env: &'a SessionEnv,
    role: String,
}

impl Checker<'_> {
    fn owns(&self, owner: &str) -> bool {
        self.catalog.roles_of(&self.role).iter().any(|r| r == owner)
    }

    fn owner(&self, oid: Oid) -> String {
        owner_of(self.catalog, self.env, oid)
    }

    fn schema_owner(&self, schema: &str) -> Option<String> {
        self.catalog.schema(schema).map(|s| s.owner.clone())
    }

    /// USAGE on the schema a qualified name names (`LookupExplicitNamespace`).
    fn usage(&self, name: &ir::ObjectName) -> SqlResult<()> {
        if let Some(s) = name.schema() {
            self.schema_usage(s)?;
        }
        Ok(())
    }

    fn schema_usage(&self, schema: &str) -> SqlResult<()> {
        if self.catalog.schema(schema).is_some()
            && !has_privilege(self.catalog, self.env, &self.role, &GrantObject::Schema(schema.to_owned()), 'U')
        {
            return Err(denied("schema", schema));
        }
        Ok(())
    }

    /// CREATE on `schema` (`pg_namespace_aclcheck(ACL_CREATE)`); temporary schemas need none.
    fn schema_create(&self, schema: &str) -> SqlResult<()> {
        if self.catalog.schema(schema).is_some()
            && !schema.starts_with("pg_temp")
            && !has_privilege(self.catalog, self.env, &self.role, &GrantObject::Schema(schema.to_owned()), 'C')
        {
            return Err(denied("schema", schema));
        }
        Ok(())
    }

    /// Creating an object named `name`: USAGE on a qualified schema, then CREATE on the
    /// schema it lands in (`RangeVarGetAndCheckCreationNamespace`). A name that resolves to
    /// no schema passes: the statement reports it.
    fn create_in(&self, name: &ir::ObjectName) -> SqlResult<()> {
        self.usage(name)?;
        match self.catalog.creation_schema(name) {
            Ok(schema) => self.schema_create(&schema),
            Err(_) => Ok(()),
        }
    }

    /// CREATE on the database (creating or renaming a schema).
    fn database_create(&self) -> SqlResult<()> {
        if has_privilege(self.catalog, self.env, &self.role, &GrantObject::Database, 'C') {
            return Ok(());
        }
        Err(denied("database", &self.env.database))
    }

    /// The relation `name` names, if it exists (a missing one is reported by the statement).
    fn relation(&self, name: &ir::ObjectName) -> SqlResult<Option<Relation>> {
        self.usage(name)?;
        Ok(self.catalog.find_relation(name).ok().flatten())
    }

    /// The owner of a relation: an index's is its table's.
    fn relation_owner(&self, rel: &Relation) -> String {
        match rel {
            Relation::Index { table, .. } => self.catalog.table_q(table).map_or_else(|| self.env.user.clone(), |t| self.owner(t.oid)),
            other => self.owner(other.oid()),
        }
    }

    fn require_relation(&self, rel: &Relation, label: Label) -> SqlResult<()> {
        if self.owns(&self.relation_owner(rel)) {
            return Ok(());
        }
        Err(not_owner(if label == Label::Relation { "relation" } else { relation_kind(rel) }, rel.name()))
    }

    /// Ownership of the relation `name` names, when it exists and `matches` its kind.
    fn relation_named(&self, name: &ir::ObjectName, label: Label, matches: impl Fn(&Relation) -> bool) -> SqlResult<Option<Relation>> {
        let Some(rel) = self.relation(name)? else { return Ok(None) };
        if matches(&rel) {
            self.require_relation(&rel, label)?;
        }
        Ok(Some(rel))
    }

    fn require_type(&self, name: &ir::ObjectName) -> SqlResult<Option<Oid>> {
        self.usage(name)?;
        let Ok(Some(t)) = self.catalog.lookup_type(name) else { return Ok(None) };
        if !self.owns(&self.owner(t.oid)) {
            return Err(not_owner("type", &t.name));
        }
        Ok(Some(t.oid))
    }

    fn require_schema(&self, schema: &str) -> SqlResult<()> {
        match self.schema_owner(schema) {
            Some(o) if !self.owns(&o) => Err(not_owner("schema", schema)),
            _ => Ok(()),
        }
    }

    fn require_function(&self, f: &pgmem_catalog::FunctionDef, word: &str) -> SqlResult<()> {
        if self.owns(&self.owner(f.oid)) {
            return Ok(());
        }
        Err(not_owner(word, &f.name))
    }

    /// The routine `name [(types)]` names, if exactly one does.
    fn routine(
        &self,
        name: &ir::ObjectName,
        types: Option<&[ir::TypeName]>,
    ) -> SqlResult<Option<std::sync::Arc<pgmem_catalog::FunctionDef>>> {
        self.usage(name)?;
        Ok(crate::functions::find_routine(self.catalog, name, types).ok())
    }

    /// OWNER TO `new` of an object owned by `owner` in `schema` (`AlterObjectOwner`): nothing
    /// to check when the owner does not change; else ownership, membership in the new owner
    /// and CREATE on the schema.
    fn owner_to(&self, owner: &str, new: &str, schema: Option<&str>, not_owned: SqlError) -> SqlResult<()> {
        let new = match new {
            "current_user" | "current_role" => self.role.clone(),
            "session_user" => self.env.user.clone(),
            other => other.to_owned(),
        };
        if !crate::roles::role_exists(self.catalog, self.env, &new) || new == owner {
            return Ok(());
        }
        if !self.owns(owner) {
            return Err(not_owned);
        }
        if !self.owns(&new) {
            return Err(SqlError::new(code::INSUFFICIENT_PRIVILEGE, format!("must be able to SET ROLE \"{new}\"")));
        }
        // The new owner needs CREATE on the object's schema.
        if let Some(s) = schema
            && !has_privilege(self.catalog, self.env, &new, &GrantObject::Schema(s.to_owned()), 'C')
        {
            return Err(denied("schema", s));
        }
        Ok(())
    }
}

/// Checks the current role may run DDL statement `stmt` (a no-op for other statements and
/// for superusers). Objects that do not exist pass: the statement reports them.
pub(crate) fn check_ddl(stmt: &Statement, catalog: &Catalog, env: &SessionEnv) -> SqlResult<()> {
    let role = current_role(env);
    if is_superuser(catalog, env, &role) {
        return Ok(());
    }
    let ck = Checker { catalog, env, role };
    match stmt {
        Statement::Query(_) | Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) | Statement::Truncate(_) => {
            check_read_usage(&ck, stmt)
        }
        Statement::Drop(d) => check_drop(&ck, d),
        Statement::DropFunction { items, if_exists, kind, .. } => {
            for (name, types) in items {
                let Some(f) = ck.routine(name, types.as_deref())? else {
                    // A missing routine without IF EXISTS fails the statement before later names.
                    if *if_exists {
                        continue;
                    }
                    return Ok(());
                };
                let word = routine_word(*kind);
                if *kind == ir::RoutineKind::Routine || (*kind == ir::RoutineKind::Procedure) == f.procedure {
                    ck.require_function(&f, word)?;
                }
            }
            Ok(())
        }
        Statement::AlterFunction { name, types, kind, actions } => check_alter_function(&ck, name, types.as_deref(), *kind, actions),
        Statement::CreateFunction(cf) => ck.create_in(&cf.name),
        Statement::CreateSequence(cs) => ck.create_in(&cs.name),
        Statement::CreateType(ct) => ck.create_in(&ct.name),
        Statement::CreateDomain(cd) => ck.create_in(&cd.name),
        Statement::CreateSchema(_) => ck.database_create(),
        Statement::CreateView(cv) => {
            ck.create_in(&cv.name)?;
            if cv.or_replace {
                ck.relation_named(&cv.name, Label::Kind, |r| matches!(r, Relation::View(_)))?;
            }
            Ok(())
        }
        Statement::CreateIndex(ci) => {
            if let Some(rel) = ck.relation_named(&ci.table, Label::Kind, |_| true)? {
                ck.schema_create(rel.schema())?;
            }
            Ok(())
        }
        Statement::CreateTable(ct) => {
            if !ct.temporary {
                ck.create_in(&ct.name)?;
            }
            if let Some((parent, _)) = &ct.options.partition_of {
                ck.relation_named(parent, Label::Kind, |_| true)?;
            }
            Ok(())
        }
        Statement::AlterTable(at) => check_alter_table(&ck, at),
        Statement::AlterSequence { name, rename_to, .. } => {
            let rel = ck.relation_named(name, Label::Kind, |r| matches!(r, Relation::Sequence(_)))?;
            rename_create(&ck, rel.as_ref(), rename_to.is_some())
        }
        Statement::RenameView { name, .. } => {
            let rel = ck.relation_named(name, Label::Kind, |r| !matches!(r, Relation::Index { .. }))?;
            rename_create(&ck, rel.as_ref(), true)
        }
        Statement::RenameIndex { name, .. } => {
            let rel = ck.relation_named(name, Label::Kind, |r| matches!(r, Relation::Index { .. }))?;
            rename_create(&ck, rel.as_ref(), true)
        }
        Statement::RefreshMatview { name, .. } => {
            if let Some(rel) = ck.relation(name)?
                && !has_privilege(catalog, env, &ck.role, &GrantObject::Relation(rel.oid()), 'm')
            {
                return Err(denied(relation_kind(&rel), rel.name()));
            }
            Ok(())
        }
        Statement::AlterType(at) => ck.require_type(&at.name).map(drop),
        Statement::AlterDomain(ad) => ck.require_type(&ad.name).map(drop),
        Statement::AlterSchema { name, rename_to, .. } => {
            ck.require_schema(name)?;
            if rename_to.is_some() && catalog.schema(name).is_some() {
                ck.database_create()?;
            }
            Ok(())
        }
        Statement::SetSchema { object_type, name, schema, .. } => check_set_schema(&ck, *object_type, name, schema),
        Statement::Comment { target, name, .. } => check_comment(&ck, target, name),
        Statement::AlterOwner { object, name, owner } => check_alter_owner(&ck, *object, name, owner),
        Statement::DropTrigger { table, .. } => {
            ck.relation_named(table, Label::Relation, |_| true)?;
            Ok(())
        }
        Statement::RenameTrigger { table, .. } => {
            ck.relation_named(table, Label::Kind, |_| true)?;
            Ok(())
        }
        Statement::CreateTrigger(ct) => check_create_trigger(&ck, ct),
        Statement::DropExtension { names, .. } => {
            for n in names {
                if let Some(e) = catalog.extension(n)
                    && !ck.owns(&ck.owner(e.oid))
                {
                    return Err(not_owner("extension", n));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// RENAME of a relation needs CREATE on its schema, after ownership.
fn rename_create(ck: &Checker, rel: Option<&Relation>, renames: bool) -> SqlResult<()> {
    match rel {
        Some(rel) if renames => ck.schema_create(rel.schema()),
        _ => Ok(()),
    }
}

fn routine_word(kind: ir::RoutineKind) -> &'static str {
    match kind {
        ir::RoutineKind::Procedure => "procedure",
        ir::RoutineKind::Routine => "routine",
        ir::RoutineKind::Function => "function",
    }
}

/// USAGE on the schemas of the qualified relation names a query or DML statement reads or
/// writes (checked at parse analysis, before any table privilege).
fn check_read_usage(ck: &Checker, stmt: &Statement) -> SqlResult<()> {
    let mut names: Vec<ir::ObjectName> = Vec::new();
    match stmt {
        Statement::Query(q) => names = crate::views::referenced_relations(q),
        Statement::Insert(i) => {
            names.push(i.table.clone());
            if let ir::InsertSource::Query(q) = &i.source {
                names.extend(crate::views::referenced_relations(q));
            }
        }
        Statement::Update(u) => names.push(u.table.clone()),
        Statement::Delete(d) => names.push(d.table.clone()),
        Statement::Truncate(t) => names.extend(t.tables.iter().cloned()),
        _ => {}
    }
    names.iter().try_for_each(|n| ck.usage(n))
}

fn check_drop(ck: &Checker, d: &ir::Drop) -> SqlResult<()> {
    use ir::ObjectType as O;
    for name in &d.names {
        let found = match d.object_type {
            O::Table | O::View | O::MaterializedView | O::Index | O::Sequence => {
                let want = d.object_type;
                ck.relation_named(name, Label::Kind, |r| match r {
                    Relation::Table(t) => (want == O::Table) == t.matview.is_none() && matches!(want, O::Table | O::MaterializedView),
                    Relation::View(_) => want == O::View,
                    Relation::Sequence(_) => want == O::Sequence,
                    Relation::Index { .. } => want == O::Index,
                })?
                .is_some()
            }
            O::Type | O::Domain => ck.require_type(name)?.is_some(),
            O::Schema => {
                ck.require_schema(name.base())?;
                ck.catalog.schema(name.base()).is_some()
            }
            O::Extension => {
                if let Some(e) = ck.catalog.extension(name.base())
                    && !ck.owns(&ck.owner(e.oid))
                {
                    return Err(not_owner("extension", name.base()));
                }
                true
            }
            O::Function => ck.routine(name, None)?.map(|f| ck.require_function(&f, "function")).transpose()?.is_some(),
        };
        // A missing object without IF EXISTS fails the statement before later names.
        if !found && !d.if_exists {
            return Ok(());
        }
    }
    Ok(())
}

fn check_alter_function(
    ck: &Checker,
    name: &ir::ObjectName,
    types: Option<&[ir::TypeName]>,
    kind: ir::RoutineKind,
    actions: &[ir::AlterFunctionAction],
) -> SqlResult<()> {
    use ir::AlterFunctionAction as A;
    let Some(f) = ck.routine(name, types)? else { return Ok(()) };
    // RENAME / SET SCHEMA / OWNER TO / DEPENDS ON EXTENSION go through the generic object
    // paths, which always say `function`; the other attributes name the statement's kind.
    let generic = actions.iter().any(|a| matches!(a, A::RenameTo(_) | A::SetSchema(_) | A::OwnerTo(_) | A::DependsOnExtension { .. }));
    let word = if generic { "function" } else { routine_word(kind) };
    for a in actions {
        if let A::OwnerTo(new) = a {
            return ck.owner_to(&ck.owner(f.oid), new, Some(&f.schema), not_owner(word, &f.name));
        }
    }
    ck.require_function(&f, word)?;
    for a in actions {
        match a {
            A::RenameTo(_) => ck.schema_create(&f.schema)?,
            A::SetSchema(target) => ck.schema_create(target)?,
            _ => {}
        }
    }
    Ok(())
}

/// CREATE OR REPLACE of an existing function (called by CREATE FUNCTION once it has found
/// the function it replaces): its owner only, `must be owner of function f`.
pub(crate) fn check_replace_function(catalog: &Catalog, env: &SessionEnv, old: &pgmem_catalog::FunctionDef) -> SqlResult<()> {
    let role = current_role(env);
    if is_superuser(catalog, env, &role) {
        return Ok(());
    }
    Checker { catalog, env, role }.require_function(old, "function")
}

fn check_alter_table(ck: &Checker, at: &ir::AlterTable) -> SqlResult<()> {
    let Some(rel) = ck.relation(&at.name)? else { return Ok(()) };
    for a in &at.actions {
        if let ir::AlterTableAction::OwnerTo(new) = a {
            return ck.owner_to(&ck.relation_owner(&rel), new, Some(rel.schema()), not_owner(relation_kind(&rel), rel.name()));
        }
    }
    ck.require_relation(&rel, Label::Kind)?;
    if at.actions.iter().any(|a| matches!(a, ir::AlterTableAction::RenameTable { .. })) {
        ck.schema_create(rel.schema())?;
    }
    Ok(())
}

/// ALTER ... SET SCHEMA: ownership, then CREATE on the target schema.
fn check_set_schema(ck: &Checker, object_type: ir::ObjectType, name: &ir::ObjectName, target: &str) -> SqlResult<()> {
    use ir::ObjectType as O;
    let found = match object_type {
        O::Type | O::Domain => ck.require_type(name)?.is_some(),
        O::Function => match ck.routine(name, None)? {
            Some(f) => ck.require_function(&f, "function").map(|()| true)?,
            None => false,
        },
        O::Schema | O::Extension => false,
        O::Table | O::View | O::MaterializedView | O::Index | O::Sequence => ck.relation_named(name, Label::Kind, |_| true)?.is_some(),
    };
    if found { ck.schema_create(target) } else { Ok(()) }
}

fn check_comment(ck: &Checker, target: &ir::CommentTarget, name: &ir::ObjectName) -> SqlResult<()> {
    use ir::CommentTarget as T;
    match target {
        T::Table | T::View | T::MaterializedView | T::Sequence | T::Index => ck.relation_named(name, Label::Kind, |_| true).map(drop),
        T::Column => {
            let table = ir::ObjectName(name.0[..name.0.len().saturating_sub(1)].to_vec());
            ck.relation_named(&table, Label::Relation, |_| true).map(drop)
        }
        T::Constraint(table) | T::Trigger(table) | T::Policy(table) => ck.relation_named(table, Label::Relation, |_| true).map(drop),
        T::Schema => ck.require_schema(name.base()),
        T::Type | T::Domain => ck.require_type(name).map(drop),
        T::Function(types) => match ck.routine(name, types.as_deref())? {
            Some(f) => ck.require_function(&f, if f.procedure { "procedure" } else { "function" }),
            None => Ok(()),
        },
        // Statistics objects and casts have no owner check here yet.
        T::Statistics | T::Cast(..) | T::Other => Ok(()),
    }
}

fn check_alter_owner(ck: &Checker, object: ir::OwnedObject, name: &ir::ObjectName, new: &str) -> SqlResult<()> {
    use ir::OwnedObject as O;
    match object {
        O::Schema => match ck.schema_owner(name.base()) {
            Some(owner) => ck.owner_to(&owner, new, None, not_owner("schema", name.base())),
            None => Ok(()),
        },
        O::Sequence | O::View | O::MaterializedView => match ck.relation(name)? {
            Some(rel) => ck.owner_to(&ck.relation_owner(&rel), new, Some(rel.schema()), not_owner(relation_kind(&rel), rel.name())),
            None => Ok(()),
        },
        O::Function | O::Procedure => match ck.routine(name, None)? {
            Some(f) => ck.owner_to(&ck.owner(f.oid), new, Some(&f.schema), not_owner("function", &f.name)),
            None => Ok(()),
        },
        O::Type | O::Domain => {
            ck.usage(name)?;
            match ck.catalog.lookup_type(name).ok().flatten() {
                Some(t) => ck.owner_to(&ck.owner(t.oid), new, Some(&t.schema), not_owner("type", &t.name)),
                None => Ok(()),
            }
        }
        O::Database => Ok(()),
    }
}

/// CREATE TRIGGER: TRIGGER on the table, then EXECUTE on the trigger function.
fn check_create_trigger(ck: &Checker, ct: &ir::CreateTrigger) -> SqlResult<()> {
    let Some(rel) = ck.relation(&ct.table)? else { return Ok(()) };
    if !has_privilege(ck.catalog, ck.env, &ck.role, &GrantObject::Relation(rel.oid()), 't') {
        return Err(denied(relation_kind(&rel), rel.name()));
    }
    ck.usage(&ct.function)?;
    let func = ck.catalog.find_functions(&ct.function).into_iter().find(|f| f.input_types().is_empty());
    if let Some(f) = func {
        crate::roles::check_execute(ck.catalog, ck.env, &f)?;
    }
    Ok(())
}
