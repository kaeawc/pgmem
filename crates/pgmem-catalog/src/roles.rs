//! Roles (`pg_authid`), memberships (`pg_auth_members`), privileges (the `*acl` columns) and
//! object owners. The session user is the bootstrap superuser and owns objects with no
//! recorded owner.

use crate::{Catalog, Oid};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct RoleDef {
    pub oid: Oid,
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub createrole: bool,
    pub createdb: bool,
    pub login: bool,
    pub replication: bool,
    pub bypassrls: bool,
    pub connlimit: i32,
    pub password: Option<String>,
    pub valid_until: Option<String>,
    /// `ALTER ROLE ... SET name = value` entries (`name=value`).
    pub config: Vec<String>,
}

impl RoleDef {
    pub fn new(oid: Oid, name: &str) -> RoleDef {
        RoleDef {
            oid,
            name: name.to_owned(),
            superuser: false,
            inherit: true,
            createrole: false,
            createdb: false,
            login: false,
            replication: false,
            bypassrls: false,
            connlimit: -1,
            password: None,
            valid_until: None,
            config: Vec::new(),
        }
    }
}

/// What a privilege is granted on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantObject {
    /// A table, view, materialized view or sequence, by OID.
    Relation(Oid),
    Schema(String),
    Function(Oid),
    Database,
    Type(Oid),
    /// A column of a table or view (`pg_attribute.attacl`): relation OID and attnum.
    Column(Oid, i16),
}

impl GrantObject {
    /// The relation a relation or column privilege is on.
    pub fn relation_oid(&self) -> Option<Oid> {
        match self {
            GrantObject::Relation(o) | GrantObject::Column(o, _) => Some(*o),
            _ => None,
        }
    }
}

/// One grantee's privileges on one object. `grantee` "" is PUBLIC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub object: GrantObject,
    pub grantee: String,
    pub grantor: String,
    /// Privilege letters (`aclitem` codes): r a w d D x t X U C c T m.
    pub privileges: Vec<char>,
    /// Privileges held WITH GRANT OPTION.
    pub grantable: Vec<char>,
}

/// `ALTER DEFAULT PRIVILEGES [IN SCHEMA s] GRANT ... ON {TABLES | SEQUENCES | ...}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultPrivilege {
    pub schema: Option<String>,
    /// `r` tables, `S` sequences, `f` functions, `T` types, `n` schemas.
    pub kind: char,
    pub grantee: String,
    pub privileges: Vec<char>,
}

/// An installed extension (`pg_extension`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionDef {
    pub oid: Oid,
    pub name: String,
    pub schema: String,
    pub version: String,
}

impl Catalog {
    pub fn extensions(&self) -> impl Iterator<Item = &ExtensionDef> {
        self.extensions.values()
    }

    pub fn extension(&self, name: &str) -> Option<&ExtensionDef> {
        self.extensions.get(name)
    }

    pub fn put_extension(&mut self, def: ExtensionDef) {
        self.extensions.insert(def.name.clone(), def);
    }

    pub fn drop_extension(&mut self, name: &str) -> Option<ExtensionDef> {
        self.extensions.remove(name)
    }

    pub fn roles(&self) -> impl Iterator<Item = &Arc<RoleDef>> {
        self.roles.values()
    }

    pub fn role(&self, name: &str) -> Option<&Arc<RoleDef>> {
        self.roles.get(name)
    }

    pub fn put_role(&mut self, def: RoleDef) {
        self.roles.insert(def.name.clone(), Arc::new(def));
    }

    pub fn drop_role(&mut self, name: &str) -> Option<Arc<RoleDef>> {
        self.members.retain(|(r, m, _)| r != name && m != name);
        self.default_privileges.retain(|d| d.grantee != name);
        self.roles.remove(name)
    }

    /// Renames a role everywhere it is referenced.
    pub fn rename_role(&mut self, old: &str, new: &str) {
        if let Some(r) = self.roles.remove(old) {
            let mut r = (*r).clone();
            new.clone_into(&mut r.name);
            self.roles.insert(new.to_owned(), Arc::new(r));
        }
        for m in &mut self.members {
            if m.0 == old {
                new.clone_into(&mut m.0);
            }
            if m.1 == old {
                new.clone_into(&mut m.1);
            }
        }
        for g in &mut self.grants {
            if g.grantee == old {
                new.clone_into(&mut g.grantee);
            }
        }
        for o in self.owners.values_mut() {
            if o == old {
                new.clone_into(o);
            }
        }
        self.rename_policy_role(old, new);
    }

