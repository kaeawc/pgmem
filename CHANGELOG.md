# Changelog

Notable changes to pgmem. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

- Static Linux binaries (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) on each release.
- `cargo binstall pgmem` installs the prebuilt binary from the GitHub release.
- `pg_get_keywords()` returns PostgreSQL 18's keyword list (word, catcode, barelabel, catdesc, baredesc).
- Multirange `+`, `*` and `-`, the range/multirange forms of `@>`, `<@`, `<<`, `>>`, `&<`, `&>` and `-|-`, `unnest(multirange)`, `range_cmp` / `multirange_cmp` and the `range::multirange` cast (part of #116).

### Fixed

- Errors carry PostgreSQL's `position` field (`P`, the 1-based character offset into the query string, shown by psql as `LINE n:` with a caret) for syntax errors and for undefined columns, relations and functions, missing FROM-clause entries, ambiguous columns, undefined operators and invalid literal input, in simple and extended (`Parse`) queries; a statement of a multi-statement query is located in the whole string. Errors inside function and `DO` bodies carry none.
- Range types follow PostgreSQL more closely (part of #116): `daterange` keeps `infinity` bounds inclusive, `@>` / `<@` with an element need the exact subtype and range constructors take only implicit casts, `min` / `max` reject ranges, `range || text` concatenates, and `pg_range.rngsubopc` / `rngsubdiff` and the range rows' `pg_type.typalign` / `typanalyze` match PostgreSQL.
- Function attributes are stored and shown: `SECURITY DEFINER`, `SET param = value` (`pg_proc.proconfig`), `PARALLEL`, `COST`, `ROWS` and `LEAKPROOF` appear in `pg_proc` and `pg_get_functiondef` as PostgreSQL prints them (procedures use `$procedure$`), and a function's `SET` options apply during its call.
- `ALTER FUNCTION / PROCEDURE / ROUTINE` supports `SET` / `RESET`, `[EXTERNAL] SECURITY`, volatility, `[NOT] LEAKPROOF`, `STRICT` / `CALLED ON NULL INPUT`, `PARALLEL`, `COST`, `ROWS`, `RENAME TO` and `SET SCHEMA`.
- `ALTER TABLE ... ALTER COLUMN SET STATISTICS | STORAGE | COMPRESSION`, `REPLICA IDENTITY`, `CLUSTER ON`, `SET WITHOUT CLUSTER` and `SET LOGGED | UNLOGGED` are accepted with PostgreSQL's validation and errors, and show up in `pg_attribute` (`attstattarget`, `attstorage`, `attcompression`), `pg_class` (`relreplident`, `relpersistence`) and `pg_index` (`indisreplident`, `indisclustered`); `ALTER COLUMN TYPE` resets storage and compression. `SET LOGGED | UNLOGGED` were silently ignored before.
- `pg_constraint.connamespace` is the namespace of the owning table or domain instead of always `public`.
- `pg_get_indexdef` and `pg_indexes.indexdef` print `INCLUDE (...)` for covering indexes, and `pg_index.indnatts`, `indnkeyatts`, `indkey` and `pg_class.relnatts` count the INCLUDE columns like PostgreSQL.
- Storage parameters are stored and validated like PostgreSQL: `WITH (...)` on tables, indexes, views and materialized views, `ALTER TABLE / INDEX / VIEW / MATERIALIZED VIEW ... SET / RESET (...)`, `pg_class.reloptions`, `pg_get_indexdef` (`WITH (fillfactor='70')`) and `information_schema.views.check_option` (including `WITH CHECK OPTION` and `check_option`). `security_invoker` is stored but not yet enforced.
- Row triggers on a partitioned table are cloned to its partitions (existing, created, attached and sub-partitions; `pg_trigger.tgparentid`) and fire on the leaf partition. The clones follow the parent trigger through `DROP TRIGGER`, `ALTER TRIGGER ... RENAME`, `ENABLE` / `DISABLE` and `CREATE OR REPLACE TRIGGER`, cannot be dropped or renamed on their own, and go with `DETACH PARTITION`. Statement triggers of a partitioned table fire for statements on it, and its partitions' statement triggers no longer do.
- `pg_depend` records the dependencies Postgres records for user objects (objects on their schema,
  row and array types, view and materialized-view rules on the columns, functions and types they
  use, constraints and indexes on their columns, foreign keys on the referenced index, defaults on
  sequences and columns, `OWNED BY` sequences, triggers, partitions, column and function types,
  domains). New `pg_rewrite` (one `_RETURN` rule per view and materialized view; `ev_action` holds
  the view's SQL text, not Postgres' node tree), `pg_class.relhasrules`, row-type array types in
  `pg_type`, and `pg_describe_object`, `pg_identify_object`, `pg_identify_object_as_address`.
  Toast tables and the internal triggers of foreign keys are not modelled.
- Volatile functions may modify data wherever they are called, as in PostgreSQL: BEFORE row
  triggers, and functions called from SELECT lists, WHERE clauses, VALUES and column defaults
  (the calling statement keeps its snapshot). Stable and immutable functions get PostgreSQL's
  `0A000` "INSERT is not allowed in a non-volatile function" (also UPDATE, DELETE, DROP TABLE,
  ...). Writes made by functions, triggers, CALL and DO in a READ ONLY transaction raise `25006`,
  and a PL/pgSQL exception block undoes the writes of the functions it called.
- `CREATE TEMP TABLE` in a READ ONLY transaction raises `25006`, as in PostgreSQL.
- `CREATE FUNCTION / TYPE / VIEW / SEQUENCE / DOMAIN pg_temp.x` creates the session's temporary
  schema on demand; temporary views, sequences, composite types and indexes report
  `relpersistence` `t`.
- UNION / INTERSECT / EXCEPT and `SELECT DISTINCT` without ORDER BY return rows in PostgreSQL's
  order (sorted, or hash-table order, as its planner picks) for inputs of constants and VALUES
  lists, instead of first-seen order. Table inputs keep first-seen order.

### Changed

- Releases go through a PR: `scripts/release.sh X.Y.Z` opens it, and merging it publishes.
- Publishing skips crates already on crates.io, so a failed release can be rerun.
- Release notes come from this changelog.

### Fixed

- `pg_get_triggerdef` schema-qualifies the trigger function when its schema is not on `search_path`, and quotes its name.
- `ALTER TRIGGER ... RENAME TO`, and `ALTER TABLE ... ENABLE REPLICA | ALWAYS TRIGGER` (`pg_trigger.tgenabled` `O`/`D`/`R`/`A`); triggers fire according to `session_replication_role`.
- `CREATE TABLE ... PARTITION OF` builds the partition from the parent's live columns only, as
  PostgreSQL does: a partition created after a parent `DROP COLUMN` no longer copies the dropped
  slot, so its `pg_attribute.attnum`, `information_schema.columns.ordinal_position`,
  `pg_index.indkey` and constraint `conkey` match PostgreSQL's.
- `GRANT` / `REVOKE` with a column list (`GRANT SELECT (id, email) ON t TO r`) grant column
  privileges instead of the table privilege: they set `pg_attribute.attacl` (not `relacl`), show
  in `information_schema.column_privileges` / `role_column_grants` and `has_column_privilege` /
  `has_any_column_privilege`, and are enforced: a role may read, insert and update only the
  granted columns (`SELECT *` fails with 42501), also through a view used as the INSERT / UPDATE
  target. The target table's own columns read by WHERE, RETURNING and ON CONFLICT of INSERT /
  UPDATE / DELETE now need SELECT, also with table-level grants, as in PostgreSQL. Other tables
  read by UPDATE ... FROM, DELETE ... USING, subqueries in their WHERE, data-modifying CTEs and
  MERGE are still not checked for SELECT. `ALTER DEFAULT PRIVILEGES` with a column list fails
  with 0LP01, as in PostgreSQL.
- `RETURNING` of a row that `UPDATE ... FROM` moves to another partition can reference the FROM
  tables, and its `old` values are the row before the update (it failed with `42P01`).

## [0.1.1] - 2026-11-25

### Changed

- First release built, published and tagged by the release workflow (crates.io trusted publishing).
- Dropped the redundant `package.homepage` field.

## [0.1.0] - 2026-11-25

### Added

- First crates.io release: `pgmem` (library and `pgmem` server binary) and its internal crates
  `pgmem-types`, `pgmem-ir`, `pgmem-parse`, `pgmem-catalog`, `pgmem-storage`, `pgmem-exec`,
  `pgmem-wire`.

[Unreleased]: https://github.com/kaeawc/pgmem/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/kaeawc/pgmem/releases/tag/v0.1.1
[0.1.0]: https://crates.io/crates/pgmem/0.1.0
