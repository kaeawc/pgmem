// Package pgmemtest starts a pgmem server (the Rust binary) for a test, keeping the shape of
// pgmem-go's in-process API (Start / SetNow / DSN) so the example suites run unchanged against
// the Rust implementation through Go's pgx driver.
//
// The binary is found through $PGMEM_BIN, falling back to ../target/release/pgmem relative to
// the examples module and then to `pgmem` on $PATH.
package pgmemtest

import (
	"bufio"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// Server is one pgmem process. It is started lazily by DSN, so SetNow can be called first.
type Server struct {
	t   testing.TB
	now time.Time
	dsn string
}

// Start registers a pgmem server for the test; the process stops when the test ends.
func Start(t testing.TB) (*Server, error) {
	t.Helper()
	return &Server{t: t}, nil
}

// SetNow pins the time now() returns. Call it before DSN.
func (s *Server) SetNow(t time.Time) { s.now = t }

// DSN starts the server (once) and returns its connection string.
func (s *Server) DSN() string {
	s.t.Helper()
	if s.dsn != "" {
		return s.dsn
	}
	args := []string{"--port", "0"}
	if !s.now.IsZero() {
		args = append(args, "--now", s.now.UTC().Format("2006-01-02 15:04:05.999999Z07:00"))
	}
	cmd := exec.Command(binary(), args...)
	out, err := cmd.StdoutPipe()
	if err != nil {
		s.t.Fatalf("pgmem: %v", err)
	}
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		s.t.Fatalf("pgmem: start %s: %v (set PGMEM_BIN or build with `cargo build --release -p pgmem`)", cmd.Path, err)
	}
	s.t.Cleanup(func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
	})
	line, err := bufio.NewReader(out).ReadString('\n')
	if err != nil {
		s.t.Fatalf("pgmem: reading DSN: %v", err)
	}
	s.dsn = strings.TrimSpace(line)
	if !strings.HasPrefix(s.dsn, "postgres://") {
		s.t.Fatalf("pgmem: unexpected output %q", s.dsn)
	}
	return s.dsn
}

func binary() string {
	if b := os.Getenv("PGMEM_BIN"); b != "" {
		return b
	}
	wd, err := os.Getwd()
	if err == nil {
		for dir := wd; dir != filepath.Dir(dir); dir = filepath.Dir(dir) {
			candidate := filepath.Join(dir, "target", "release", "pgmem")
			if _, err := os.Stat(candidate); err == nil {
				return candidate
			}
		}
	}
	return "pgmem"
}

