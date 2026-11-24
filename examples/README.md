# pgmem examples

Each subdirectory is a self-contained sqlc + pgx project that runs its
test suite against a pgmem server (the Rust `pgmem` binary). Together they
exercise the SQL dialect surface pgmem needs to cover for typical sqlc
workloads, through Go's pgx driver.

## Layout

```
examples/
  blog/          users · posts · comments    (CRUD, joins, RETURNING, correlated subquery)
  todo/          lists · items               (upsert, soft delete, intervals)
  inventory/     products · stock · txns     (aggregates, CTEs, CASE)
  events/        events with jsonb payloads  (jsonb ops, regex, date funcs)
  chat/          users · rooms · messages    (UNION, self-joins, correlated EXISTS)
  analytics/     players · matches           (window funcs, arrays + ANY, array_agg, unnest)
```

Each project owns:

- `schema.sql` — `CREATE TABLE …` statements applied at test setup
- `queries.sql` — sqlc-annotated query definitions
- `sqlc.yaml` — sqlc generator config (Postgres dialect, `pgx/v5` driver)
- `db/` — generated Go code (committed so `go test` works without sqlc)
- `*_test.go` — exercises every generated query against a pgmem server
  started per test

## Per-project surface

### blog
- Plain CRUD with `RETURNING *`
- INNER and LEFT joins for "post with author" and "comments per post"
- `count(*) … GROUP BY` for per-post comment counts
- Pagination via `ORDER BY … LIMIT $1 OFFSET $2`

### todo
- `INSERT … ON CONFLICT (id) DO UPDATE SET …` upserts
- Soft delete (`deleted_at IS NULL` filtering)
- `created_at + interval '7 days'` for due-date math
- Boolean aggregates (`bool_and(done)` per list)

### inventory
- `sum(qty) GROUP BY product_id` to compute on-hand stock
- `WITH … SELECT …` rolling up per-warehouse stock
- `CASE WHEN qty < threshold THEN 'low' ELSE 'ok' END`
- Self-referential FK on the categories tree

### events
- `body @> '{"key":"value"}'::jsonb` filters
- `body ->> 'kind'` text extraction
- `EXTRACT(epoch FROM created_at)` and `date_trunc('hour', …)` for
  time-bucketing
- Regex match on a string field (`message ~* 'error'`)

### chat
- `UNION ALL` across `messages` and `system_messages` for an inbox view
- Correlated `EXISTS (SELECT 1 FROM subscriptions WHERE … s.room_id = r.id)` to test membership
- Self-join for threaded replies (`messages m JOIN messages r ON r.parent_id = m.id`)

### analytics
- `row_number() OVER (PARTITION BY region ORDER BY score DESC)`
- `rank() OVER (ORDER BY score DESC)` with tied scores
- `array_agg(score)` aggregating per player
- `WHERE id = ANY($1::bigint[])` parameter list filter
- `FROM unnest($1::bigint[])` set-returning expansion

## Running

```
cargo build --release -p pgmem   # from the repository root
cd examples
go test ./...
```

Each test starts its own `pgmem` process via `pgmemtest.Start(t)`
(`internal/pgmemtest`), applies `schema.sql`, then runs the
sqlc-generated queries. The binary is `$PGMEM_BIN` if set, else the
nearest `target/release/pgmem` above the working directory, else `pgmem`
on `PATH`. No external Postgres required.

## Regenerating sqlc code

Install sqlc 1.30+ (`go install github.com/sqlc-dev/sqlc/cmd/sqlc@latest`),
then from any example dir:

```
sqlc generate
```

The generated `db/` directory is committed so a fresh checkout can
`go test` without the sqlc toolchain.
