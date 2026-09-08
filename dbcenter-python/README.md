# DBCenter

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
- **Sync to object storage**, optionally: S3, anything S3-compatible, or Google
  Drive. Tables are written as Parquet, and a lookup reads the row groups it
  needs rather than the whole file.

## Notes

The PostgreSQL distribution is downloaded to `~/.dbcenter` on first use, about
60MB, and reused after that. Extension archives are verified against digests
pinned in the source, so a substituted one is refused.

Extensions are built per PostgreSQL major version. Asking for one with no
published build is an error naming the versions that exist, rather than a
download that fails later.

MIT licensed. Source at [github.com/heyobi/dbcenter](https://github.com/heyobi/dbcenter).
