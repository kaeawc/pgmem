//! pgmem's query engine: the binder (name and type resolution, operator overloading,
//! parameter inference), the executor (relational operators, DML with constraint
//! enforcement, DDL), the builtin function library, and the per-connection [`Session`].

pub mod agg;
pub mod bind;
pub mod cast;
pub mod catalog_views;
pub mod copy;
pub mod ddl;
mod ddl_sequences;
mod ddl_views;
pub mod deparse;
pub mod deps;
pub mod dml;
pub mod eval;
mod extensions;
pub mod funcs;
pub mod functions;
pub mod opclass;
pub mod partition;
pub mod plan;
pub mod plpgsql;
pub mod portal;
pub mod reg;
pub mod roles;
pub mod run;
pub mod sequences;
pub mod session;
pub mod settings;
mod statement;
mod triggers;
pub mod typeddl;
pub mod usertypes;
pub mod views;
pub mod window;

pub use plan::{OutCol, Row};
pub use session::{Clock, CopyIn, CopyOut, Description, Notice, QueryResult, ServerClock, Session, SessionEnv, TxStatus};
