# Progress

Where the Rust rewrite of pgmem stands against its goal — behave like PostgreSQL 18 for anything a
test can observe — and what comes next. Updated with every PR.

## Metrics

| Metric | Value | How it is measured |
|---|---|---|
| pgmem-go parity | **37 / 564** Go tests ported | `scripts/parity.py check` (`parity/go_tests.tsv`) |
| SQL conformance corpus | not yet measured | Phase 2: sqllogictest corpus recorded from real PostgreSQL |

### History

| Date | PR | Parity | Notes |
|---|---|---|---|
| 2026-11-24 | Rust foundation: types, IR, parser | 37/564 | `types` and `postgres/parse` Go tests ported |

## The port, PR by PR

1. **Foundation** (this PR): Cargo workspace and CI, `pgmem-types` (types, values, SQLSTATE
   errors, text/binary codecs, numeric, date/time, jsonb), `pgmem-ir`, `pgmem-parse`.
2. Catalog and storage (row-level transaction overlays, snapshots, savepoints).
3. Executor: binder, operators, DML/DDL, builtin functions, sessions.
4. Wire protocol, `pgmem::Server`, test harness (pgmem or real Postgres), first integration tests.
5. to 8. Remaining pgmem-go integration tests by area.
9. Remove the Go implementation.

Then Phase 2: a conformance corpus checked against real PostgreSQL, a client matrix (psql,
tokio-postgres, sqlx, pgx, JDBC), and hill-climbing the gaps.
