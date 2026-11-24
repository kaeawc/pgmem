# pgmem

An in-memory, Postgres-wire-compatible database for tests, written in Rust. **Fidelity over
performance**: when pgmem and real PostgreSQL disagree, pgmem is wrong.

This repository is a Rust rewrite of pgmem-go (the Go implementation was removed once every Go
test had a Rust port; see PARITY.md). `examples/` is a Go module of sqlc + pgx suites that run
against the Rust `pgmem` binary as a client-compatibility check (CI job `pgx-examples`).

## Working rules

- New dependencies go in root `[workspace.dependencies]` with `default-features = false` and only
  the features used; crates reference them with `{ workspace = true }`.
- One version of every crate (`deny.toml`, enforced by the CI `deny` job). Pure Rust only: no C
  toolchain, no OpenSSL.
- Every behaviour is checked against real PostgreSQL: integration tests run against pgmem by
  default and against real Postgres when `PGMEM_TEST_DSN` is set (CI job `postgres-fidelity`).
  A test that passes on pgmem but fails on Postgres is a bug in the test.
- Error codes and messages follow Postgres (SQLSTATE + wording); clients match on them.
- Integration tests live in one binary: `crates/pgmem/tests/it/` (one module per area). Unit tests
  stay in-module. Doctests are disabled.
- Time goes through the server `Clock` (`Server::set_now`), never `SystemTime::now()` in engine code.
- The SQL conformance corpus (`conformance/<area>/*.slt`, format in
  `crates/pgmem-conformance/src/corpus.rs`) is the feature-completeness measure. Expected results
  are recorded from PostgreSQL 18 (`--dsn ... --record`), never written by hand, and every record
  must pass on Postgres (CI `postgres-fidelity`). pgmem's failures are listed in
  `conformance/known_failures.txt`; CI job `conformance` fails on any change to that set, so after
  fixing something run `cargo run -p pgmem-conformance -- --update` and commit the result
  (it also regenerates CONFORMANCE.md).
- `parity/go_tests.tsv` maps every pgmem-go test to its Rust port; `scripts/parity.py check` must
  pass. Deviations (where pgmem-go was not Postgres-accurate) need a note.

## Build & validate

```bash
. /workspaces/tools/env.sh          # in Hive VMs: toolchain lives under /workspaces/tools
cargo c                             # check --all-targets
cargo t                             # nextest run (or: cargo test)
cargo lint                          # clippy --all-targets -D warnings
cargo fmt --all
cargo deny check
python3 scripts/parity.py check
cargo run -p pgmem-conformance       # corpus vs pgmem (--update after fixes; -v shows failures)
cargo run -p pgmem-conformance -- --dsn "host=127.0.0.1 port=5432 user=postgres" [--record] [area/]
(cd examples && go test ./...)       # pgx suites against target/release/pgmem
PGMEM_TEST_DSN="host=127.0.0.1 port=5432 user=postgres" cargo nextest run -p pgmem --test it
```

After any change: `cargo c && cargo lint && cargo t`.

## Map

- `crates/pgmem-types` — `Type` (Postgres OIDs), `Value`, `SqlError` (SQLSTATE), text/binary codecs,
  numeric, date/time (Postgres calendar algorithms), jsonb.
- `crates/pgmem-ir` — dialect-neutral statement tree produced by parsers.
- `crates/pgmem-parse` — SQL → IR via `sqlparser` (PostgreSQL dialect), Postgres identifier folding,
  `42601` syntax errors, rewrites for syntax sqlparser lacks.
- `crates/pgmem-catalog` — tables, columns, constraints, indexes, views, sequences.
- `crates/pgmem-storage` — committed state + per-transaction row overlays, snapshots, savepoints,
  sequences.
- `crates/pgmem-exec` — binder (`bind/`), executor (`run.rs`, `agg.rs`, `window.rs`), DML/DDL,
  builtins (`funcs/`), `Session` (transaction state machine, settings).
- `crates/pgmem-wire` — protocol v3 server (startup, simple and extended query).
- `crates/pgmem` — public `Server` API, the `pgmem` binary, integration tests.
- `crates/pgmem-conformance` — corpus runner (records from Postgres, ratchets pgmem's pass rate).
- `examples/` — Go sqlc + pgx projects; `examples/internal/pgmemtest` starts the Rust binary.
