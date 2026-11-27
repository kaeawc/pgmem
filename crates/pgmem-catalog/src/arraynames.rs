//! Names of the array types Postgres creates alongside every table, view, materialized view
//! and user type (`_name`, or another name `makeArrayTypeName` picks when that one is taken),
//! by the OID of the object that owns them. The names depend on history (a later type named
//! like an existing array type pushes that array out of the way, `moveArrayTypeName`), so they
//! are stored rather than derived.

use crate::{Catalog, MAX_IDENTIFIER_LEN, Oid, QualName, clip};

impl Catalog {
    /// The name of the array type of `owner`'s type (`base`): the recorded one, else `_base`.
    pub fn array_type_name(&self, owner: Oid, base: &str) -> String {
        self.array_names.get(&owner).cloned().unwrap_or_else(|| array_name(base, None))
    }

    /// The object (table, view or user type, by OID) whose array type is `schema.name`.
    pub fn array_type_owner(&self, schema: &str, name: &str) -> Option<Oid> {
        self.array_owners(schema).find(|(_, n)| n == name).map(|(o, _)| o)
    }

    /// (owner OID, array type name) of every object in `schema` that has an array type.
    fn array_owners<'a>(&'a self, schema: &'a str) -> impl Iterator<Item = (Oid, String)> + 'a {
        let tables = self.tables.values().filter(move |t| t.schema == schema).map(|t| (t.oid, t.name.as_str()));
        let views = self.views.values().filter(move |v| v.schema == schema).map(|v| (v.oid, v.name.as_str()));
        let types = self.types.values().filter(move |t| t.schema == schema).map(|t| (t.oid, t.name.as_str()));
        tables.chain(views).chain(types).map(|(o, n)| (o, self.array_type_name(o, n)))
    }

    /// True if `pg_type` has a type `schema.name` (array types of `except` aside).
    fn type_name_taken(&self, schema: &str, name: &str, except: Option<Oid>) -> bool {
        let key = QualName::new(schema, name);
        self.types.contains_key(&key)
            || self.tables.contains_key(&key)
            || self.views.contains_key(&key)
            || self.array_owners(schema).any(|(o, n)| n == name && Some(o) != except)
    }

    /// `makeArrayTypeName`: `_base`, or `_base_1`, `_base_2`, ... until the name is free.
    fn choose_array_name(&self, schema: &str, base: &str, except: Option<Oid>) -> String {
        let mut name = array_name(base, None);
        let mut pass = 0;
        while self.type_name_taken(schema, &name, except) {
            pass += 1;
            name = array_name(base, Some(pass));
        }
        name
    }

    /// `moveArrayTypeName`: before a type named `schema.name` is created (or another type is
    /// renamed to it), an array type of that name is renamed out of the way.
    pub(crate) fn move_array_type_name(&mut self, schema: &str, name: &str) {
        if let Some(owner) = self.array_type_owner(schema, name) {
            let moved = self.choose_array_name(schema, name, None);
            self.array_names.insert(owner, moved);
        }
    }

    /// Names the array type of a new type `schema.base` owned by `owner`.
    pub(crate) fn name_array_type(&mut self, owner: Oid, schema: &str, base: &str) {
        let name = self.choose_array_name(schema, base, Some(owner));
        self.array_names.insert(owner, name);
    }

    /// `RenameTypeInternal`: a type renamed within its schema renames its array type too. Run
    /// before the definition changes: the old name still counts as taken, the type's own array
    /// type does not.
    pub(crate) fn rename_array_type(&mut self, owner: Oid, schema: &str, new: &str) {
        if self.array_type_owner(schema, new).is_some_and(|o| o != owner) {
            self.move_array_type_name(schema, new);
        }
        self.name_array_type(owner, schema, new);
    }
}

/// `makeObjectName("", base, suffix)`: `_base[_suffix]`, `base` cut to fit 63 bytes.
fn array_name(base: &str, suffix: Option<u32>) -> String {
    let label = suffix.map(|s| format!("_{s}")).unwrap_or_default();
    let avail = MAX_IDENTIFIER_LEN.saturating_sub(1 + label.len());
    format!("_{}{label}", clip(base, avail))
}

#[cfg(test)]
mod tests {
    use super::array_name;

    #[test]
    fn array_names_follow_make_object_name() {
        assert_eq!(array_name("zpoint", None), "_zpoint");
        assert_eq!(array_name("zz", Some(1)), "_zz_1");
        let long = "a".repeat(63);
        assert_eq!(array_name(&long, None).len(), 63);
        assert_eq!(array_name(&long, Some(12)), format!("_{}_12", "a".repeat(59)));
    }
}
