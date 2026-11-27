//! Extension membership: the objects `ALTER EXTENSION ... ADD` attached to an extension
//! (`pg_depend` rows with deptype `e`).

use crate::{Catalog, Oid};

/// The catalog an extension member lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtMemberClass {
    /// A table, view, materialized view or sequence (`pg_class`).
    Relation,
    /// A function or procedure (`pg_proc`).
    Function,
    /// A type or domain (`pg_type`).
    Type,
    /// A schema (`pg_namespace`).
    Schema,
}

/// One object that belongs to an extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtMember {
    /// The owning extension's OID.
    pub ext: Oid,
    pub class: ExtMemberClass,
    pub oid: Oid,
}

impl Catalog {
    /// Records `oid` as a member of extension `ext`.
    pub fn add_extension_member(&mut self, ext: Oid, class: ExtMemberClass, oid: Oid) {
        self.ext_members.insert(oid, ExtMember { ext, class, oid });
    }

    /// Detaches `oid` from its extension.
    pub fn remove_extension_member(&mut self, oid: Oid) -> Option<ExtMember> {
        self.ext_members.remove(&oid)
    }

    /// The extension `oid` belongs to, if any.
    pub fn extension_of(&self, oid: Oid) -> Option<Oid> {
        self.ext_members.get(&oid).map(|m| m.ext)
    }

    /// Every member whose object still exists, by OID (what `pg_depend` lists with deptype
    /// `e`: `refclassid` is `pg_extension`, `refobjid` the extension, `classid` / `objid` the
    /// member).
    pub fn extension_members(&self) -> Vec<ExtMember> {
        self.ext_members.values().filter(|m| self.member_exists(m)).copied().collect()
    }

    /// The live members of extension `ext`.
    pub fn members_of_extension(&self, ext: Oid) -> Vec<ExtMember> {
        self.extension_members().into_iter().filter(|m| m.ext == ext).collect()
    }

    /// Forgets every member of extension `ext` (when the extension is dropped).
    pub fn clear_extension_members(&mut self, ext: Oid) {
        self.ext_members.retain(|_, m| m.ext != ext);
    }

    fn member_exists(&self, m: &ExtMember) -> bool {
        match m.class {
            ExtMemberClass::Relation => self.relation_by_oid(m.oid).is_some(),
            ExtMemberClass::Function => self.function_by_oid(m.oid).is_some(),
            ExtMemberClass::Type => self.type_by_oid(m.oid).is_some(),
            ExtMemberClass::Schema => self.schema_by_oid(m.oid).is_some(),
        }
    }
}
