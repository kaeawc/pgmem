//! Integration tests: each test drives a real Postgres client (the `postgres` crate, built on
//! tokio-postgres) against a fresh pgmem server.
//!
//! Set `PGMEM_TEST_DSN` (e.g. `host=127.0.0.1 port=5432 user=postgres`) to run the same
//! tests against a real Postgres server instead: each test gets its own temporary database.
//! That checks every assertion here is real Postgres behaviour, not just pgmem's.

// Not every helper is used by every module.
#[allow(dead_code)]
mod support;

mod alter_column_notnull;
mod alter_table;
mod check;
mod column_default;
mod current_setting;
mod delete;
mod drop_table;
mod example_analytics;
mod example_blog;
mod example_chat;
mod example_events;
mod example_inventory;
mod example_todo;
mod exec_delete;
mod exec_funcs;
mod exec_funcs_substring;
mod exec_insert_check;
mod exec_insert_notnull;
mod exec_insert_returning;
mod exec_insert_serial;
mod exec_insert_unique;
mod exec_join;
mod exec_join_left_cross;
mod exec_update;
mod foreign_key;
mod foreign_key_cascade;
mod generated_columns;
mod index_noop;
mod insert_default_marker;
mod insert_default_values;
mod insert_returning;
mod insert_select;
mod locking;
mod m2;
mod migration_noops;
mod named_constraints;
mod named_stmt;
mod notnull;
mod on_conflict;
mod qualified_star;
mod returning_star;
mod savepoint;
mod select_star;
mod serial;
mod serialization;
mod server;
mod setnow;
mod smoke;
mod table_alias;
mod table_fk;
mod transactions;
mod truncate;
mod unique;
mod update;
mod update_from;
mod view;
