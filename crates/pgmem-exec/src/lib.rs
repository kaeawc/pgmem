//! pgmem's query engine: the binder (name and type resolution, operator overloading,
//! parameter inference), the executor (relational operators, DML with constraint
//! enforcement, DDL), the builtin function library, and the per-connection [`Session`].

pub mod agg;
pub mod bind;
pub mod cast;
pub mod catalog_views;
pub mod ddl;
pub mod deparse;
pub mod dml;
pub mod eval;
pub mod funcs;
pub mod plan;
pub mod portal;
pub mod reg;
pub mod run;
pub mod session;
pub mod settings;
mod statement;
pub mod typeddl;
pub mod usertypes;
pub mod window;

pub use plan::{OutCol, Row};
pub use session::{Clock, Description, Notice, QueryResult, ServerClock, Session, SessionEnv, TxStatus};
