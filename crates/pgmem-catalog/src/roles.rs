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

/// One grantee's entry in a default ACL. `grantee` "" is PUBLIC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultItem {
    pub grantee: String,
    pub privileges: Vec<char>,
    pub grantable: Vec<char>,
}

/// One `pg_default_acl` entry: what `ALTER DEFAULT PRIVILEGES FOR ROLE role [IN SCHEMA s]`
/// grants on new objects of `kind` (`r` tables, `S` sequences, `f` functions, `T` types,
/// `n` schemas). A global entry (no schema) starts from the hard-wired defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DefaultPrivilege {
    pub role: String,
    pub schema: Option<String>,
    pub kind: char,
    pub items: Vec<DefaultItem>,
}

impl DefaultPrivilege {
    /// The hard-wired default ACL of a new object of `kind` owned by `owner` (`acldefault`).
    pub fn hardwired(owner: &str, kind: char) -> Vec<DefaultItem> {
        let (own, public) = match kind {
            'S' => ("rwU", ""),
            'f' => ("X", "X"),
            'T' => ("U", "U"),
            'n' => ("UC", ""),
            _ => ("arwdDxtm", ""),
        };
        let item =
            |grantee: &str, p: &str| DefaultItem { grantee: grantee.to_owned(), privileges: p.chars().collect(), grantable: Vec::new() };
        let mut items = vec![item(owner, own)];
        if !public.is_empty() {
            items.push(item("", public));
        }
        items
    }

    /// A new entry: global ones start from the hard-wired defaults, schema ones empty.
    pub fn new(role: &str, schema: Option<&str>, kind: char) -> DefaultPrivilege {
        let items = if schema.is_none() { Self::hardwired(role, kind) } else { Vec::new() };
        DefaultPrivilege { role: role.to_owned(), schema: schema.map(str::to_owned), kind, items }
    }

    pub fn grant(&mut self, grantee: &str, privileges: &[char], grantable: bool) {
        let i = match self.items.iter().position(|x| x.grantee == grantee) {
            Some(i) => i,
            None => {
                self.items.push(DefaultItem { grantee: grantee.to_owned(), privileges: Vec::new(), grantable: Vec::new() });
                self.items.len() - 1
            }
        };
        let item = &mut self.items[i];
        for p in privileges {
            if !item.privileges.contains(p) {
                item.privileges.push(*p);
            }
            if grantable && !item.grantable.contains(p) {
                item.grantable.push(*p);
            }
        }
    }

    pub fn revoke(&mut self, grantee: &str, privileges: &[char], grant_option_only: bool) {
        for x in self.items.iter_mut().filter(|x| x.grantee == grantee) {
            x.grantable.retain(|p| !privileges.contains(p));
            if !grant_option_only {
                x.privileges.retain(|p| !privileges.contains(p));
            }
        }
        self.items.retain(|x| !x.privileges.is_empty());
    }

    /// Whether the entry adds nothing (so Postgres deletes it): a global one equal to the
    /// hard-wired defaults, a schema one with no items.
    pub fn is_default(&self) -> bool {
        if self.schema.is_some() {
            return self.items.is_empty();
        }
        let sorted = |items: &[DefaultItem]| {
            let mut v: Vec<(String, Vec<char>, Vec<char>)> = items
                .iter()
                .map(|x| {
                    let (mut p, mut g) = (x.privileges.clone(), x.grantable.clone());
                    p.sort_unstable();
                    g.sort_unstable();
                    (x.grantee.clone(), p, g)
                })
                .collect();
            v.sort();
            v
        };
        sorted(&self.items) == sorted(&Self::hardwired(&self.role, self.kind))
    }
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
        self.drop_default_privileges_of(name);
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
        for d in &mut self.default_privileges {
            if d.role == old {
                new.clone_into(&mut d.role);
            }
            for x in &mut d.items {
                if x.grantee == old {
                    new.clone_into(&mut x.grantee);
                }
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

    pub fn default_privilege(&self, role: &str, schema: Option<&str>, kind: char) -> Option<&DefaultPrivilege> {
        self.default_privileges.iter().find(|d| d.role == role && d.schema.as_deref() == schema && d.kind == kind)
    }

    /// Stores an entry (replacing its predecessor); an entry that adds nothing is removed.
    pub fn put_default_privilege(&mut self, d: DefaultPrivilege) {
        self.default_privileges.retain(|x| !(x.role == d.role && x.schema == d.schema && x.kind == d.kind));
        if !d.is_default() {
            self.default_privileges.push(d);
        }
    }

    /// Removes a role's default-ACL entries and its grantee items (`DROP OWNED`).
    pub fn drop_default_privileges_of(&mut self, role: &str) {
        self.default_privileges.retain(|d| d.role != role);
        let entries = std::mem::take(&mut self.default_privileges);
        for mut d in entries {
            d.items.retain(|x| x.grantee != role);
            self.put_default_privilege(d);
        }
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
