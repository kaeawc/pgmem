# Changelog

Notable changes to pgmem. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

- Static Linux binaries (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) on each release.
- `cargo binstall pgmem` installs the prebuilt binary from the GitHub release.
- `pg_get_keywords()` returns PostgreSQL 18's keyword list (word, catcode, barelabel, catdesc, baredesc).

### Fixed

- Function attributes are stored and shown: `SECURITY DEFINER`, `SET param = value` (`pg_proc.proconfig`), `PARALLEL`, `COST`, `ROWS` and `LEAKPROOF` appear in `pg_proc` and `pg_get_functiondef` as PostgreSQL prints them (procedures use `$procedure$`), and a function's `SET` options apply during its call.
- `ALTER FUNCTION / PROCEDURE / ROUTINE` supports `SET` / `RESET`, `[EXTERNAL] SECURITY`, volatility, `[NOT] LEAKPROOF`, `STRICT` / `CALLED ON NULL INPUT`, `PARALLEL`, `COST`, `ROWS`, `RENAME TO` and `SET SCHEMA`.
- `SECURITY DEFINER` functions, procedures and trigger functions run as their owner: `current_user` and privilege checks inside the body are the owner's (session_user and the `role` setting are unchanged), restored after the call even when it fails; `SET role` in one fails when called (`42501 cannot set parameter "role" within security-definer function`).
- Statements inside function bodies are checked for the current user's privileges, and calling a function needs EXECUTE (`42501 permission denied for function f` / `procedure p`), checked before the statement runs; `REVOKE EXECUTE ... FROM PUBLIC` takes away the default, and `has_function_privilege` answers from the grants.
- A function's `SET search_path` applies to the names its statements resolve, not only to `current_setting`.
- Functions are owned by the role that creates them (`pg_proc.proowner`); `ALTER FUNCTION f(args) OWNER TO` changes the named overload; `REASSIGN OWNED` and `DROP OWNED` cover functions.
- `ALTER FUNCTION ... [NO] DEPENDS ON EXTENSION`: recorded in `pg_depend` (`x`), and dropping the extension drops the function.
- `information_schema.routines` has PostgreSQL's 82 columns (`routine_body` is `SQL` for SQL functions; `is_null_call`, `sql_data_access`, `parameter_style`, `type_udt_*`, ...), lists only routines the current user owns or may execute, and shows `routine_definition` to the owner only.
- `pg_constraint.connamespace` is the namespace of the owning table or domain instead of always `public`.
- `pg_get_indexdef` and `pg_indexes.indexdef` print `INCLUDE (...)` for covering indexes, and `pg_index.indnatts`, `indnkeyatts`, `indkey` and `pg_class.relnatts` count the INCLUDE columns like PostgreSQL.

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
