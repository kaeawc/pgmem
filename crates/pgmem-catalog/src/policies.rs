//! Row-level security: the policies of tables (`pg_policy`) and their
//! `relrowsecurity` / `relforcerowsecurity` flags, by table OID.

use crate::{Catalog, Oid};
use pgmem_ir::{Expr, ObjectName};
use std::sync::Arc;

/// A row-level security policy (`pg_policy`).
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyDef {
    pub oid: Oid,
    pub name: String,
    /// The table the policy is on.
    pub table: Oid,
    /// `polcmd`: `*` ALL, `r` SELECT, `a` INSERT, `w` UPDATE, `d` DELETE.
    pub command: char,
    pub permissive: bool,
    /// The roles the policy applies to; empty for PUBLIC.
    pub roles: Vec<String>,
    /// `USING`: which existing rows are visible (`polqual`).
    pub using: Option<Expr>,
    /// `WITH CHECK`: which new rows may be written (`polwithcheck`).
    pub with_check: Option<Expr>,
    /// The search_path the expressions were analyzed with (Postgres stores resolved OIDs):
    /// they bind with it, whatever the querying session's search_path.
    pub search_path: Vec<String>,
    /// The relations the expressions' subqueries read, as written and as resolved when the
    /// expressions were set: they keep binding to those relations when renamed, and the
    /// relations cannot be dropped without the policy (`pg_depend`).
    pub relations: Vec<(ObjectName, Oid)>,
}

impl PolicyDef {
    /// Whether the policy applies to statements of command `cmd` (`r`, `a`, `w`, `d`).
    pub fn applies_to(&self, cmd: char) -> bool {
        self.command == '*' || self.command == cmd
    }
}

impl Catalog {
    pub fn policies(&self) -> impl Iterator<Item = &Arc<PolicyDef>> {
        self.policies.values()
    }

    /// Policies of table `oid`, by name (the order Postgres applies them in).
    pub fn table_policies(&self, oid: Oid) -> Vec<Arc<PolicyDef>> {
        let mut out: Vec<Arc<PolicyDef>> = self.policies.values().filter(|p| p.table == oid).cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn policy_by_oid(&self, oid: Oid) -> Option<&Arc<PolicyDef>> {
        self.policies.get(&oid)
    }

    pub fn put_policy(&mut self, def: PolicyDef) {
        self.policies.insert(def.oid, Arc::new(def));
    }

    pub fn drop_policy(&mut self, oid: Oid) -> Option<Arc<PolicyDef>> {
        self.comments.retain(|(_, o, _), _| *o != oid);
        self.policies.remove(&oid)
    }

    /// (`relrowsecurity`, `relforcerowsecurity`) of table `oid`.
    pub fn row_security(&self, oid: Oid) -> (bool, bool) {
        self.row_security.get(&oid).copied().unwrap_or_default()
    }

    pub fn set_row_security(&mut self, oid: Oid, enabled: Option<bool>, forced: Option<bool>) {
        let (mut e, mut f) = self.row_security(oid);
        e = enabled.unwrap_or(e);
        f = forced.unwrap_or(f);
        if e || f {
            self.row_security.insert(oid, (e, f));
        } else {
            self.row_security.remove(&oid);
        }
    }

    /// Forgets the policies and flags of a dropped table.
    pub(crate) fn drop_table_policies(&mut self, oid: Oid) {
        let dropped: Vec<Oid> = self.policies.values().filter(|p| p.table == oid).map(|p| p.oid).collect();
        for p in dropped {
            self.drop_policy(p);
        }
        self.row_security.remove(&oid);
    }

    /// The policies on other tables whose expressions read relation `oid`, by OID.
    pub fn policies_reading(&self, oid: Oid) -> Vec<Arc<PolicyDef>> {
        let mut out: Vec<Arc<PolicyDef>> =
            self.policies.values().filter(|p| p.table != oid && p.relations.iter().any(|(_, r)| *r == oid)).cloned().collect();
        out.sort_by_key(|p| p.oid);
        out
    }

    /// Keeps policy role lists pointing at a renamed role.
    pub(crate) fn rename_policy_role(&mut self, old: &str, new: &str) {
        for p in self.policies.values_mut() {
            if p.roles.iter().any(|r| r == old) {
                let mut def = (**p).clone();
                for r in &mut def.roles {
                    if r == old {
                        new.clone_into(r);
                    }
                }
                *p = Arc::new(def);
            }
        }
    }
}
