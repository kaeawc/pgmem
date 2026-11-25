//! Declarative partitioning (`pg_partitioned_table`, `relpartbound`, `pg_inherits`): the
//! partition key of partitioned tables and the bound of each partition, by table OID.

use crate::{Catalog, Oid, TableDef};
use pgmem_ir::Expr;
use pgmem_types::Value;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartitionStrategy {
    Range,
    List,
    Hash,
}

impl PartitionStrategy {
    /// `pg_partitioned_table.partstrat`.
    pub fn code(self) -> char {
        match self {
            PartitionStrategy::Range => 'r',
            PartitionStrategy::List => 'l',
            PartitionStrategy::Hash => 'h',
        }
    }

    pub fn keyword(self) -> &'static str {
        match self {
            PartitionStrategy::Range => "RANGE",
            PartitionStrategy::List => "LIST",
            PartitionStrategy::Hash => "HASH",
        }
    }
}

/// One element of a partition key: a column (physical slot) or an expression over the row.
#[derive(Clone, Debug, PartialEq)]
pub enum PartitionKeyPart {
    Column(usize),
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PartitionKey {
    pub strategy: PartitionStrategy,
    pub parts: Vec<PartitionKeyPart>,
}

/// A range bound datum.
#[derive(Clone, Debug, PartialEq)]
pub enum RangeDatum {
    MinValue,
    MaxValue,
    Value(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PartitionBound {
    Default,
    /// `FOR VALUES IN (...)` (NULL allowed).
    List(Vec<Value>),
    Range { from: Vec<RangeDatum>, to: Vec<RangeDatum> },
    Hash { modulus: u32, remainder: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PartitionOf {
    pub parent: Oid,
    pub bound: PartitionBound,
}

impl Catalog {
    pub fn partition_key(&self, table: Oid) -> Option<&PartitionKey> {
        self.partition_keys.get(&table)
    }

    pub fn set_partition_key(&mut self, table: Oid, key: Option<PartitionKey>) {
        match key {
            Some(k) => {
                self.partition_keys.insert(table, k);
            }
            None => {
                self.partition_keys.remove(&table);
            }
        }
    }

    pub fn partition_of(&self, table: Oid) -> Option<&PartitionOf> {
        self.partition_bounds.get(&table)
    }

    pub fn set_partition_of(&mut self, table: Oid, of: Option<PartitionOf>) {
        match of {
            Some(p) => {
                self.partition_bounds.insert(table, p);
            }
            None => {
                self.partition_bounds.remove(&table);
            }
        }
    }

    /// The direct partitions of a partitioned table, in creation (OID) order.
    pub fn partitions(&self, parent: Oid) -> Vec<(Arc<TableDef>, &PartitionOf)> {
        let mut out: Vec<(Arc<TableDef>, &PartitionOf)> = self
            .partition_bounds
            .iter()
            .filter(|(_, p)| p.parent == parent)
            .filter_map(|(oid, p)| self.table_by_oid(*oid).map(|t| (Arc::clone(t), p)))
            .collect();
        out.sort_by_key(|(t, _)| t.oid);
        out
    }

    /// Every partition below `parent`, depth first (sub-partitioned ones included).
    pub fn partition_descendants(&self, parent: Oid) -> Vec<Arc<TableDef>> {
        let mut out = Vec::new();
        for (t, _) in self.partitions(parent) {
            let oid = t.oid;
            out.push(t);
            out.extend(self.partition_descendants(oid));
        }
        out
    }

    /// The leaf partitions below a partitioned table (the tables holding its rows).
    pub fn partition_leaves(&self, parent: Oid) -> Vec<Arc<TableDef>> {
        self.partition_descendants(parent).into_iter().filter(|t| !self.partition_keys.contains_key(&t.oid)).collect()
    }

    /// The ancestors of a partition, nearest first.
    pub fn partition_ancestors(&self, table: Oid) -> Vec<Oid> {
        let mut out = Vec::new();
        let mut cur = table;
        while let Some(p) = self.partition_bounds.get(&cur) {
            out.push(p.parent);
            cur = p.parent;
        }
        out
    }

    pub fn is_partitioned(&self, table: Oid) -> bool {
        self.partition_keys.contains_key(&table)
    }
}
