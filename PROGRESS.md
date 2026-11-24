# Progress

Where pgmem stands against its goal — behave like PostgreSQL 18 for anything a test can observe —
and what comes next. Updated with every PR.

## Metrics

| Metric | Value | How it is measured |
|---|---|---|
| pgmem-go parity | **564 / 564** Go tests ported | `scripts/parity.py check` ([PARITY.md](PARITY.md)) |
| Integration suite on pgmem | **517 / 517** pass | `cargo nextest run -p pgmem --test it` |
| Integration suite on real PostgreSQL 18 | **517 / 517** pass | same suite with `PGMEM_TEST_DSN` (CI `postgres-fidelity`) |
| Go pgx example suites (sqlc) against the Rust binary | **27 / 27** pass | `make examples` (CI `pgx-examples`) |
| SQL conformance corpus | not yet measured | Phase 2: sqllogictest corpus recorded from real PostgreSQL |

### Conformance history

| Date | PR | Parity | Conformance corpus |
|---|---|---|---|
| 2026-11-24 | #103 foundation: types, IR, parser | 37/564 | — |
| 2026-11-24 | #104 catalog and storage | 49/564 | — |
| 2026-11-24 | #105 executor | 49/564 | — |
| 2026-11-24 | #106 wire protocol, server, core + example tests | 189/564 | — |
| 2026-11-24 | #107 transactions, constraints, DDL tests | 268/564 | — |
| 2026-11-24 | #108 query shape tests (joins, subqueries, CTEs, aggregates, windows) | 364/564 | — |
| 2026-11-24 | #109 types and expression tests | 466/564 | — |
| 2026-11-24 | #110 builtin function tests | 564/564 | — |
| 2026-11-24 | #111 remove the Go implementation; pgx examples run against Rust | 564/564 | — |

## Supported

- **Wire protocol**: protocol 3.0 (3.x and `_pq_` options negotiated down), trust auth, SSL/GSS
  requests refused, simple query (multi-statement implicit transactions), extended query
  (Parse/Bind/Describe/Execute/Close/Sync/Flush, named statements and portals, row-limited
  Execute with PortalSuspended), skip-until-Sync after errors, ParameterStatus, notices, error
  fields (SQLSTATE, detail, hint, schema/table/column/constraint).
- **Types**: bool, int2/4/8, float4/8, numeric (arbitrary precision, Postgres scale rules),
  text/varchar(n)/char(n) (as text), bytea, uuid, date, time, timetz, timestamp, timestamptz,
  interval, jsonb (`json` is treated as jsonb), one-dimensional arrays; text and binary formats.
- **Queries**: joins (inner/left/right/full/cross, USING, NATURAL, LATERAL), subqueries (scalar,
  IN, EXISTS, ANY/ALL, ARRAY(), correlated), WITH and WITH RECURSIVE, UNION/INTERSECT/EXCEPT
  [ALL], DISTINCT, DISTINCT ON, GROUP BY (ordinal, alias, functional dependency on the primary
  key), HAVING, ORDER BY (ordinal, alias, expression, NULLS FIRST/LAST), LIMIT/OFFSET/FETCH,
  VALUES, set-returning functions in FROM (generate_series, unnest, jsonb_each, ...), views.
- **Aggregates & windows**: count/sum/avg/min/max/bool_and/bool_or/every/string_agg/array_agg/
  jsonb_agg/jsonb_object_agg/stddev/variance/bit_and/bit_or with DISTINCT, FILTER and ORDER BY;
  row_number/rank/dense_rank/percent_rank/cume_dist/ntile/lag/lead/first_value/last_value/
  nth_value and aggregates over windows with ROWS/RANGE/GROUPS frames (RANGE without offsets).
- **DML**: INSERT (VALUES, SELECT, DEFAULT VALUES, DEFAULT), ON CONFLICT DO NOTHING/DO UPDATE with
  arbiter inference, UPDATE ... FROM, DELETE ... USING, RETURNING, TRUNCATE (RESTART IDENTITY,
  CASCADE).
- **DDL**: CREATE/DROP TABLE, VIEW, INDEX (unique and partial unique indexes are enforced),
  SEQUENCE; ALTER TABLE (add/drop/rename column, set/drop default and NOT NULL, alter type, add/
  drop constraint, rename table); no-op acceptance of COMMENT, extensions, schemas, GRANT, VACUUM,
  ANALYZE. DDL is transactional.
- **Constraints**: NOT NULL, CHECK, PRIMARY KEY, UNIQUE (NULLS NOT DISTINCT), FOREIGN KEY
  (multi-column; NO ACTION/RESTRICT/CASCADE/SET NULL/SET DEFAULT on delete and update), serial
  and identity columns, generated columns; Postgres constraint naming.
- **Transactions**: BEGIN/COMMIT/ROLLBACK, savepoints, failed-transaction state (25P02),
  READ COMMITTED (per-statement snapshots) and REPEATABLE READ/SERIALIZABLE (transaction
  snapshot), row-level write-conflict detection at commit (40001), non-transactional sequences.
- **Functions**: string, regex (`~`, regexp_* ), LIKE/ILIKE/SIMILAR TO, math, date/time (extract,
  date_trunc, age, make_*, to_char/to_date/to_timestamp, AT TIME ZONE for UTC and fixed offsets),
  jsonb operators and functions, array operators and functions, sequences, session information,
  gen_random_uuid.

## Known gaps

- Session time zone is fixed at UTC; named time zones are not supported.
- No `pg_catalog` / `information_schema` relations yet (drivers that introspect will fail).
- No COPY, LISTEN/NOTIFY, cursors (DECLARE/FETCH), prepared statements via SQL (PREPARE/EXECUTE),
  stored procedures, triggers, multidimensional arrays, range types, enums, domains, composite
  types, collations (comparison is bytewise, like the C collation).
- Concurrent writers never block: where Postgres would wait on a row lock, pgmem detects the
  conflict at COMMIT (40001).
- Extended-protocol pipelines autocommit per Execute rather than per Sync.

## Next

1. Conformance harness: sqllogictest corpus by area (wire, types, DDL, constraints, transactions,
   joins, aggregates, windows, CTEs, catalog, error codes), expected results recorded from and
   re-verified against PostgreSQL 18 in CI; track the pass rate here.
2. Client matrix in CI: psql, tokio-postgres, sqlx and JDBC (Go pgx is covered by `examples/`).
3. Hill-climb, highest value first: `pg_catalog`/`information_schema` for driver and ORM
   introspection, named time zones, COPY, LISTEN/NOTIFY.
