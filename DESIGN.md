# pgmem — design

pgmem is an AST-walking SQL engine behind a PostgreSQL wire-protocol server, built for tests:
it optimizes for behaving exactly like PostgreSQL 18, then for simplicity, and only then for
speed (every operator materializes its rows; there is no planner).

```
client ──wire──► pgmem-wire ──SQL──► pgmem-parse ──IR──► pgmem-exec ──► pgmem-storage
                  (protocol)          (sqlparser)          binder           rows, snapshots
                                                           executor         pgmem-catalog
                                                           builtins
```

## Parsing: sqlparser behind an IR

`pgmem-parse` lowers `sqlparser`'s PostgreSQL-dialect AST into `pgmem-ir`, a small SQL-shaped
tree. Lowering is where Postgres' surface rules live: unquoted identifiers fold to lower case,
special syntax becomes function calls the way Postgres' grammar does it (`EXTRACT`, `POSITION`,
`SUBSTRING`, `TRIM`), a minus sign folds into numeric literals, and syntax errors become `42601
syntax error at or near "..."`. Utility statements clients send at connect time and migration
tools emit (`SET` variants, `COMMENT ON`, `CREATE EXTENSION`, ...) are recognized even when
sqlparser can't parse them, and a token-level rewrite covers the few Postgres forms it lacks
(`FOR NO KEY UPDATE`, `DROP INDEX CONCURRENTLY`).

Options considered: porting pgmem-go's hand-written parser (exact parity, but a narrow grammar
every future feature would have to extend); `pg_query` (libpg_query — the real grammar, but its
build compiles Postgres' C sources and runs bindgen, a heavy requirement for a test dependency);
`sqlparser` (pure Rust, broad PostgreSQL coverage, widely used). The IR boundary keeps a
`pg_query` front end possible as an opt-in feature without touching the engine.

## Binding: Postgres' analyzer, simplified

`pgmem-exec::bind` resolves the IR against the catalog the way Postgres' parse analysis does:
scopes for each query level (with correlation tracking), column resolution with Postgres'
ambiguity rules and error codes, operator and function overload resolution with Postgres' type
promotion (unknown-typed literals and parameters take their type from context; numeric literals
are `int4`/`int8`/`numeric` by magnitude), and the SELECT pipeline: FROM → WHERE → GROUP BY /
aggregates (with the functional-dependency rule) → HAVING → windows → projection → ORDER BY
(output names, ordinals, hidden sort columns) → DISTINCT [ON] → LIMIT. Binding happens at Parse
time, so Describe knows parameter and result types and semantic errors surface where Postgres
reports them; Execute re-binds with the final parameter types (cheap, and always sees the current
schema).

## Execution

Plans (`plan.rs`) are trees of relational operators over materialized rows, with expressions
resolved to row positions and function pointers. Correlated subqueries and LATERAL items re-run
per outer row (an outer-row stack in the evaluation context); uncorrelated subqueries run once per
statement. DML computes all of a statement's effects against its snapshot first (a statement
never sees its own changes), then validates them against the table's final state — NOT NULL,
CHECK, unique indexes (including partial and expression indexes), foreign keys in both directions
with referential actions — and applies them.

## Storage and transactions

The committed database is an immutable `DbState` (catalog + table contents) behind an `Arc`;
readers never block writers. A transaction records a row-level overlay — inserted rows and deleted
row ids; an UPDATE is a delete plus an insert under a new row id, so updated rows move to the end
of a scan as in a Postgres heap — plus a private catalog copy once it runs DDL, which makes DDL
transactional. COMMIT replays the overlay onto the latest committed state under a short lock.

- READ COMMITTED takes a fresh snapshot per statement; REPEATABLE READ and SERIALIZABLE keep the
  snapshot of their first statement.
- Conflicts are per row: committing a delete/update of a row another transaction already
  changed fails with `40001` (where Postgres would have waited on the row lock).
- Savepoints snapshot the overlay; sequences are non-transactional, as in Postgres.
- Tables keep dropped columns' slots and give added columns a "missing" value (Postgres'
  `attisdropped` / `attmissingval`), so ADD/DROP COLUMN don't rewrite rows.

Serializable snapshot isolation (detecting read/write dependency cycles) is not implemented.

## Types

`pgmem-types` holds `Type` (with real OIDs, so `RowDescription` matches Postgres), runtime
`Value`s and both wire formats for every type. `numeric` is arbitrary precision with Postgres'
display-scale rules (`select_div_scale`, round half away from zero); date/time arithmetic is a
port of Postgres' own calendar code (`date2j`/`j2date`, `timestamp2tm`, ISO weeks), so ranges and
corner cases match; jsonb is normalized as Postgres stores it (deduplicated keys sorted by length,
Postgres' spacing). Output formats follow the settings pgmem reports: `DateStyle=ISO, MDY`,
`TimeZone=UTC`, `IntervalStyle=postgres`, `extra_float_digits=1`, `bytea_output=hex`.

## Wire protocol

`pgmem-wire` implements protocol 3.0 directly on tokio (one task per connection): startup with
trust authentication, simple query (multiple statements share an implicit transaction), extended
query with named statements and portals, and Postgres' error recovery (discard messages until
Sync). The message codec is hand-written — about 400 lines — which keeps full control over message
ordering, the part of the protocol drivers are most sensitive to.

## Testing strategy

Every integration test is written against a real client and runs twice in CI: against pgmem and
against PostgreSQL 18 (`PGMEM_TEST_DSN`). An assertion that doesn't hold on real Postgres is a
broken test. The pgmem-go suite was ported this way ([PARITY.md](PARITY.md)), and its sqlc + pgx
examples run against the `pgmem` binary in CI. Phase 2 adds a
sqllogictest conformance corpus whose expected results are recorded from PostgreSQL
([PROGRESS.md](PROGRESS.md)).
