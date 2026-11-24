//! Integration tests: each test drives a real Postgres client (the `postgres` crate, built on
//! tokio-postgres) against a fresh pgmem server.
//!
//! Set `PGMEM_TEST_DSN` (e.g. `host=127.0.0.1 port=5432 user=postgres`) to run the same
//! tests against a real Postgres server instead: each test gets its own temporary database.
//! That checks every assertion here is real Postgres behaviour, not just pgmem's.

// Not every helper is used by every module.
#[allow(dead_code)]
mod support;

mod check;
mod column_default;
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
mod generated_columns;
mod insert_default_marker;
mod insert_default_values;
mod insert_returning;
mod insert_select;
mod m2;
mod named_constraints;
mod named_stmt;
mod notnull;
mod qualified_star;
mod returning_star;
mod select_star;
mod serial;
mod server;
mod smoke;
mod table_alias;
mod truncate;
mod unique;
mod update;
mod update_from;
