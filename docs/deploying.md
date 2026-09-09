# Running DBCenter

Three ways, in order of how much you have to think about.

## 1. In a Python process

Nothing to deploy. The library starts a real `postgres` for you and stops it
when you are done.

```sh
pip install dbcenter
```

```python
from dbcenter import DBCenter

db = DBCenter(path="./data", persist=True)
db.users.insert_one({"name": "Ada", "age": 36})
print(db.users.find({"age": {"$gt": 18}}))
```

The first run downloads a PostgreSQL distribution into `~/.dbcenter`, which
takes a few seconds and never happens again. Nothing else is installed on the
machine, and nothing is left running: the watcher process shuts the server down
even if your program is killed.

To reach the same database from psql or DBeaver, give it a port:

```python
db = DBCenter(path="./data", persist=True, host="127.0.0.1", port=5433,
              password="something")
```

```sh
psql "host=127.0.0.1 port=5433 user=postgres password=something dbname=postgres"
```

## 2. As a container

`dbcenter-serve` is the same engine with the lifecycle wired to a container's:
it restores from object storage at boot, pushes on a timer, and flushes on
SIGTERM. Everything is an environment variable, because that is what a platform
hands a process.

```sh
docker build -t dbcenter .
docker run -p 5432:5432 \
  -e DBCENTER_PASSWORD=something \
  -e DBCENTER_COLLECTIONS=events \
  -e DBCENTER_SCHEMA='{"events":{"user_id":"int","region":"text","amount":"float"}}' \
  -v dbcenter-data:/data \
  dbcenter
```

| variable | what it does |
|---|---|
| `DBCENTER_PASSWORD` | the `postgres` password. Without it only the container's own Unix socket works, which makes the TCP listener pointless |
| `DBCENTER_COLLECTIONS` | collections to create at boot, comma separated |
| `DBCENTER_SCHEMA` | the fields each collection has, as JSON: `{"events":{"user_id":"int",...}}`. Types are `int`, `float`, `text`, `bool`, `json` |
| `DBCENTER_STORAGE` | `s3`, `local` or `none` |
| `DBCENTER_BUCKET`, `DBCENTER_PREFIX` | where in the bucket |
| `DBCENTER_COLUMNAR` | install pg_duckdb and keep a `<name>_columnar` view of every collection |
| `DBCENTER_LAZY_RESTORE` | restore from the deltas only, leaving the history in the bucket |
| `DBCENTER_SYNC_INTERVAL_SECS` | how often to push (default 30) |
| `DBCENTER_SYNC_BATCH` | rows per table per pass (default 10,000) |
| `DBCENTER_COMPACT_INTERVAL_SECS` | how often to rewrite the deltas into one base (default 900; 0 disables) |
| `DBCENTER_PULL` | also take in other nodes' changes each pass. Only worth it when more than one node writes |

**`DBCENTER_SCHEMA` is not optional if you care about read speed.** A client on
the wire protocol cannot add a field to a collection - schema evolution happens
inside `insert_one`, which it cannot reach - so without a declared schema every
field it writes lands in the `data` JSONB column. That works, and the Parquet
then holds one JSON string per row instead of one column per field, which is
exactly the shape the columnar read path was built to avoid.

`docker-compose.yml` in the repository root stands two nodes up against MinIO if
you want to watch them share a bucket.

## 3. On Fly.io, with object storage

This is the arrangement `docs/cost-and-scale.md` measures: a small machine that
can be stopped when idle, and a bucket that is the durable copy.

```sh
fly apps create my-db
fly storage create                  # provisions Tigris, sets the AWS_* secrets
fly secrets set DBCENTER_PASSWORD=something
fly volumes create dbcenter_data --size 3
fly deploy
```

`fly storage create` is the easy path: it provisions Tigris and writes
`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_ENDPOINT_URL_S3` and
`BUCKET_NAME` as secrets, which is exactly what the engine reads. For
Cloudflare R2 instead, set them yourself:

```sh
fly secrets set \
  AWS_ACCESS_KEY_ID=... \
  AWS_SECRET_ACCESS_KEY=... \
  AWS_ENDPOINT_URL_S3=https://<account>.r2.cloudflarestorage.com \
  AWS_REGION=auto \
  AWS_S3_FORCE_PATH_STYLE=true \
  DBCENTER_BUCKET=my-db
```

`fly.toml` in the repository root has the rest: the machine stops when the last
connection closes and Fly's proxy starts it again on the next one.

### Sizing it

- **Volume.** With `DBCENTER_LAZY_RESTORE` the machine holds recent changes
  rather than the table, and 3 GB is generous - the measured footprint for
  200,000 rows was 39 MB, most of it PostgreSQL's own cluster. Without lazy
  restore, size it like an ordinary database volume.
- **Memory.** 256 MB runs a plain node. `DBCENTER_COLUMNAR` takes the resident
  set from about 128 MB to 250 MB, so budget 512 MB with it on. **Compaction
  needs more than that:** it reads the whole base snapshot back to rewrite it, and
  a million-row table did not fit in 1 GB. Either give the node that compacts
  2 GB, or set `DBCENTER_COMPACT_INTERVAL_SECS=0` on the serving machines and run
  compaction on a separate, larger one.
- **Region.** Put the machine where the bucket is. Every read the local tables
  cannot answer is a round trip, and that is the number that decides whether the
  lazy arrangement feels fast or slow.

### Scale to zero

Fly stops a machine with no connections and starts it on the next one. What
makes that safe here is that the bucket, not the disk, is the durable copy: the
engine flushes on SIGTERM and restores at boot. What makes it usable is that the
boot is quick - a measured 4.7 s for a lazy restore, 6.3 s for a full one at
200,000 rows.

If the machine is going to be stopped a lot, keep `DBCENTER_PULL` off unless
several nodes write to the same bucket. A single writer that restores at boot is
already current, and pulling again on every pass buys nothing.

## Extensions

`db.install_extension("postgis")` downloads a build matched to the running
PostgreSQL, checks it against a pinned SHA-256, and unpacks it - no compiler and
no system packages. PostGIS, pgvector, pg_trgm, pgsql-http and pg_duckdb are
published for PostgreSQL 12 to 18 on Linux; Windows has a subset.

Set `DBCENTER_EXTENSION_BASE_URL` to serve them from your own mirror; the digest
is still checked against the one compiled in.
