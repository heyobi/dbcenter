#!/usr/bin/env bash
# The standard PostgreSQL benchmark, run against the embedded engine.
#
# pgbench's default workload is TPC-B-like: five statements per transaction over
# four tables, mostly small random updates. It is the measurement people quote
# for PostgreSQL, so it is the one to quote here - and because DBCenter runs a
# real PostgreSQL, it connects to it the same way it would to any server.
#
#   ./scripts/loadtest.sh [scale] [seconds]
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

SCALE="${1:-10}"          # pgbench scale: 100k rows per unit
SECONDS_PER_RUN="${2:-15}"
PORT=55432
PASSWORD=loadtest
DATA="${TMPDIR:-/tmp}/dbcenter_loadtest"

PGBENCH=$(command -v pgbench || ls /usr/lib/postgresql/*/bin/pgbench 2>/dev/null | tail -1)
if [ -z "$PGBENCH" ]; then
  echo "pgbench not found; install postgresql-client" >&2
  exit 1
fi

cleanup() {
  [ -n "${SERVE_PID:-}" ] && kill "$SERVE_PID" 2>/dev/null
  wait "${SERVE_PID:-}" 2>/dev/null
  rm -rf "$DATA"
}
trap cleanup EXIT

echo "building dbcenter-serve"
cargo build --release -p dbcenter-serve -p dbcenter-watcher 2>&1 | tail -1

rm -rf "$DATA"
DBCENTER_DATA_DIR="$DATA" DBCENTER_LISTEN=127.0.0.1 DBCENTER_PORT=$PORT \
DBCENTER_PASSWORD=$PASSWORD DBCENTER_STORAGE=none RUST_LOG=warn \
  ./target/release/dbcenter-serve &
SERVE_PID=$!

echo -n "waiting for the server"
for _ in $(seq 1 60); do
  PGPASSWORD=$PASSWORD psql -h 127.0.0.1 -p $PORT -U postgres -tAc 'select 1' >/dev/null 2>&1 && break
  echo -n "."; sleep 1
done
echo

export PGPASSWORD=$PASSWORD
CONN="-h 127.0.0.1 -p $PORT -U postgres postgres"

echo
echo "== $($PGBENCH --version) against $(psql $CONN -tAc 'select version()' | cut -d, -f1)"
echo "== scale $SCALE, ${SECONDS_PER_RUN}s per run"
echo

"$PGBENCH" -i -s "$SCALE" -q $CONN 2>&1 | tail -2
echo

printf '%-10s %-12s %-14s %s\n' "clients" "tps" "latency(ms)" "workload"
for clients in 1 2 4 8 16; do
  jobs=$(( clients < 4 ? clients : 4 ))
  out=$("$PGBENCH" -c "$clients" -j "$jobs" -T "$SECONDS_PER_RUN" $CONN 2>&1)
  if ! echo "$out" | grep -q "^tps"; then echo "--- pgbench çıktısı ---"; echo "$out" | tail -8; fi
  tps=$(echo "$out" | awk '/^tps/ {print $3; exit}')
  lat=$(echo "$out" | awk '/^latency average/ {print $4; exit}')
  printf '%-10s %-12s %-14s %s\n' "$clients" "${tps:-?}" "${lat:-?}" "read-write (TPC-B)"
done

echo
for clients in 1 4 16; do
  jobs=$(( clients < 4 ? clients : 4 ))
  out=$("$PGBENCH" -S -c "$clients" -j "$jobs" -T "$SECONDS_PER_RUN" $CONN 2>&1)
  tps=$(echo "$out" | awk '/^tps/ {print $3; exit}')
  lat=$(echo "$out" | awk '/^latency average/ {print $4; exit}')
  printf '%-10s %-12s %-14s %s\n' "$clients" "${tps:-?}" "${lat:-?}" "read-only"
done
