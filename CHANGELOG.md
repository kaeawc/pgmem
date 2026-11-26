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
- `pg_depend` records the dependencies Postgres records for user objects (objects on their schema,
  row and array types, view and materialized-view rules on the columns, functions and types they
  use, constraints and indexes on their columns, foreign keys on the referenced index, defaults on
  sequences and columns, `OWNED BY` sequences, triggers, partitions, column and function types,
  domains). New `pg_rewrite` (one `_RETURN` rule per view and materialized view; `ev_action` holds
  the view's SQL text, not Postgres' node tree), `pg_class.relhasrules`, row-type array types in
  `pg_type`, and `pg_describe_object`, `pg_identify_object`, `pg_identify_object_as_address`.
  Toast tables and the internal triggers of foreign keys are not modelled.

### Changed

- Releases go through a PR: `scripts/release.sh X.Y.Z` opens it, and merging it publishes.
- Publishing skips crates already on crates.io, so a failed release can be rerun.
- Release notes come from this changelog.

### Fixed

- `pg_get_triggerdef` schema-qualifies the trigger function when its schema is not on `search_path`, and quotes its name.
- `ALTER TRIGGER ... RENAME TO`, and `ALTER TABLE ... ENABLE REPLICA | ALWAYS TRIGGER` (`pg_trigger.tgenabled` `O`/`D`/`R`/`A`); triggers fire according to `session_replication_role`.

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
