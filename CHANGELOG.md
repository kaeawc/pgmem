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
