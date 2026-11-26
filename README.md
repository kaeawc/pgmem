# pgmem

An in-memory, Postgres-wire-compatible database for tests. **Fidelity over performance.**

pgmem speaks the PostgreSQL frontend/backend protocol, so any Postgres client connects to it
unmodified — tokio-postgres, sqlx, Go's pgx, JDBC, psql. Each server is an isolated database that
boots in milliseconds, lives in memory, and disappears when dropped. Its goal is to behave like
real PostgreSQL wherever a test can observe the difference: result types and formats, SQLSTATE
error codes and messages, transaction semantics, constraint enforcement.

> This repository started as **pgmem-go** (Go). It now holds the Rust rewrite, which ports every
> pgmem-go test ([PARITY.md](PARITY.md)); the Go implementation has been removed. The Go
> [`examples/`](examples/) remain as sqlc + pgx suites running against the Rust server.

## Install

```bash
cargo add --dev pgmem          # library, for Rust tests
cargo binstall pgmem           # the `pgmem` server binary, prebuilt (or: cargo install pgmem)
```

Prebuilt binaries for Linux (glibc and static musl), macOS and Windows are attached to each
[GitHub release](https://github.com/kaeawc/pgmem/releases).

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
rows)` loads rows without SQL; `Server::fork()` starts a second server on an independent copy of
the first one's committed data.

## Embedding

`pgmem::Embedded` runs the database in-process, without the wire protocol. With
`pgmem = { version = "...", default-features = false }` it builds without tokio (the `server`
feature adds `Server` and the binary).

```rust
let base = pgmem::Embedded::new();
let mut session = base.session();
// Simple-query semantics: one implicit transaction, rolled back if any statement fails.
for result in session.execute_batch("CREATE TABLE t (id serial, v text); INSERT INTO t (v) VALUES ('a')") {
    result?;                                     // pgmem::DbError: code (SQLSTATE), message, detail, hint
}
let out = session.query("SELECT id, v FROM t")?; // columns (name, type OID) and rows as Postgres text
assert_eq!(out.rows, vec![vec![Some("1".to_owned()), Some("a".to_owned())]]);

let trial = base.fork();                         // cheap copy of the committed state, sequences included
```

`pgmem::to_pg_text(&value)` renders a value in Postgres text output form.

## Use it from any language

```bash
cargo run --release -p pgmem -- --port 5432     # prints the DSN, serves until interrupted
                                                # --port 0 picks a free port; --now pins now()
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
large builtin function library. Details: [PROGRESS.md](PROGRESS.md). Conformance against PostgreSQL 18: [CONFORMANCE.md](CONFORMANCE.md).
Architecture: [DESIGN.md](DESIGN.md).

## Development

See [AGENTS.md](AGENTS.md). Integration tests run against pgmem by default; set
`PGMEM_TEST_DSN="host=... port=... user=..."` to run the same tests against real PostgreSQL.
