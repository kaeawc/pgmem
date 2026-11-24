//! Virtual `pg_catalog` / `information_schema` relations, computed from the live catalog on
//! each query (no materialized copy to drift). Populated incrementally as drivers need them.

use pgmem_catalog::Catalog;
use pgmem_types::{Type, Value};

pub struct VirtualTable {
    pub columns: &'static [(&'static str, Type)],
    pub rows: fn(&Catalog) -> Vec<Vec<Value>>,
}

/// Looks up a virtual relation by schema and name.
pub fn lookup(_schema: &str, _name: &str) -> Option<VirtualTable> {
    None
}
