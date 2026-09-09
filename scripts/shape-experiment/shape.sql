-- Built by run.sh; see the comment at the top of it.
\set QUIET on
SET client_min_messages=warning;

-- ---------- 1. plain PostgreSQL, 200k rows ----------
DROP TABLE IF EXISTS plain CASCADE;
CREATE TABLE plain (
  id uuid PRIMARY KEY, data jsonb,
  created_at timestamptz, updated_at timestamptz,
  a bigint, b bigint, c double precision, s text, payload text);
INSERT INTO plain
SELECT gen_random_uuid(), NULL,
       now() - (g||' seconds')::interval, now() - (g||' seconds')::interval,
       g, g%1000, random(), 'k'||(g%50), md5(g::text)||md5((g+1)::text)||md5((g+2)::text)||md5((g+3)::text)
FROM generate_series(1,200000) g;
CREATE INDEX ON plain (created_at DESC);
ANALYZE plain;

-- ---------- 2. the shape we ship: base + adds + dels behind a view ----------
DROP VIEW IF EXISTS coll CASCADE;
DROP TABLE IF EXISTS coll_base, coll_adds, coll_dels CASCADE;
CREATE TABLE coll_base (LIKE plain INCLUDING ALL);
CREATE TABLE coll_adds (LIKE plain INCLUDING ALL);
CREATE TABLE coll_dels (id uuid PRIMARY KEY, deleted_at timestamptz DEFAULT now());
INSERT INTO coll_base SELECT * FROM plain;
-- 20k rows updated since the last compaction: new version in adds, tombstone in dels
INSERT INTO coll_adds SELECT * FROM coll_base ORDER BY created_at DESC LIMIT 20000;
INSERT INTO coll_dels (id) SELECT id FROM coll_adds;
CREATE VIEW coll AS
  SELECT * FROM coll_adds a
  UNION ALL
  SELECT * FROM coll_base b
  WHERE NOT EXISTS (SELECT 1 FROM coll_adds x WHERE x.id=b.id)
    AND NOT EXISTS (SELECT 1 FROM coll_dels y WHERE y.id=b.id);
ANALYZE coll_base; ANALYZE coll_adds; ANALYZE coll_dels;

-- ---------- 3. one partitioned table ----------
DROP TABLE IF EXISTS part CASCADE;
CREATE TABLE part (
  id uuid, part_key text NOT NULL, data jsonb,
  created_at timestamptz, updated_at timestamptz,
  a bigint, b bigint, c double precision, s text, payload text,
  PRIMARY KEY (id, part_key)) PARTITION BY LIST (part_key);
CREATE TABLE part_base PARTITION OF part FOR VALUES IN ('base');
CREATE TABLE part_adds PARTITION OF part FOR VALUES IN ('adds');
INSERT INTO part SELECT id,'base',data,created_at,updated_at,a,b,c,s,payload FROM coll_base
  WHERE NOT EXISTS (SELECT 1 FROM coll_adds x WHERE x.id=coll_base.id);
INSERT INTO part SELECT id,'adds',data,created_at,updated_at,a,b,c,s,payload FROM coll_adds;
CREATE INDEX ON part (created_at DESC);
ANALYZE part;
