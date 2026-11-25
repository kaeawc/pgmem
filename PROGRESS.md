# Progress

Where pgmem stands against its goal — behave like PostgreSQL 18 for anything a test can observe —
and what comes next. Updated with every PR.

## Metrics

| Metric | Value | How it is measured |
|---|---|---|
| pgmem-go parity | **564 / 564** Go tests ported | `scripts/parity.py check` ([PARITY.md](PARITY.md)) |
| Integration suite on pgmem | **636 / 636** pass | `cargo nextest run -p pgmem --test it` |
| Integration suite on real PostgreSQL 18 | **636 / 636** pass | same suite with `PGMEM_TEST_DSN` (CI `postgres-fidelity`) |
| Go pgx example suites (sqlc) against the Rust binary | **27 / 27** pass | `make examples` (CI `pgx-examples`) |
| **SQL conformance corpus** (3309 records, 9 areas) | **3240 / 3309 (97.9%)** on pgmem; 3309 / 3309 on PostgreSQL 18 | `cargo run -p pgmem-conformance` ([CONFORMANCE.md](CONFORMANCE.md); CI `conformance`, `postgres-fidelity`) |

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
| 2026-11-24 | #113 varchar/bpchar/name/"char"/json/oid/reg*/void types | 564/564 | 2132/3309 (64.4%) |
| 2026-11-24 | #114 pg_catalog / information_schema, COMMENT ON, deparser | 564/564 | 2238/3309 (67.6%) |
| 2026-11-24 | #115 session time zones, zone-aware output, AT TIME ZONE | 564/564 | 2276/3309 (68.8%) |
| 2026-11-24 | #119 enums, domains and composite types | 564/564 | 2368/3309 (71.6%) |
| 2026-11-24 | #120 FULL JOIN USING, set-op typing, WITH TIES, numeric infinity, bytea/uuid functions | 564/564 | 2429/3309 (73.4%) |
| 2026-11-24 | schemas, search_path, temporary tables | 564/564 | 2467/3309 (74.6%) |
| 2026-11-24 | #122 json fidelity, jsonpath, SQL/JSON | 564/564 | 2518/3309 (76.1%) |
| 2026-11-24 | #123 arrays, select-list SRFs, settings, output styles, date/time input, function sweep | 564/564 | 2711/3309 (81.9%) |
| 2026-11-25 | #124 session statements: PREPARE/EXECUTE, cursors, LISTEN/NOTIFY, DISCARD, utility statements, transactions per Sync | 564/564 | 2809/3309 (84.9%) |
| 2026-11-25 | #125 query statements: MERGE, writable CTEs, RETURNING old/new, GROUPING SETS, window RANGE offsets / EXCLUDE, SEARCH / CYCLE | 564/564 | 2880/3309 (87.0%) |
| 2026-11-25 | #126 system columns, row / table / advisory locks, READ ONLY, AND CHAIN, WHERE CURRENT OF | 564/564 | 2944/3309 (89.0%) |
| 2026-11-25 | #127 deferrable constraints, SET CONSTRAINTS, NOT VALID, FK fidelity, SERIALIZABLE write skew | 564/564 | 3022/3309 (91.3%) |
| 2026-11-25 | #128 updatable views, materialized views, DROP / ALTER dependencies, CREATE TABLE AS / LIKE, ON COMMIT, index and constraint renames | 564/564 | 3089/3309 (93.4%) |
| 2026-11-25 | #129 sequence parameters and ALTER SEQUENCE, identity options and ALTER ... IDENTITY, generated column rules | 564/564 | 3130/3309 (94.6%) |
| 2026-11-25 | #130 range and multirange types, EXCLUDE constraints, WITHOUT OVERLAPS keys | 564/564 | 3165/3309 (95.6%) |
| 2026-11-25 | #131 array lower bounds, bit strings, money, inet/cidr/macaddr, pretty deparse | 564/564 | 3183/3309 (96.2%) |
| 2026-11-25 | SQL and PL/pgSQL functions and procedures, DO, CALL, triggers | 564/564 | 3240/3309 (97.9%) |

## Supported

- **Wire protocol**: protocol 3.0 (3.x and `_pq_` options negotiated down), trust auth, SSL/GSS
  requests refused, simple query (multi-statement implicit transactions), extended query
  (Parse/Bind/Describe/Execute/Close/Sync/Flush, named statements and portals, row-limited
  Execute with PortalSuspended), one implicit transaction per Sync (an error rolls back the
  whole pipeline), skip-until-Sync after errors, ParameterStatus, notices,
  NotificationResponse (delivered between transactions, also to idle connections), error
  fields (SQLSTATE, detail, hint, schema/table/column/constraint).
