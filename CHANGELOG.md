# Changelog

Notable changes to pgmem. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Add entries under
`## [Unreleased]`; `scripts/release.sh` moves them under the new version.

## [Unreleased]

### Added

- Client matrix: node-postgres (`pg`, with `pg-copy-streams`) and psycopg 3 suites (`clients/node`, `clients/python`) run against pgmem and PostgreSQL in CI.
- Static Linux binaries (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) on each release.
- `cargo binstall pgmem` installs the prebuilt binary from the GitHub release.
- `ALTER EXTENSION name UPDATE [TO version]`, `SET SCHEMA schema` and `ADD` / `DROP` of tables, views, sequences, functions, types, domains and schemas. Extension members cannot be dropped on their own (`2BP01`, "extension requires it"), `DROP EXTENSION` removes them, and `pg_extension.extrelocatable` follows the extension. `Catalog::extension_members()` exposes membership for `pg_depend` rows with deptype `e`.
- `pg_get_keywords()` returns PostgreSQL 18's keyword list (word, catcode, barelabel, catdesc, baredesc).
- `pg_locks` lists relation locks: the ones queries (with the indexes the planner opens), DML,
  `LOCK TABLE` and DDL take, at PostgreSQL's lock level for each statement and ALTER TABLE
  subcommand. They are held to the end of the transaction (or released by `ROLLBACK TO`) and
  conflict with `LOCK ... NOWAIT` in other sessions.
