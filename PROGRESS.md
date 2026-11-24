# Progress

Where pgmem stands against its goal — behave like PostgreSQL 18 for anything a test can observe —
and what comes next. Updated with every PR.

## Metrics

| Metric | Value | How it is measured |
|---|---|---|
| pgmem-go parity | **564 / 564** Go tests ported | `scripts/parity.py check` ([PARITY.md](PARITY.md)) |
| Integration suite on pgmem | **526 / 526** pass | `cargo nextest run -p pgmem --test it` |
| Integration suite on real PostgreSQL 18 | **526 / 526** pass | same suite with `PGMEM_TEST_DSN` (CI `postgres-fidelity`) |
| Go pgx example suites (sqlc) against the Rust binary | **27 / 27** pass | `make examples` (CI `pgx-examples`) |
| **SQL conformance corpus** (3309 records, 9 areas) | **2132 / 3309 (64.4%)** on pgmem; 3309 / 3309 on PostgreSQL 18 | `cargo run -p pgmem-conformance` ([CONFORMANCE.md](CONFORMANCE.md); CI `conformance`, `postgres-fidelity`) |

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
| 2026-11-24 | #112 conformance corpus and runner (baseline) | 564/564 | 2031/3309 (61.4%) |
| 2026-11-24 | varchar/bpchar/name/"char"/json/oid/reg*/void types | 564/564 | 2132/3309 (64.4%) |

## Supported

- **Wire protocol**: protocol 3.0 (3.x and `_pq_` options negotiated down), trust auth, SSL/GSS
  requests refused, simple query (multi-statement implicit transactions), extended query
  (Parse/Bind/Describe/Execute/Close/Sync/Flush, named statements and portals, row-limited
  Execute with PortalSuspended), skip-until-Sync after errors, ParameterStatus, notices, error
  fields (SQLSTATE, detail, hint, schema/table/column/constraint).
- **Types**: bool, int2/4/8, float4/8, numeric (arbitrary precision, Postgres scale rules),
  text, varchar(n), char(n) (blank-padded, trailing spaces insignificant), name, "char", bytea,
  uuid, date, time, timetz, timestamp, timestamptz, interval, json (input text kept as written),
  jsonb, oid, regclass/regtype/regnamespace/regproc (name ↔ OID through the catalog), void,
  one-dimensional arrays; text and binary formats; Postgres' result-column naming for casts.
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

Hill-climb, ordered by conformance records recovered per unit of work and by what blocks real
clients. Each item names the corpus area it moves (see CONFORMANCE.md for per-file rates).

1. **System catalogs**: `pg_catalog` (pg_class, pg_namespace, pg_attribute, pg_type, pg_index,
   pg_constraint, pg_attrdef, pg_proc, pg_description, pg_settings, pg_sequences, pg_views,
   pg_tables, pg_indexes, pg_roles, pg_extension) and `information_schema` (tables, columns,
   constraints, key/constraint column usage, views, sequences); `reg*` types and `to_reg*`,
   `pg_get_*def`, `format_type`, `pg_get_serial_sequence`. Blocks ORM/driver introspection and
   array decoding in drivers that look up `pg_type` (catalog 23%, ddl, types).
2. **JSON fidelity**: json-typed results from `json_build_object`/`to_json`/`json_agg`/
   `row_to_json`/`json_array_elements` with Postgres' json formatting; SQL/JSON (`IS JSON`,
   `JSON_VALUE`, `JSON_TABLE`), jsonpath.
3. **Named time zones**: `SET TIME ZONE`, `AT TIME ZONE 'Region/City'`, zone names in input.
4. **Set-returning functions in the select list**, multidimensional arrays, array element
   assignment, `ROW(...)` constructors and row comparisons.
5. **Settings fidelity**: validation (22023/42704), value normalisation, custom `x.y` settings,
   `SET LOCAL`/savepoint scoping, DateStyle/IntervalStyle output styles.
6. **Statements**: SQL-level PREPARE/EXECUTE, cursors, LISTEN/NOTIFY, MERGE, writable CTEs,
   `RETURNING old/new`, GROUPING SETS/ROLLUP/CUBE, ordered-set aggregates, window `RANGE` offsets
   and `EXCLUDE`, CTE `SEARCH`/`CYCLE`.
7. **Types**: enums, domains, composite types, range types (and EXCLUDE constraints).
8. **Transactions and constraints**: deferred constraints, per-row unique checks, row locks with
   NOWAIT/SKIP LOCKED between sessions, advisory locks shared across sessions, READ ONLY
   transactions, SERIALIZABLE write-skew detection, dependency errors (2BP01).
9. **Functions/procedures/triggers** (SQL and PL/pgSQL), `DO` blocks — large; most of ddl/migrations.
10. Client matrix in CI: psql, tokio-postgres, sqlx and JDBC (Go pgx is covered by `examples/`).

Known wrong result (not just unsupported): `FULL JOIN ... USING (k)` yields NULL for the merged
column on right-only rows (queries/joins.slt).
