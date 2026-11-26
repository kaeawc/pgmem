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

- `pg_constraint.connamespace` is the namespace of the owning table or domain instead of always `public`.

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
