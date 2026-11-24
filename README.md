# pgmem

An in-memory, Postgres-wire-compatible database for tests. **Fidelity over performance.**

pgmem speaks the PostgreSQL frontend/backend protocol, so any Postgres client connects to it
unmodified — tokio-postgres, sqlx, Go's pgx, JDBC, psql. Each server is an isolated database that
boots in milliseconds, lives in memory, and disappears when dropped. Its goal is to behave like
real PostgreSQL wherever a test can observe the difference: result types and formats, SQLSTATE
error codes and messages, transaction semantics, constraint enforcement.

> This repository started as **pgmem-go** (Go). It now holds the Rust rewrite; the Go code stays
> until the Rust version reached parity with its whole test suite (it has: see
> [PARITY.md](PARITY.md)) and is then removed. The repository name is unchanged.

## Use it from Rust tests

```rust
let server = pgmem::Server::start()?;            // free localhost port, isolated database
let mut client = postgres::Client::connect(&server.conninfo(), postgres::NoTls)?;
client.batch_execute("CREATE TABLE users (id serial PRIMARY KEY, email text UNIQUE NOT NULL)")?;
let row = client.query_one("INSERT INTO users (email) VALUES ($1) RETURNING id", &[&"a@example.com"])?;
let id: i32 = row.get(0);
// server.dsn() is a postgres:// URL for sqlx and other clients.
```

Test helpers: `Server::set_now(Some(t))` pins `now()` / `CURRENT_TIMESTAMP`; `Server::seed(table,
rows)` loads rows without SQL.

## Use it from any language

```bash
cargo run --release -p pgmem -- --port 5432     # prints the DSN, serves until interrupted
psql "postgres://pgmem@127.0.0.1:5432/pgmem?sslmode=disable"
```

## What works

SELECT with joins (inner/left/right/full/cross, USING, NATURAL, LATERAL), subqueries (scalar,
IN/EXISTS/ANY/ALL, correlated), CTEs (including recursive), set operations, DISTINCT ON, GROUP BY
with Postgres' functional-dependency rule, aggregates (DISTINCT, FILTER, ORDER BY), window functions
with frames; INSERT/UPDATE/DELETE with RETURNING, ON CONFLICT, UPDATE ... FROM, DELETE ... USING;
DDL (tables, views, indexes, sequences, ALTER TABLE) — transactional, like Postgres; constraints
(NOT NULL, CHECK, PRIMARY KEY, UNIQUE and unique indexes, FOREIGN KEY with actions, generated and
identity columns); transactions with savepoints and isolation levels; types `bool`, `int2/4/8`,
`float4/8`, `numeric`, `text`/`varchar(n)`, `bytea`, `uuid`, `date`, `time`, `timetz`,
`timestamp`, `timestamptz`, `interval`, `jsonb` and arrays, with binary and text wire formats; a
large builtin function library. Details and the conformance status: [PROGRESS.md](PROGRESS.md).
Architecture: [DESIGN.md](DESIGN.md).

## Development

See [AGENTS.md](AGENTS.md). Integration tests run against pgmem by default; set
`PGMEM_TEST_DSN="host=... port=... user=..."` to run the same tests against real PostgreSQL.
