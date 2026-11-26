# Changelog

Notable changes to pgmem. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

- Static Linux binaries (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) on each release.
- `cargo binstall pgmem` installs the prebuilt binary from the GitHub release.
- `pg_get_keywords()` returns PostgreSQL 18's keyword list (word, catcode, barelabel, catdesc, baredesc).
- Database forks: `Embedded::fork()` and `Server::fork()` copy the committed state (catalog, rows, roles and grants, sequence values, pinned `now()`) cheaply, by sharing it copy-on-write; the copies are independent afterwards.
- In-process embedding API: `pgmem::Embedded` → `session()` with `execute_batch(sql)` (simple-query semantics: implicit transaction block, rollback on error), `query(sql)` (column names and type OIDs, rows as Postgres text) and `DbError` (SQLSTATE, message, detail, hint, object fields). `pgmem::to_pg_text` renders a value in Postgres text form. `Server::embedded()` and `Builder::start_on(&Embedded)` connect the two.
- The wire server is behind the default-on `server` feature: `pgmem = { default-features = false }` builds the embedding API without tokio.

### Fixed

- Function attributes are stored and shown: `SECURITY DEFINER`, `SET param = value` (`pg_proc.proconfig`), `PARALLEL`, `COST`, `ROWS` and `LEAKPROOF` appear in `pg_proc` and `pg_get_functiondef` as PostgreSQL prints them (procedures use `$procedure$`), and a function's `SET` options apply during its call.
- `ALTER FUNCTION / PROCEDURE / ROUTINE` supports `SET` / `RESET`, `[EXTERNAL] SECURITY`, volatility, `[NOT] LEAKPROOF`, `STRICT` / `CALLED ON NULL INPUT`, `PARALLEL`, `COST`, `ROWS`, `RENAME TO` and `SET SCHEMA`.
- `pg_constraint.connamespace` is the namespace of the owning table or domain instead of always `public`.
- `pg_get_indexdef` and `pg_indexes.indexdef` print `INCLUDE (...)` for covering indexes, and `pg_index.indnatts`, `indnkeyatts`, `indkey` and `pg_class.relnatts` count the INCLUDE columns like PostgreSQL.
- Storage parameters are stored and validated like PostgreSQL: `WITH (...)` on tables, indexes, views and materialized views, `ALTER TABLE / INDEX / VIEW / MATERIALIZED VIEW ... SET / RESET (...)`, `pg_class.reloptions`, `pg_get_indexdef` (`WITH (fillfactor='70')`) and `information_schema.views.check_option` (including `WITH CHECK OPTION` and `check_option`). `security_invoker` is stored but not yet enforced.
- Row triggers on a partitioned table are cloned to its partitions (existing, created, attached and sub-partitions; `pg_trigger.tgparentid`) and fire on the leaf partition. The clones follow the parent trigger through `DROP TRIGGER`, `ALTER TRIGGER ... RENAME`, `ENABLE` / `DISABLE` and `CREATE OR REPLACE TRIGGER`, cannot be dropped or renamed on their own, and go with `DETACH PARTITION`. Statement triggers of a partitioned table fire for statements on it, and its partitions' statement triggers no longer do.

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
