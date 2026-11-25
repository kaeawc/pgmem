package analytics_test

import (
	"bytes"
	"context"
	"strings"
	"testing"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"

	postgres "github.com/kaeawc/pgmem-go/examples/internal/pgmemtest"
)

// TestCopy bulk-loads rows with pgx's CopyFrom (binary COPY FROM STDIN) and reads them back
// with COPY TO STDOUT in CSV.
func TestCopy(t *testing.T) {
	srv, err := postgres.Start(t)
	if err != nil {
		t.Fatalf("postgres.Start: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	pool, err := pgxpool.New(ctx, srv.DSN())
	if err != nil {
		t.Fatalf("pgxpool.New: %v", err)
	}
	defer pool.Close()
	if _, err := pool.Exec(ctx, `CREATE TABLE scores (id bigint PRIMARY KEY, name text, score int, at timestamptz)`); err != nil {
		t.Fatalf("create: %v", err)
	}
	at := time.Date(2024, 7, 15, 12, 0, 0, 0, time.UTC)
	rows := [][]any{{int64(1), "alice", int32(10), at}, {int64(2), nil, int32(-3), at.Add(time.Hour)}}
	n, err := pool.CopyFrom(ctx, pgx.Identifier{"scores"}, []string{"id", "name", "score", "at"}, pgx.CopyFromRows(rows))
	if err != nil {
		t.Fatalf("CopyFrom: %v", err)
	}
	if n != 2 {
		t.Fatalf("CopyFrom copied %d rows, want 2", n)
	}
	if _, err := pool.CopyFrom(ctx, pgx.Identifier{"scores"}, []string{"id"}, pgx.CopyFromRows([][]any{{int64(1)}})); err == nil ||
		!strings.Contains(err.Error(), "23505") {
		t.Fatalf("duplicate key: got %v, want SQLSTATE 23505", err)
	}
	conn, err := pool.Acquire(ctx)
	if err != nil {
		t.Fatalf("acquire: %v", err)
	}
	defer conn.Release()
	var out bytes.Buffer
	if _, err := conn.Exec(ctx, "SET TIME ZONE 'UTC'"); err != nil {
		t.Fatalf("set time zone: %v", err)
	}
	tag, err := conn.Conn().PgConn().CopyTo(ctx, &out, "COPY scores TO STDOUT WITH (FORMAT csv, HEADER)")
	if err != nil {
		t.Fatalf("CopyTo: %v", err)
	}
	if tag.RowsAffected() != 2 {
		t.Fatalf("CopyTo tag %q", tag)
	}
	want := "id,name,score,at\n1,alice,10,2024-07-15 12:00:00+00\n2,,-3,2024-07-15 13:00:00+00\n"
	if out.String() != want {
		t.Fatalf("CopyTo output %q, want %q", out.String(), want)
	}
}
