# Plain cargo underneath; `examples` runs the Go pgx suites against the release binary.
.PHONY: check test lint fmt deny parity fidelity conformance conformance-update examples ci build serve

check:            ## type-check everything (no codegen, no link)
	cargo check --all-targets

test:             ## all tests against pgmem (nextest if installed)
	@command -v cargo-nextest >/dev/null && cargo nextest run --workspace || cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

deny:             ## dependency policy: duplicates, advisories, licenses
	cargo deny check

parity:           ## every pgmem-go test has a Rust port
	python3 scripts/parity.py check

fidelity:         ## the integration suite against real Postgres (set PGMEM_TEST_DSN)
	@test -n "$$PGMEM_TEST_DSN" || (echo "set PGMEM_TEST_DSN, e.g. host=127.0.0.1 port=5432 user=postgres" && exit 2)
	cargo nextest run -p pgmem --test it
	cargo run -q -p pgmem-conformance -- --dsn "$$PGMEM_TEST_DSN"

ci: fmt lint deny test parity conformance

build:
	cargo build --release -p pgmem

serve:            ## run a standalone server on port 5432
	cargo run --release -p pgmem -- --port 5432

conformance:      ## SQL corpus against pgmem, compared with conformance/known_failures.txt
	cargo run -q -p pgmem-conformance

conformance-update: ## record pgmem's current failures and regenerate CONFORMANCE.md
	cargo run -q -p pgmem-conformance -- --update

examples: build    ## Go sqlc + pgx example suites against target/release/pgmem
	cd examples && go vet ./... && go test -count=1 ./...
