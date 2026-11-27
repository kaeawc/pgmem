//! Table inheritance (`CREATE TABLE ... INHERITS`, `ALTER TABLE ... [NO] INHERIT`): each child's
//! parents (`pg_inherits`), and how often each column and constraint is inherited
//! (`attinhcount` / `attislocal`, `coninhcount` / `conislocal`, `connoinherit`). Partitions are
//! tracked separately (`partitions.rs`).

use crate::{Catalog, Oid, TableDef};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// How a column or constraint came to a table: from `count` parents, and/or defined locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inherited {
    pub count: i16,
    pub local: bool,
}

impl Default for Inherited {
    fn default() -> Inherited {
        Inherited { count: 0, local: true }
    }
}

impl Inherited {
    /// Inherited from one parent, not defined locally.
    pub const FROM_PARENT: Inherited = Inherited { count: 1, local: false };
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Inheritance {
    /// Each child's parents, in `inhseqno` order.
    parents: BTreeMap<Oid, Vec<Oid>>,
    /// Tables that have had inheritance children (`relhassubclass` stays set, as in Postgres).
    had_children: BTreeSet<Oid>,
    /// Columns by (table, slot); absent means local and not inherited.
    columns: BTreeMap<(Oid, usize), Inherited>,
    /// NOT NULL constraints by (table, slot).
    not_nulls: BTreeMap<(Oid, usize), Inherited>,
    /// CHECK constraints by constraint OID.
    constraints: BTreeMap<Oid, Inherited>,
    /// CHECK constraints declared `NO INHERIT`, by OID.
    no_inherit: BTreeSet<Oid>,
}

fn set<K: Ord>(map: &mut BTreeMap<K, Inherited>, key: K, v: Inherited) {
    if v == Inherited::default() {
        map.remove(&key);
    } else {
        map.insert(key, v);
    }
}

impl Catalog {
    /// The parents of an inheritance child, in `inhseqno` order.
    pub fn inherit_parents(&self, child: Oid) -> &[Oid] {
        self.inheritance.parents.get(&child).map_or(&[], Vec::as_slice)
    }

    pub fn set_inherit_parents(&mut self, child: Oid, parents: Vec<Oid>) {
        self.inheritance.had_children.extend(parents.iter().copied());
        if parents.is_empty() {
            self.inheritance.parents.remove(&child);
        } else {
            self.inheritance.parents.insert(child, parents);
        }
    }

    /// Every (child, parent, inhseqno) link.
    pub fn inherit_links(&self) -> Vec<(Oid, Oid, i32)> {
        self.inheritance
            .parents
            .iter()
            .flat_map(|(c, ps)| ps.iter().enumerate().map(move |(i, p)| (*c, *p, i32::try_from(i + 1).unwrap_or(i32::MAX))))
            .collect()
    }

    /// The direct inheritance children of a table, in OID order.
    pub fn inherit_children(&self, parent: Oid) -> Vec<Oid> {
        self.inheritance.parents.iter().filter(|(_, ps)| ps.contains(&parent)).map(|(c, _)| *c).collect()
    }

    /// Every table below `parent` in the inheritance tree, each once, nearest first.
    pub fn inherit_descendants(&self, parent: Oid) -> Vec<Arc<TableDef>> {
        let mut seen = vec![parent];
        let mut out = Vec::new();
        let mut i = 0;
        while i < seen.len() {
            for c in self.inherit_children(seen[i]) {
                if !seen.contains(&c) {
                    seen.push(c);
                    if let Some(t) = self.table_by_oid(c) {
                        out.push(Arc::clone(t));
                    }
                }
            }
            i += 1;
        }
        out
    }

    /// Whether `ancestor` is `table` or above it in the inheritance tree.
    pub fn inherits_from(&self, table: Oid, ancestor: Oid) -> bool {
        table == ancestor || self.inherit_parents(table).iter().any(|p| self.inherits_from(*p, ancestor))
    }

    /// `pg_class.relhassubclass` for regular inheritance.
    pub fn had_inherit_children(&self, table: Oid) -> bool {
        self.inheritance.had_children.contains(&table)
    }

    pub fn column_inheritance(&self, table: Oid, slot: usize) -> Inherited {
        self.inheritance.columns.get(&(table, slot)).copied().unwrap_or_default()
    }

    pub fn set_column_inheritance(&mut self, table: Oid, slot: usize, v: Inherited) {
        set(&mut self.inheritance.columns, (table, slot), v);
    }

    pub fn not_null_inheritance(&self, table: Oid, slot: usize) -> Inherited {
        self.inheritance.not_nulls.get(&(table, slot)).copied().unwrap_or_default()
    }

    pub fn set_not_null_inheritance(&mut self, table: Oid, slot: usize, v: Inherited) {
        set(&mut self.inheritance.not_nulls, (table, slot), v);
    }

    pub fn constraint_inheritance(&self, constraint: Oid) -> Inherited {
        self.inheritance.constraints.get(&constraint).copied().unwrap_or_default()
    }

    pub fn set_constraint_inheritance(&mut self, constraint: Oid, v: Inherited) {
        set(&mut self.inheritance.constraints, constraint, v);
    }

    /// `pg_constraint.connoinherit` of a CHECK constraint.
    pub fn is_no_inherit(&self, constraint: Oid) -> bool {
        self.inheritance.no_inherit.contains(&constraint)
    }

    pub fn set_no_inherit(&mut self, constraint: Oid, on: bool) {
        if on {
            self.inheritance.no_inherit.insert(constraint);
        } else {
            self.inheritance.no_inherit.remove(&constraint);
        }
    }

    /// Forgets a dropped table's inheritance state.
    pub(crate) fn drop_inheritance(&mut self, table: &TableDef) {
        let inh = &mut self.inheritance;
        inh.parents.remove(&table.oid);
        for ps in inh.parents.values_mut() {
            ps.retain(|p| *p != table.oid);
        }
        inh.parents.retain(|_, ps| !ps.is_empty());
        inh.had_children.remove(&table.oid);
        inh.columns.retain(|(t, _), _| *t != table.oid);
        inh.not_nulls.retain(|(t, _), _| *t != table.oid);
        for c in &table.constraints {
            inh.constraints.remove(&c.oid);
            inh.no_inherit.remove(&c.oid);
        }
    }
}
