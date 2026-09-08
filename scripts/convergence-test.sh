#!/usr/bin/env bash
# Do two live nodes see each other's writes?
#
# The scale-to-zero case only needs one machine at a time. Horizontal scaling
# needs more: each node writes locally and publishes to the shared bucket, and
# every node has to end up with everyone's rows without any of them being the
# coordinator.
set -euo pipefail
cd "$(dirname "$0")/.."

# MinIO keeps its data across `compose down`, so give every run its own key
# namespace. Otherwise a node restores the previous run's rows at boot and
# the result says nothing about this one.
export DBCENTER_PREFIX="run-$(date +%s)"
echo "storage prefix: $DBCENTER_PREFIX"

PSQL="docker run --rm --network dbcenter_default -e PGPASSWORD=dbcenter123 postgres:14 psql"
A="-h node-a -U postgres -d postgres -tAq"
B="-h node-b -U postgres -d postgres -tAq"

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
names() { $PSQL "$@" -c "select string_agg(data->>'name', ',' order by data->>'name') from users"; }

wait_for() {
  for _ in $(seq 1 60); do
    $PSQL "$@" -c 'select 1' >/dev/null 2>&1 && return 0
    sleep 2
  done
  echo "timed out waiting for a node" >&2; return 1
}

step "Bringing two nodes up against one bucket"
docker compose up -d >/dev/null
wait_for $A
wait_for $B

# Start from empty so a rerun is not confused by the previous run's rows.
$PSQL $A -c "delete from users" >/dev/null 2>&1 || true

step "node-a writes a row that node-b has never seen"
$PSQL $A -c "insert into users (data) values ('{\"name\":\"from-a\"}'::jsonb)"
$PSQL $B -c "insert into users (data) values ('{\"name\":\"from-b\"}'::jsonb)"

# Before any sync round each node must know only its own row. If they already
# agree here, something other than the bucket is carrying the rows and the test
# below would prove nothing.
before_a=$(names $A); before_b=$(names $B)
echo "node-a before sync: $before_a"
echo "node-b before sync: $before_b"
if [ "$before_a" = "$before_b" ]; then
  printf '\n\033[31mINCONCLUSIVE\033[0m nodes already agreed before syncing\n'
  exit 1
fi

step "Waiting for two sync rounds"
sleep 55
docker compose logs node-b 2>/dev/null | grep -c "Pulled" | sed 's/^/  pull passes on node-b: /' 

a=$(names $A); b=$(names $B)
echo "node-a: $a"
echo "node-b: $b"

if [ "$a" = "from-a,from-b" ] && [ "$b" = "from-a,from-b" ]; then
  printf '\n\033[32mPASS\033[0m both nodes converged on both rows\n'
else
  printf '\n\033[31mFAIL\033[0m expected from-a,from-b on both nodes\n'
  docker compose logs node-a node-b 2>/dev/null | grep -iE "pull|push|error" | tail -20
  exit 1
fi