    /// (role, member, admin option): `member` is a member of `role`.
    pub fn memberships(&self) -> &[(String, String, bool)] {
        &self.members
    }

    pub fn add_member(&mut self, role: &str, member: &str, admin: bool) {
        match self.members.iter_mut().find(|(r, m, _)| r == role && m == member) {
            Some(e) => e.2 |= admin,
            None => self.members.push((role.to_owned(), member.to_owned(), admin)),
        }
    }

    pub fn remove_member(&mut self, role: &str, member: &str) -> bool {
        let before = self.members.len();
        self.members.retain(|(r, m, _)| !(r == role && m == member));
        self.members.len() != before
    }

    /// The roles `name` is a member of, directly or indirectly (itself included).
    pub fn roles_of(&self, name: &str) -> Vec<String> {
        let mut out = vec![name.to_owned()];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i].clone();
            for (r, m, _) in &self.members {
                if *m == cur && !out.contains(r) {
                    out.push(r.clone());
                }
            }
            i += 1;
        }
        out
    }

    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// Adds privileges for a grantee on an object (merging with what it has).
    pub fn grant(&mut self, object: GrantObject, grantee: &str, grantor: &str, privileges: &[char], grantable: bool) {
        let entry = match self.grants.iter_mut().find(|g| g.object == object && g.grantee == grantee) {
            Some(g) => g,
            None => {
                self.grants.push(Grant {
                    object,
                    grantee: grantee.to_owned(),
                    grantor: grantor.to_owned(),
                    privileges: Vec::new(),
                    grantable: Vec::new(),
                });
                self.grants.last_mut().unwrap_or_else(|| unreachable!())
            }
        };
        for p in privileges {
            if !entry.privileges.contains(p) {
                entry.privileges.push(*p);
            }
            if grantable && !entry.grantable.contains(p) {
                entry.grantable.push(*p);
            }
        }
    }

    /// Removes privileges (or just their grant option).
    pub fn revoke(&mut self, object: &GrantObject, grantee: &str, privileges: &[char], grant_option_only: bool) {
        for g in self.grants.iter_mut().filter(|g| g.object == *object && g.grantee == grantee) {
            g.grantable.retain(|p| !privileges.contains(p));
            if !grant_option_only {
                g.privileges.retain(|p| !privileges.contains(p));
            }
        }
        self.grants.retain(|g| !g.privileges.is_empty());
    }

    /// Drops every grant on an object (when the object is dropped) or to a role.
    pub fn drop_grants(&mut self, f: impl Fn(&Grant) -> bool) {
        self.grants.retain(|g| !f(g));
    }

    pub fn default_privileges(&self) -> &[DefaultPrivilege] {
        &self.default_privileges
    }

    pub fn add_default_privilege(&mut self, d: DefaultPrivilege) {
        match self.default_privileges.iter_mut().find(|x| x.schema == d.schema && x.kind == d.kind && x.grantee == d.grantee) {
            Some(x) => {
                for p in d.privileges {
                    if !x.privileges.contains(&p) {
                        x.privileges.push(p);
                    }
                }
            }
            None => self.default_privileges.push(d),
        }
    }

    pub fn remove_default_privilege(&mut self, schema: Option<&str>, kind: char, grantee: &str, privileges: &[char]) {
        for x in self.default_privileges.iter_mut().filter(|x| x.schema.as_deref() == schema && x.kind == kind && x.grantee == grantee) {
            x.privileges.retain(|p| !privileges.contains(p));
        }
        self.default_privileges.retain(|x| !x.privileges.is_empty());
    }

    /// The recorded owner of an object (None: the bootstrap superuser).
    pub fn owner(&self, oid: Oid) -> Option<&str> {
        self.owners.get(&oid).map(String::as_str)
    }

    pub fn set_owner(&mut self, oid: Oid, owner: Option<&str>) {
        match owner {
            Some(o) => {
                self.owners.insert(oid, o.to_owned());
            }
            None => {
                self.owners.remove(&oid);
            }
        }
    }

    /// Objects owned by `role`, by OID.
    pub fn owned_by(&self, role: &str) -> Vec<Oid> {
        self.owners.iter().filter(|(_, o)| *o == role).map(|(k, _)| *k).collect()
    }
}
