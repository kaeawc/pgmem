#!/usr/bin/env bash
# Runs every clients/psql/*.sql script with psql against pgmem and against PostgreSQL and diffs
# the output. Error positions (`LINE n:` and the caret under it) are dropped: pgmem reports
# none.
#
#   clients/psql/compare.sh PGMEM_BIN POSTGRES_URL
#   (POSTGRES_URL: an admin URL such as postgres://postgres@127.0.0.1:5432/postgres)
set -euo pipefail
bin=$1
admin=$2
dir=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
status=0

run() { # url script out
  PGTZ=UTC psql -X -a -v ON_ERROR_STOP=0 "$1" -f "$2" 2>&1 | grep -Ev '^LINE [0-9]+:|^ +\^$' | sed "s|$2|SCRIPT|" > "$3"
}

for script in "$dir"/*.sql; do
  name=$(basename "$script" .sql)
  # A fresh pgmem server and a fresh PostgreSQL database per script.
  "$bin" --port 0 > "$work/url" &
  pid=$!
  for _ in $(seq 50); do [ -s "$work/url" ] && break; sleep 0.1; done
  db="psql_client_$name"
  psql -X -q "$admin" -c "DROP DATABASE IF EXISTS $db" -c "CREATE DATABASE $db"
  # Both connect as postgres (pgmem accepts any user), so owners print the same.
  run "$(sed "s|//[^@]*@|//postgres@|" "$work/url")" "$script" "$work/$name.pgmem"
  run "${admin%/*}/$db" "$script" "$work/$name.postgres"
  kill "$pid"
  if diff -u "$work/$name.postgres" "$work/$name.pgmem"; then
    echo "psql $name: identical"
  else
    echo "psql $name: pgmem differs from PostgreSQL" >&2
    status=1
  fi
done
exit $status
