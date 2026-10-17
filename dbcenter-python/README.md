# DBCenter

<!-- mcp-name: io.github.heyobi/dbcenter -->

A real PostgreSQL, embedded in your process. No server to install, no Docker, no
connection string to configure - the binaries are fetched once and the database
runs as a child process against a directory you choose.

```python
from dbcenter import DBCenter

db = DBCenter(path="./data")
users = db.collection("users")
users.insert_one({"name": "Ada", "age": 36})
print(users.find({"age": {"$gt": 30}}))
```

Because it is PostgreSQL rather than an imitation of it, the extensions come
too. PostGIS and pgvector are downloaded and installed on first use:

```python
db.install_extension("postgis")
db.query("""
    SELECT ST_Distance(
        ST_SetSRID(ST_MakePoint(29.0, 41.0), 4326)::geography,
        ST_SetSRID(ST_MakePoint(32.85, 39.93), 4326)::geography
    ) / 1000 AS km
""")

store = db.vector_store("documents", dimension=1536)
store.add(ids=["a"], embeddings=[[0.1] * 1536])
```

Fuzzy text search is `pg_trgm`, which ships with PostgreSQL:

```python
db.collection("cities").fuzzy_search("Istanbu", "name")
```

## What you get

- **A real PostgreSQL.** Its planner, its transactions, its types. `db.query()`
  takes any SQL, with bound parameters.
- **PostGIS and pgvector**, installed on demand. The Linux PostGIS build carries
  its own GEOS and PROJ, so it works on a machine with nothing installed.
- **A collection API** over ordinary relational tables. Columns are added as
  documents introduce fields.
- **Sync to storage**, optionally, with `multi_user_sync=True` and a
  `SyncEngine`: a local directory or Google Drive from Python, and S3 or anything
  S3-compatible through the `dbcenter-serve` container. Tables are written as
  Parquet with a column per field, and `columnar=True` reads them back through
  DuckDB for aggregates.

## Notes

The PostgreSQL distribution is downloaded to `~/.dbcenter` on first use, about
60MB, and reused after that. Extension archives are verified against digests
pinned in the source, so a substituted one is refused.

PostgreSQL refuses to run as root, so neither can this. In a container that runs
as root - which most base images do - create a user first and start the database
as them; the error says so if you forget.

In Google Colab, which runs everything as root, create a user and run your
script as them, with a data directory they can write to:

```bash
!id -u dbuser >/dev/null 2>&1 || useradd -m dbuser
!sudo -u dbuser python3 /path/to/your_script.py
```

Extensions are built per PostgreSQL major version. Asking for one with no
published build is an error naming the versions that exist, rather than a
download that fails later.

MIT licensed. Source at [github.com/heyobi/dbcenter](https://github.com/heyobi/dbcenter).
