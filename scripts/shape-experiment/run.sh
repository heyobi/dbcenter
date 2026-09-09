#!/usr/bin/env bash
# Does making _base and _adds partitions of one table actually pay?
#
# Builds three shapes of the same 200,000 rows in a throwaway PostgreSQL and
# times the same queries against each:
#
#   plain  a single table                       - the PostgreSQL we are measured against
#   coll   base + adds + dels behind the view   - what DBCenter ships today
#   part   one LIST-partitioned table           - the proposal
#
# It touches no DBCenter code. Run it before and after any change to the delta
# layer; the point of it is that the answer is a measurement, not an argument.
set -euo pipefail
cd "$(dirname "$0")"
C=${CONTAINER:-pgshape}
docker rm -f "$C" >/dev/null 2>&1 || true
docker run -d --name "$C" -e POSTGRES_HOST_AUTH_METHOD=trust \
  --memory=1g --cpus=1 postgres:18 >/dev/null
for _ in $(seq 1 60); do docker exec "$C" pg_isready -q && break; sleep 1; done

psql() { docker exec "$C" psql -qAt -U postgres "$@"; }
for f in shape.sql writes.sql; do docker cp "$f" "$C:/tmp/$f" >/dev/null; psql -f "/tmp/$f" >/dev/null; done

best() { # label, query - report the best of seven, so a cold cache does not decide it
  psql -c "$2" >/dev/null
  local b=99999 ms
  for _ in $(seq 1 7); do
    ms=$(psql -c "EXPLAIN (ANALYZE,TIMING OFF,FORMAT JSON) $2" | grep -o '"Execution Time": [0-9.]*' | grep -o '[0-9.]*')
    awk -v a="$ms" -v b="$b" 'BEGIN{exit !(a<b)}' && b=$ms
  done
  printf '%-44s %9s ms\n' "$1" "$b"
}

psql -c "CREATE INDEX ON plain(a); CREATE INDEX ON coll_base(a); CREATE INDEX ON coll_adds(a);
         CREATE INDEX ON part(a); ANALYZE plain; ANALYZE coll_base; ANALYZE coll_adds; ANALYZE part;" >/dev/null

echo "reads"
for t in plain coll part; do best "  $t | 100 rows by timestamp"    "SELECT * FROM $t ORDER BY created_at DESC LIMIT 100"; done
for t in plain coll part; do best "  $t | count+avg over the table" "SELECT count(*), avg(c) FROM $t"; done
for t in plain coll part; do best "  $t | point lookup, indexed"    "SELECT * FROM $t WHERE a = 137651"; done

echo; echo "the plan that matters"
psql -c "EXPLAIN (ANALYZE,TIMING OFF,BUFFERS) SELECT * FROM part ORDER BY created_at DESC LIMIT 100" | sed 's/^/  /' | head -8

echo; echo "writes, 2000 statements each"
w() { psql -c "DO \$\$ DECLARE t timestamptz := clock_timestamp(); BEGIN
  FOR i IN 1..2000 LOOP $2 END LOOP;
  RAISE NOTICE '%', round(extract(epoch from clock_timestamp()-t)*1000/2000::numeric, 4); END \$\$;" 2>&1 |
  grep -o '[0-9.]*$' | head -1 | xargs printf "  %-42s %9s ms/row\n" "$1"; }
w "coll | insert" "INSERT INTO coll (id,created_at,updated_at,a,c,s,payload) VALUES (gen_random_uuid(),now(),now(),i,random(),'z',md5(i::text));"
w "part | insert" "INSERT INTO part (id,part_key,created_at,updated_at,a,c,s,payload) VALUES (gen_random_uuid(),'adds',now(),now(),i,random(),'z',md5(i::text));"
w "coll | update a row held in base" "UPDATE coll SET c = random() WHERE a = 50000 + i;"
w "part | update a row held in base" "UPDATE part SET c = random() WHERE a = 50000 + i;"
w "coll | delete" "DELETE FROM coll WHERE a = 120000 + i;"
w "part | delete" "DELETE FROM part WHERE a = 120000 + i;"

echo; echo "the hole partitioning opens"
psql -c "CREATE TEMP TABLE v AS SELECT id FROM part_base LIMIT 1;
  INSERT INTO part (id,part_key,c) SELECT id,'adds',9.9 FROM v;
  SELECT '  partitioned: rows visible for a duplicated id -> '||count(*) FROM part WHERE id=(SELECT id FROM v);"
psql -c "CREATE TEMP TABLE v AS SELECT id FROM coll_base WHERE NOT EXISTS(SELECT 1 FROM coll_adds x WHERE x.id=coll_base.id) LIMIT 1;
  INSERT INTO coll_adds (id,c) SELECT id,9.9 FROM v;
  SELECT '  view:        rows visible for a duplicated id -> '||count(*) FROM coll WHERE id=(SELECT id FROM v);"

docker rm -f "$C" >/dev/null
