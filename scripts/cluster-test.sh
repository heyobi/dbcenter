#!/usr/bin/env bash
# Does state actually live in object storage?
#
# Writes through one node, then destroys that node's container and disk
# entirely. If the row comes back on a different node, the bucket is the source
# of truth and a machine really is disposable - which is the whole premise of
# stopping machines when they are idle.
set -euo pipefail
cd "$(dirname "$0")/.."

PSQL="docker run --rm --network dbcenter_default -e PGPASSWORD=dbcenter123 postgres:14 psql"
CONN_A="-h node-a -U postgres -d postgres -tAq"
CONN_B="-h node-b -U postgres -d postgres -tAq"

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

wait_for() { # wait_for <conn args...>
  for _ in $(seq 1 60); do
    if $PSQL "$@" -c 'select 1' >/dev/null 2>&1; then return 0; fi
    sleep 2
  done
  echo "timed out waiting for a node to accept connections" >&2
  return 1
}

step "Bringing the cluster up"
docker compose up -d
wait_for $CONN_A
wait_for $CONN_B
echo "both nodes accepting connections"

step "Writing through node-a"
$PSQL $CONN_A -c "insert into users (data) values ('{\"name\":\"Ada\"}'::jsonb)"
count_a=$($PSQL $CONN_A -c "select count(*) from users")
echo "node-a sees $count_a row(s)"
[ "$count_a" -ge 1 ] || { echo "write did not land" >&2; exit 1; }

step "Waiting for the sync interval so the row reaches object storage"
sleep 12
docker compose logs node-a --tail 5 | sed 's/^/  /'

step "Destroying node-a entirely, disk included"
docker compose rm -sfv node-a >/dev/null
echo "node-a is gone"

step "Restarting node-b so it restores from the bucket"
docker compose restart node-b >/dev/null
wait_for $CONN_B
docker compose logs node-b 2>/dev/null | grep -E "Restoring|Restored|Nothing in object" | tail -3 | sed 's/^/  /' || true

count_b=$($PSQL $CONN_B -c "select count(*) from users")
echo "node-b sees $count_b row(s) after restoring"

if [ "$count_b" -ge 1 ]; then
  printf '\n\033[32mPASS\033[0m the row survived the node that wrote it\n'
else
  printf '\n\033[31mFAIL\033[0m the row did not come back from object storage\n'
  docker compose logs node-b --tail 25
  exit 1
fi