- `CREATE STATISTICS` (and `ALTER` / `DROP` / `COMMENT ON STATISTICS`): the object is stored, listed in `pg_statistic_ext`, printed by `pg_get_statisticsobjdef`, `pg_get_statisticsobjdef_columns` and `pg_get_statisticsobjdef_expressions`, shown in psql's `\d` footer, and dropped with its table or a referenced column. No statistics are gathered.
- `CREATE CAST` (`WITH FUNCTION`, `WITHOUT FUNCTION`, `WITH INOUT`, `AS ASSIGNMENT | IMPLICIT`) and `DROP CAST`, with `pg_cast` listing PostgreSQL 18's builtin casts alongside user ones. Casts apply to `::` and `CAST` (and to assignments and implicit coercions of the expression itself, not to operator or function argument resolution).
- Database forks: `Embedded::fork()` and `Server::fork()` copy the committed state (catalog, rows, roles and grants, sequence values, pinned `now()`) cheaply, by sharing it copy-on-write; the copies are independent afterwards.
- In-process embedding API: `pgmem::Embedded` → `session()` with `execute_batch(sql)` (simple-query semantics: implicit transaction block, rollback on error), `query(sql)` (column names and type OIDs, rows as Postgres text) and `DbError` (SQLSTATE, message, detail, hint, object fields). `pgmem::to_pg_text` renders a value in Postgres text form. `Server::embedded()` and `Builder::start_on(&Embedded)` connect the two.
- The wire server is behind the default-on `server` feature: `pgmem = { default-features = false }` builds the embedding API without tokio.
- Multirange `+`, `*` and `-`, the range/multirange forms of `@>`, `<@`, `<<`, `>>`, `&<`, `&>` and `-|-`, `unnest(multirange)`, `range_cmp` / `multirange_cmp` and the `range::multirange` cast (part of #116).
- Row-level security (part of #151): `ALTER TABLE ... ENABLE / DISABLE / FORCE / NO FORCE ROW LEVEL SECURITY` (`pg_class.relrowsecurity` / `relforcerowsecurity`, `pg_tables.rowsecurity`), `CREATE / ALTER / DROP POLICY` and `COMMENT ON POLICY`, shown in `pg_policy` and the `pg_policies` view with expressions deparsed like PostgreSQL. Policies are enforced for roles other than superusers, `BYPASSRLS` roles and the table owner (unless FORCEd): SELECT / UPDATE / DELETE see only rows passing USING (permissive policies ORed, restrictive ones ANDed, nothing without a permissive policy), new rows of INSERT / UPDATE must pass WITH CHECK (42501 `new row violates row-level security policy ...`), SELECT policies also apply when DML reads the target, `SELECT ... FOR UPDATE / SHARE` adds the UPDATE policies of the tables it locks, views (including INSTEAD OF triggers on them) and `REFRESH MATERIALIZED VIEW` apply their owner's policies (the invoker's with `security_invoker`), `row_security = off` raises 42501, `row_security_active()`, COPY TO filters and COPY FROM is refused, and DROP ROLE / DROP COLUMN report dependent policies. Policy expressions keep the search_path they were created with, follow renamed tables, and block DROP of a table or view they read (CASCADE drops the policy). The connection's startup user is pgmem's bootstrap superuser, so policies apply only after `SET ROLE`. Not yet: `ON CONFLICT DO UPDATE` and MERGE on tables with active policies (refused with 0A000).
- Table inheritance (part of #141): `CREATE TABLE ... INHERITS (parent, ...)` merges columns like PostgreSQL (its NOTICEs, type / collation / default conflict errors) and inherits CHECK and NOT NULL constraints and defaults (`CHECK ... NO INHERIT` excepted); scans of a parent include its children (with their `tableoid`) unless `ONLY` is given (`parent*` too), `UPDATE`, `DELETE` and `TRUNCATE` recurse unless `ONLY`, and `INSERT` stays in the named table. `ALTER TABLE` `ADD` / `DROP` / `RENAME COLUMN`, `ALTER COLUMN TYPE`, `SET` / `DROP DEFAULT`, `SET` / `DROP NOT NULL` and `ADD` / `DROP` / `RENAME CONSTRAINT` recurse with PostgreSQL's rules and errors (inherited columns and constraints cannot be dropped or renamed in a child), `ALTER TABLE ... INHERIT / NO INHERIT` checks compatibility, and `DROP TABLE` of a parent needs `CASCADE`. `pg_inherits`, `pg_class.relhassubclass`, `pg_attribute.attislocal` / `attinhcount`, `pg_constraint.conislocal` / `coninhcount` / `connoinherit` and `pg_depend` report it; views print `FROM ONLY`. Not yet: `NOT NULL ... NO INHERIT`, MERGE into a parent.

### Fixed

- Errors carry PostgreSQL's `position` field (`P`, the 1-based character offset into the query string, shown by psql as `LINE n:` with a caret) for syntax errors and for undefined columns, relations and functions, missing FROM-clause entries, ambiguous columns, undefined operators and invalid literal input, in simple and extended (`Parse`) queries; a statement of a multi-statement query is located in the whole string. Errors inside function and `DO` bodies carry none.
- Indexes store their access method and operator classes: `CREATE INDEX ... USING hash|gin|gist|brin|spgist` shows in `pg_get_indexdef` / `pg_indexes.indexdef`, `pg_class.relam` and `pg_index.indclass` (default classes per type and method; explicit ones such as `jsonb_path_ops` or `text_pattern_ops` are stored and printed). PostgreSQL's errors apply: unknown method (42704), unique / multicolumn / `INCLUDE` on a method that lacks them (0A000), no default operator class for a type (42704 with HINT). `LIKE ... INCLUDING INDEXES` and partition index cloning keep the method.
- Range types follow PostgreSQL more closely (part of #116): `daterange` keeps `infinity` bounds inclusive, `@>` / `<@` with an element need the exact subtype and range constructors take only implicit casts, `min` / `max` reject ranges, `range || text` concatenates, and `pg_range.rngsubopc` / `rngsubdiff` and the range rows' `pg_type.typalign` / `typanalyze` match PostgreSQL.
- `EXECUTE` parameters and `CALL` arguments reject subqueries (0A000 `cannot use subquery in EXECUTE parameter` / `... in CALL argument`) and name themselves in aggregate / window function errors, as PostgreSQL does.
- Binary `COPY ... TO STDOUT` sends the file header in the first row's CopyData message, as PostgreSQL does (psycopg could not read binary COPY rows).
- `Describe` of a portal named after a `DECLARE`d cursor returns its row description (psycopg server-side cursors failed with "portal does not exist").
- `ALTER TABLE ... DROP COLUMN` drops the CHECK constraints that use the column, as PostgreSQL does.
- Function attributes are stored and shown: `SECURITY DEFINER`, `SET param = value` (`pg_proc.proconfig`), `PARALLEL`, `COST`, `ROWS` and `LEAKPROOF` appear in `pg_proc` and `pg_get_functiondef` as PostgreSQL prints them (procedures use `$procedure$`), and a function's `SET` options apply during its call.
- `ALTER FUNCTION / PROCEDURE / ROUTINE` supports `SET` / `RESET`, `[EXTERNAL] SECURITY`, volatility, `[NOT] LEAKPROOF`, `STRICT` / `CALLED ON NULL INPUT`, `PARALLEL`, `COST`, `ROWS`, `RENAME TO` and `SET SCHEMA`.
- `ALTER TABLE ... ALTER COLUMN SET STATISTICS | STORAGE | COMPRESSION`, `REPLICA IDENTITY`, `CLUSTER ON`, `SET WITHOUT CLUSTER` and `SET LOGGED | UNLOGGED` are accepted with PostgreSQL's validation and errors, and show up in `pg_attribute` (`attstattarget`, `attstorage`, `attcompression`), `pg_class` (`relreplident`, `relpersistence`) and `pg_index` (`indisreplident`, `indisclustered`); `ALTER COLUMN TYPE` resets storage and compression. `SET LOGGED | UNLOGGED` were silently ignored before.
- `int2vector` / `oidvector` print space-separated (`3 1 4`) when cast to text, cast to `int2[]` / `oid[]` with lower bound 0, and parse from text literals.
- An index with mixed column and expression keys reports the plain column's attnum in `pg_index.indkey` (`1 0`) and lists only the real expressions in `indexprs`.
- The SQL deparser (`pg_get_viewdef`, `pg_get_constraintdef`, `pg_get_indexdef`, column defaults) quotes identifiers with the same recorded keyword data as `quote_ident()`.
- Function lookup and `pg_get_triggerdef` never resolve functions through `pg_temp` implicitly (a temp trigger function prints as `pg_temp.f()`), and a user function shadowed by a same-signature `pg_catalog` builtin is schema-qualified.
- `DROP SCHEMA` lists the schema's functions and procedures (and materialized views) in the "depends on schema" detail and the `CASCADE` notice, in creation order, fails with 2BP01 while they exist, and `CASCADE` drops them.
- `ALTER TABLE ... ADD COLUMN ... DEFAULT` on a partitioned table sets `atthasmissing` on existing partitions only, not on the parent or on partitions created afterwards.
- Under `session_replication_role = replica`, foreign keys are not enforced and `ON DELETE` / `ON UPDATE` actions do not fire, as in PostgreSQL (`ADD CONSTRAINT` still validates existing rows).
- `SECURITY DEFINER` functions, procedures and trigger functions run as their owner: `current_user` and privilege checks inside the body are the owner's (session_user and the `role` setting are unchanged), restored after the call even when it fails; `SET role` in one fails when called (`42501 cannot set parameter "role" within security-definer function`).
- Statements inside function bodies are checked for the current user's privileges, and calling a function needs EXECUTE (`42501 permission denied for function f` / `procedure p`), checked before the statement runs; `REVOKE EXECUTE ... FROM PUBLIC` takes away the default, and `has_function_privilege` answers from the grants.
- A function's `SET search_path` applies to the names its statements resolve, not only to `current_setting`.
- Functions are owned by the role that creates them (`pg_proc.proowner`); `ALTER FUNCTION f(args) OWNER TO` changes the named overload; `REASSIGN OWNED` and `DROP OWNED` cover functions.
- `ALTER FUNCTION ... [NO] DEPENDS ON EXTENSION`: recorded in `pg_depend` (`x`), and dropping the extension drops the function.
- `information_schema.routines` has PostgreSQL's 82 columns (`routine_body` is `SQL` for SQL functions; `is_null_call`, `sql_data_access`, `parameter_style`, `type_udt_*`, ...), lists only routines the current user owns or may execute, and shows `routine_definition` to the owner only.
- `pg_constraint.connamespace` is the namespace of the owning table or domain instead of always `public`.
- Ownership is enforced: ALTER, DROP, COMMENT ON and CREATE OR REPLACE of a function, procedure, table, view, materialized view, sequence, index, trigger, type, domain, schema or extension by a role that is neither its owner, a member of the owning role nor a superuser fail with `42501 must be owner of <kind> <name>` (PostgreSQL's wording per statement, e.g. `relation t` for DROP TRIGGER and COMMENT ON COLUMN, `type d` for domains). `OWNER TO` also needs membership in the new owner (`must be able to SET ROLE "r"`) and the new owner's CREATE on the schema. Types, domains, extensions and schemas created under `SET ROLE` are owned by that role.
- CREATE TRIGGER needs TRIGGER on the table and EXECUTE on the trigger function; REFRESH MATERIALIZED VIEW needs MAINTAIN.
- After `SET ROLE`, `$user` in `search_path` is the current role, schemas the role has no USAGE on drop out of the search path, and a qualified name in one fails with `42501 permission denied for schema s`.
- DROP EXTENSION checks what depends on the functions marked `DEPENDS ON EXTENSION` (column defaults, triggers): `2BP01 cannot drop extension x because other objects depend on it` with PostgreSQL's DETAIL and HINT, and CASCADE drops them; DROP FUNCTION ... CASCADE drops dependent triggers too.
- CREATE on the schema is required to create tables, views, materialized views, sequences, indexes, functions, procedures, types and domains in it, to rename tables, views, sequences, indexes and functions, and for `ALTER ... SET SCHEMA` on the target schema (`42501 permission denied for schema s`); `public` gives PUBLIC only USAGE, as PostgreSQL 15+. `CREATE SCHEMA` and `ALTER SCHEMA ... RENAME` need CREATE on the database (`GRANT CREATE ON DATABASE`).
- A qualified type name in a schema the role has no USAGE on is refused, and a `SECURITY DEFINER` function's `$user` is its owner.
- `DROP EXTENSION ... CASCADE` reports its dependents in one notice, `drop cascades to N other objects` with a DETAIL list (as does `DROP SCHEMA ... CASCADE`); `pg_proc.proacl` shows a function's ACL once EXECUTE was granted or revoked.
- `ALTER SEQUENCE ... OWNER TO` parses; `pg_tables.tableowner` shows the table's owner.
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
- `LOCK TABLE` locks a partitioned table's partitions (not with `ONLY`) and a view's base
  relations, and refuses sequences and indexes (`42809 cannot lock relation`).
- `regrole` input and output (`'role'::regrole`, `to_regrole()`, casts to and from `oid`), `pg_class.relowner` for objects owned by other roles, and the `pg_policies` view (empty until row-level security exists).
- `ALTER DEFAULT PRIVILEGES [FOR ROLE r] [IN SCHEMA s]` is stored in `pg_default_acl` (as in Postgres, global entries start from the hard-wired defaults and vanish when equal to them) and applied to tables, views and sequences created afterwards by that role.
- Single-row `INSERT`, and `UPDATE` / `DELETE` by key, no longer slow down as the table or the
  transaction grows: unique, primary key and foreign key checks, `ON CONFLICT` and `WHERE col =
  constant` use hash indexes, and `COMMIT` applies its changes in place instead of copying the
  table. 5,000 inserts in one transaction: 5.5 s → 0.7 s; 5,000 autocommit inserts into a
  5,000-row table: 15.9 s → 0.7 s; 5,000 updates plus 5,000 deletes by primary key in one
  transaction: 52 s → 1.6 s. Cost per statement is now flat (20,000 inserts: 2.7 s).
- `MERGE` into a partitioned table acts on the rows of its leaf partitions instead of none: inserts
  are routed to their partition (23514 when none accepts the row), updates move rows across
  partitions, and a target that is itself a partition enforces its partition constraint.
  `MERGE ... RETURNING` can read the target's system columns (`tableoid` is the row's leaf).
- `MERGE` fires triggers: the target's BEFORE / AFTER statement triggers for each action type its
  WHEN clauses list (even when no row matches), and BEFORE / AFTER row triggers (WHEN, `UPDATE OF`)
  of the table or leaf partition each row is stored in; BEFORE row triggers can change the new row
  or skip it. A row moved to another partition fires DELETE and INSERT row triggers.
- `RETURNING old.tableoid` / `new.tableoid` (and the other system columns of `old` / `new`) work in
  INSERT, UPDATE, DELETE and MERGE instead of failing with "column does not exist".
- Privilege checks cover every relation a statement reads, as in PostgreSQL: `UPDATE ... FROM`,
  `DELETE ... USING`, subqueries in `WHERE` / `SET` / `RETURNING` / `VALUES`, `INSERT ... SELECT`,
  `ON CONFLICT DO UPDATE`, and WITH queries (data-modifying ones always, SELECT ones when
  referenced). `MERGE` needs each listed action's privilege on the target (column privileges for
  `UPDATE SET` and `INSERT` columns), SELECT on the target columns it reads, and SELECT on the
  source. `COPY table TO` / `FROM` need SELECT / INSERT on the copied columns, checked before the
  copy starts. `UPDATE` / `DELETE` on a view with `INSTEAD OF` triggers need SELECT on the view
  columns `WHERE`, `SET` and `RETURNING` read.
- Constraint-violation DETAIL hides what the role cannot read: without SELECT on the table,
  `Failing row contains (cols) = (values).` lists only the columns it may select or the statement
  writes (no DETAIL when none), and unique, exclusion, foreign key and partition key details omit
  the key unless every key column is readable, deferred checks at `COMMIT` / `SET CONSTRAINTS`
  included. Writes through an automatically updatable view count the base columns they write.
- A row routed into a partition is described in the partitioned table's column order in a
  failing-row DETAIL, as PostgreSQL does.
- `UPDATE` on a view with `INSTEAD OF` triggers accepts `SET c = DEFAULT` and
  `SET (a, b) = (x, y)`.
- `information_schema.table_privileges` and `role_table_grants` show only grants whose grantor or
  grantee is a role the current user has the privileges of (`table_privileges` also PUBLIC's).
- Partitioning matches PostgreSQL in six details: `CREATE TABLE ... PARTITION OF parent (col DEFAULT ...)` column options parse; a partition's NOT NULL constraints keep the parent's names; a CHECK violated through the parent names the partition; a foreign key violation when deleting a referenced row names the partition (and its copy of the constraint, `<name>_<n>`); new partitions do not copy `atthasmissing`.
- `DROP SCHEMA ... CASCADE` reports its dependents as the notice's `DETAIL` in creation order, leaves out partitions, and qualifies names only when they are not on the `search_path`; `DROP SCHEMA` without `CASCADE` lists them the same way.
- Storage parameters are stored and validated like PostgreSQL: `WITH (...)` on tables, indexes, views and materialized views, `ALTER TABLE / INDEX / VIEW / MATERIALIZED VIEW ... SET / RESET (...)`, `pg_class.reloptions`, `pg_get_indexdef` (`WITH (fillfactor='70')`) and `information_schema.views.check_option` (including `WITH CHECK OPTION` and `check_option`).
- Storage-parameter gaps: `CREATE TABLE ... PARTITION BY ... WITH (...)` takes PostgreSQL's clause order (the reverse is a syntax error); `PRIMARY KEY`, `UNIQUE` and `EXCLUDE` constraints take `WITH (...)` (in `CREATE TABLE` and `ALTER TABLE ADD`), shown by `pg_get_indexdef` and, for `EXCLUDE`, `pg_get_constraintdef`; `ALTER TABLE ... SET / RESET (...)` combines with other actions; `RESET (x = 1)` and empty `SET ()` / `WITH ()` give PostgreSQL's errors; integer values in hex, octal, binary or with `_` are stored in decimal.
- `WITH CHECK OPTION` (or `check_option`) on a view that is not automatically updatable fails with `0A000` and PostgreSQL's hint; `LOCAL` and `CASCADED` check options are told apart when a view over views is written, each violation naming the view whose condition failed; `ALTER VIEW ... SET / RESET` keeps the view's grants and owner.
- `security_invoker` views run with the caller's privileges: the caller needs privileges on the views' base tables.

### Changed

- Releases go through a PR: `scripts/release.sh X.Y.Z` opens it, and merging it publishes.
- Publishing skips crates already on crates.io, so a failed release can be rerun.
- Release notes come from this changelog.

### Fixed

- `pg_get_triggerdef` schema-qualifies the trigger function when its schema is not on `search_path`, and quotes its name.
- Partitioned indexes: `CREATE INDEX ... ON ONLY` and `ALTER INDEX ... ATTACH PARTITION` (with Postgres' checks and errors); partition indexes are attached to their parent (`pg_inherits`, `relispartition`, `pg_partition_tree` / `pg_partition_ancestors` / `pg_partition_root`), `pg_index.indisvalid` is false until every partition has one, `CREATE INDEX` on a partitioned table and `ATTACH PARTITION` reuse a matching index of the partition, and `pg_get_indexdef` prints `ON ONLY` for an index on a partitioned table.
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