- **Session statements**: PREPARE / EXECUTE (assignment coercion of arguments) / DEALLOCATE,
  sharing one namespace with protocol statements, and `pg_prepared_statements`; cursors
  (DECLARE [BINARY] [NO] SCROLL [WITH HOLD], FETCH / MOVE in every direction with Postgres'
  positioning rules, CLOSE, `pg_cursors` incl. protocol portals; closed at transaction end or
  ROLLBACK TO); LISTEN / UNLISTEN / NOTIFY and `pg_notify` (sent at commit, folded within a
  transaction, `pg_listening_channels`); DISCARD ALL / PLANS / SEQUENCES / TEMP; EXPLAIN
  [ANALYZE] (any format; ANALYZE runs the statement); VACUUM / ANALYZE / REINDEX / CLUSTER /
  CHECKPOINT / LOCK with their existence and transaction-block checks.
- **Types**: bool, int2/4/8, float4/8, numeric (arbitrary precision, Postgres scale rules),
  text, varchar(n), char(n) (blank-padded, trailing spaces insignificant), name, "char", bytea,
  uuid, date, time, timetz, timestamp, timestamptz, interval, json (input text kept as written;
  operators return pieces of it verbatim), jsonb (numbers normalized as numeric), jsonpath
  (canonical text form), oid, regclass/regtype/regnamespace/regproc (name ↔ OID through the catalog), void,
  enums, domains (NOT NULL / CHECK), composite types and table row types, the builtin range and
  multirange types (int4/int8/num/date/ts/tstz; discrete ranges canonical, text and binary I/O,
  ordering, `@>` `<@` `&&` `<<` `>>` `&<` `&>` `-|-` `+` `*` `-`, lower/upper/isempty/..._inc/
  ..._inf, range_merge, range_agg, range_intersect_agg), bit / bit varying (B'' and X''
  constants, length rules, & | # || << >>, get/set_bit), money (lc_monetary C), inet / cidr
  (classful cidr input, network functions, containment operators, network_cmp ordering),
  macaddr / macaddr8, multidimensional
  arrays (subscripts, slices, element and slice assignment in UPDATE; lower bounds other than
  1: `'[2:4]={...}'`, array_fill, array_lower/upper/dims, generate_subscripts); text and binary formats; Postgres' result-column naming for casts. numeric has NaN
  and ±Infinity and accepts negative / large scales (`numeric(5,-2)`); `bytea_output` hex or
  escape.
- **Queries**: joins (inner/left/right/full/cross, USING with `AS alias`, NATURAL, LATERAL,
  aliased parenthesized joins), subqueries (scalar, IN incl. row-valued, EXISTS, ANY/ALL with any
  boolean operator, ARRAY(), correlated), WITH and WITH RECURSIVE, UNION/INTERSECT/EXCEPT [ALL],
  DISTINCT, DISTINCT ON, GROUP BY (ordinal, alias, functional dependency on the primary key),
  HAVING, ORDER BY (ordinal, alias, expression, NULLS FIRST/LAST), LIMIT/OFFSET/FETCH (WITH
  TIES), `TABLE t`, BETWEEN SYMMETRIC, VALUES, set-returning functions in FROM and in the select
  list (run in lockstep, as Postgres 10+ does), views, `(a, b) OVERLAPS (c, d)`, GROUPING SETS /
  ROLLUP / CUBE with `GROUPING()`, recursive CTEs with SEARCH DEPTH / BREADTH FIRST and CYCLE.
- **Aggregates & windows**: count/sum/avg/min/max/bool_and/bool_or/every/string_agg/array_agg/
  jsonb_agg/jsonb_object_agg/stddev/variance/bit_and/bit_or/bit_xor, corr/covar/regr_*
  (Postgres' Youngs-Cramer accumulation), ordered-set (`percentile_cont`, `percentile_disc`,
  `mode`) and hypothetical-set (`rank(..) WITHIN GROUP`, ...) aggregates, with DISTINCT, FILTER
  and ORDER BY;
  row_number/rank/dense_rank/percent_rank/cume_dist/ntile/lag/lead/first_value/last_value/
  nth_value and aggregates over windows with ROWS/RANGE/GROUPS frames (RANGE offsets over
  numeric and date/time keys), EXCLUDE CURRENT ROW / GROUP / TIES, aggregates inside window
  calls (`sum(sum(x)) OVER ()`).
- **DML**: INSERT (VALUES, SELECT, DEFAULT VALUES, DEFAULT), ON CONFLICT DO NOTHING/DO UPDATE with
  arbiter inference (partial unique indexes need the matching WHERE), UPDATE ... FROM,
  `SET (a, b) = (SELECT ...)`, DELETE ... USING, RETURNING with Postgres 18's `old` / `new`
  (and `RETURNING WITH (OLD AS o, NEW AS n)`), MERGE (MATCHED / NOT MATCHED [BY SOURCE |
  TARGET], DO NOTHING, RETURNING with `merge_action()`), data-modifying WITH queries (run
  once, in order, all reading the statement's snapshot), TRUNCATE (CASCADE; RESTART IDENTITY is
  undone on rollback).
- **Schemas**: CREATE SCHEMA (with embedded elements, AUTHORIZATION), ALTER SCHEMA RENAME,
  DROP SCHEMA [CASCADE], `ALTER {TABLE|VIEW|SEQUENCE|TYPE|DOMAIN} ... SET SCHEMA`; qualified
  names everywhere; `search_path` resolution with `$user`, implicit pg_catalog and the creation
  schema (3F000 when none); views resolve names as at creation; session-private temporary
  tables in `pg_temp_N` that shadow permanent ones and go away with the session; catalogs,
  `regclass`/`regtype` output and deparsed definitions qualify names not on the search path.
- **DDL**: CREATE/DROP TABLE, VIEW, INDEX (unique and partial unique indexes are enforced,
  NULLS NOT DISTINCT, access methods checked, CONCURRENTLY outside blocks only), SEQUENCE;
  CREATE TABLE AS / SELECT INTO (column lists, WITH NO DATA), `LIKE source [INCLUDING ...]`
  (defaults, constraints, indexes, identity, generated, comments), UNLOGGED, temporary tables'
  ON COMMIT DELETE ROWS / DROP; ALTER TABLE (add/drop/rename column, set/drop default and NOT
  NULL, alter type (42804 without an assignment cast), add/drop/rename constraint, `UNIQUE /
  PRIMARY KEY USING INDEX`, rename table), ALTER INDEX RENAME; DROP checks dependents (views on
  tables, views and columns, defaults on sequences, constraints on indexes: 2BP01, CASCADE drops
  them); no-op acceptance of extensions, GRANT. DDL is transactional.
- **Views**: automatically updatable simple views (INSERT / UPDATE / DELETE rewritten to the
  base table, nested views, Postgres' "not automatically updatable" details), WITH [CASCADED |
  LOCAL] CHECK OPTION (44000), CREATE OR REPLACE rules (42P16), ALTER VIEW RENAME; materialized
  views (WITH [NO] DATA, REFRESH [CONCURRENTLY] with its unique-index rule, 55000 when not
  populated, `pg_matviews`, relkind `m`).
- **Constraints**: NOT NULL (named, Postgres 18 style), CHECK, PRIMARY KEY, UNIQUE (NULLS NOT
  DISTINCT), FOREIGN KEY (multi-column, MATCH SIMPLE / FULL; NO ACTION / RESTRICT (23001) /
  CASCADE / SET NULL / SET DEFAULT on delete and update, `SET NULL (cols)`), serial and identity
  columns, generated columns; Postgres constraint naming.
  Sequences take every CREATE / ALTER SEQUENCE parameter (AS type, INCREMENT, MIN/MAXVALUE,
  START, RESTART, CACHE, CYCLE, OWNED BY, RENAME) with Postgres' validation, and read as a
  relation (`SELECT last_value, is_called FROM seq`); identity columns take sequence options and
  `ALTER COLUMN ... ADD / SET GENERATED, SET option, RESTART, DROP IDENTITY`, and `OVERRIDING
  USER VALUE`; generated columns are checked (immutable, no other generated column), may be
  added to filled tables, and take `SET / DROP EXPRESSION`; dropping a column they read needs
  CASCADE. DEFERRABLE / INITIALLY DEFERRED
  foreign keys and unique constraints with `SET CONSTRAINTS` (checked at commit or `...
  IMMEDIATE`; a failed COMMIT rolls back), non-deferrable unique checks row by row (UPDATE in
  scan order), deferrable ones at statement end; `ADD CONSTRAINT ... NOT VALID`, `VALIDATE
  CONSTRAINT`, `ALTER CONSTRAINT ... DEFERRABLE`; EXCLUDE constraints (any operator, WHERE,
  DEFERRABLE, gist operator-class checks) and Postgres 18 `PRIMARY KEY / UNIQUE (..., c WITHOUT
  OVERLAPS)`; `pg_constraint` / `information_schema` /
  `pg_get_constraintdef` report all of it.
- **Functions and triggers**: CREATE [OR REPLACE] FUNCTION / PROCEDURE in SQL (statements or a
  SQL-standard `RETURN` / `BEGIN ATOMIC` body) and PL/pgSQL, with Postgres' OR REPLACE rules,
  overloads and resolution, named and `$n` arguments, defaults, STRICT, OUT parameters,
  `RETURNS SETOF` / `TABLE (...)` (in FROM), DROP (by signature, dependencies, CASCADE), CALL,
  DO, `COMMENT ON FUNCTION`; PL/pgSQL blocks with DECLARE (`%TYPE`, `%ROWTYPE`, record,
  CONSTANT, NOT NULL), IF / CASE, LOOP / WHILE / FOR (integer, query, EXECUTE) / FOREACH with
  labels, EXIT / CONTINUE, RETURN / RETURN NEXT / RETURN QUERY, SELECT / DML ... INTO [STRICT],
  PERFORM, EXECUTE ... USING / INTO, GET DIAGNOSTICS, FOUND, RAISE (levels, `%` formatting,
  USING options, conditions), ASSERT, and EXCEPTION handlers (condition names, SQLSTATE,
  SQLSTATE / SQLERRM, the block's changes undone); triggers BEFORE / AFTER / INSTEAD OF, row and
  statement level, `UPDATE OF`, WHEN, TG_* variables, NEW / OLD, ENABLE / DISABLE; `pg_proc`,
  `pg_trigger`, `information_schema.routines` / `parameters` / `triggers`,
  `pg_get_function_arguments` / `_result` / `_identity_arguments`, `pg_get_functiondef`,
  `pg_get_triggerdef`.
- **Transactions**: BEGIN/COMMIT/ROLLBACK [AND [NO] CHAIN], savepoints, failed-transaction state
  (25P02), READ COMMITTED (per-statement snapshots) and REPEATABLE READ/SERIALIZABLE
  (transaction snapshot; updating, deleting or locking a row changed since the snapshot fails
  with 40001 at the statement; SERIALIZABLE write skew between two transactions fails the second
  COMMIT), READ ONLY transactions (`BEGIN READ ONLY`, `SET TRANSACTION`,
  `default_transaction_read_only`; 25006, temporary tables stay writable), non-transactional
  sequences.
- **Locks**: `SELECT ... FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE [OF t] [NOWAIT | SKIP
  LOCKED]` with Postgres' conflict table between sessions (LIMIT counts locked rows), the row
  locks UPDATE (NO KEY UPDATE unless a key changes) and DELETE take, ROLLBACK TO releasing later
  locks, `LOCK TABLE ... IN mode MODE [NOWAIT]`; advisory locks shared by every session
  (session / transaction level, shared / exclusive, re-entrant, blocking with `lock_timeout`,
  `pg_locks`), `hashtext` (Postgres' `hash_bytes`).
- **System columns**: `ctid`, `xmin`, `cmin`, `xmax`, `cmax`, `tableoid` on table scans and
  UPDATE / DELETE / RETURNING (`tid`, `xid`, `cid` types; `xmax = 0` tells an upsert's inserted
  rows from updated ones), `WHERE CURRENT OF` for FOR UPDATE cursors and plain table scans.
- **Settings**: every parameter of Postgres 18's `pg_settings` with its validation (bool, integer
  and real ranges with units, enum spellings, DateStyle), context rules (55P02), `SHOW` /
  `current_setting` unit formatting, custom `x.y` placeholders, `set_config`, SET LOCAL, undo
  at ROLLBACK and ROLLBACK TO SAVEPOINT; DateStyle (ISO/SQL/Postgres/German with field order),
  IntervalStyle (postgres, postgres_verbose, sql_standard, iso_8601) and extra_float_digits
  shape output.
- **Catalogs**: `pg_catalog` (pg_class, pg_attribute, pg_type, pg_namespace, pg_index,
  pg_constraint incl. Postgres 18 NOT NULL constraints, pg_attrdef, pg_description, pg_depend,
  pg_sequence(s), pg_tables, pg_views, pg_indexes, pg_settings, pg_roles, pg_database, ...) and
  `information_schema` (tables, columns, table_constraints, key_column_usage,
  referential_constraints, constraint_column_usage, check_constraints, views, sequences,
  privileges), computed from the live catalog; `format_type`, `pg_get_expr`,
  `pg_get_constraintdef`, `pg_get_indexdef`, `pg_get_viewdef` (ruleutils-exact deparser, with
  pretty mode's minimal parentheses),
  `pg_get_serial_sequence`, `obj_description`/`col_description` (COMMENT ON is stored),
  `to_regclass`, privilege checks; int2vector/oidvector with zero-based subscripts.
- **Functions**: string (format with `*` widths, VARIADIC calls, unistr, to_bin/to_oct,
  string_to_table, SIMILAR substring), regex (`~`, regexp_match(es)/replace/count/instr/substr/
  split with Postgres' flags and empty-match rules), LIKE/ILIKE/SIMILAR TO, math (numeric
  exp/ln/log/power with Postgres' result scales, degree trigonometry, hyperbolic functions),
  date/time (Postgres' full date/time input parser with DateStyle field order, month/day names,
  zone names and abbreviations, `now`/`today`/`allballs`; fractional-second typmods; infinite
  intervals; extract, date_trunc, age, make_* with named arguments, to_char for numbers
  (`formatting.c` port), to_number, to_date/to_timestamp templates, AT TIME ZONE),
  json/jsonb operators and functions with Postgres' json formatting (`json_build_object`,
  `row_to_json`, `json_agg`, ...), jsonb subscripting, jsonpath (lax/strict, filters, methods,
  `like_regex`, variables; `@?`, `@@`, `jsonb_path_*`), SQL/JSON (`IS JSON`, `JSON_OBJECT`,
  `JSON_ARRAY`, `JSON_ARRAYAGG`, `JSON_OBJECTAGG`, `JSON_VALUE`, `JSON_QUERY`, `JSON_EXISTS`,
  `JSON_TABLE`, `JSON()`), `json(b)_populate_record(set)` / `json(b)_to_record(set)` with column
  definition lists, array operators and functions, bytea (get/set_byte/bit,
  convert_to/from), sequences, session information, gen_random_uuid/uuidv4/uuidv7 and
  uuid_extract_version/timestamp.

## Known gaps

- VIRTUAL generated columns (Postgres 18's default) are computed on write, like STORED; the
  values read back are the same.
- WITH LOCAL CHECK OPTION checks like CASCADED; views with INSTEAD OF triggers or rules are not
  supported (no triggers yet).
- Functions called from a query (and BEFORE row triggers) run read-only: data-modifying
  statements in them fail (0A000); CALL, DO and AFTER / statement triggers may modify data.
  Set-returning user functions work in FROM, not in the select list. No transaction control
  inside procedures, no event triggers, no cursors in PL/pgSQL; errors carry no CONTEXT field.
- `pg_proc` has no rows for builtin functions; `regproc` / `regprocedure` do not resolve
  builtin function OIDs.
- Multiranges have I/O, constructors, `@>`, `&&`, lower/upper and range_agg, not the full
  operator set. No user-defined range types.
- No COPY, stored procedures, triggers. Functions that build arrays from bounded ones (`||`,
  slices, element assignment) return lower bound 1. Unicode
  normalization (`normalize`, `IS NORMALIZED`) covers ASCII text only. `VARIADIC` needs an
  array known when the statement is bound. Comparison is bytewise, which is what the corpus' `C.UTF-8`
  database collation does; other collations are not supported.
- `ctid` and `xmin` derive from pgmem's row ids (they change with every row version, but xmin is
  not the inserting transaction's id); ON CONFLICT targets must be columns (not expressions).
  UNION without ORDER BY returns rows in first-seen order where Postgres' hash order can differ.
- SERIALIZABLE tracks reads and writes per table (Postgres' relation-level predicate locks of
  sequential scans): it catches two-transaction write skew, not longer dependency cycles, and
  can fail where index-level predicate locks would not.
- Row and table locks never block: NOWAIT fails and SKIP LOCKED skips, but where Postgres would
  wait, pgmem proceeds and a write-write conflict surfaces at COMMIT (40001). Only explicit
  LOCK TABLE takes table locks. Advisory locks do block.
- Cursors are materialized at DECLARE (volatile functions run then, not at FETCH); no
  `FOR UPDATE` cursors or `WHERE CURRENT OF`. EXPLAIN reports a single `Result` node: pgmem
  has no planner.

## Next

Hill-climb, ordered by conformance records recovered per unit of work and by what blocks real
clients. Each item names the corpus area it moves (see CONFORMANCE.md for per-file rates).

1. Roles, extensions (pgcrypto, uuid-ossp, citext), COPY, collations, catalog remainder.
2. Client matrix in CI: psql, tokio-postgres and sqlx (Go pgx is covered by `examples/`).
