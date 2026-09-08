# Where things stand

Last updated 8 September 2026.

Merged to `main` on 8 September 2026. Each commit says what was wrong and how
it was checked.

## What was broken and now is not

On Linux no extension could load at all: `dynamic_library_path` was joined with
`;`, the Windows separator, so PostgreSQL read the whole list as one nonexistent
directory. PostGIS and pgvector were served from a CDN that is off.

An earlier version of this file said the published wheel raised on construction
and did not carry the watcher binary. That was wrong. The watcher is embedded in
the extension module by `build.rs` and written out at run time, and 0.1.0 installs
and works. What produced the failure was running from a directory containing a
folder named `dbcenter-watcher` - the repository root, for instance - because the
path search used `exists()`, which is true for a directory. That is now `is_file`.

Fixed and verified against a real server:

- Extensions load on Linux. `pg_trgm`, `pgvector` and PostGIS all install from the
  public release and work - fuzzy match 0.875, vector distance 2.828, PostGIS 3.6
  with GEOS and PROJ computing Istanbul to Ankara as 347.4 km.
- Extension distribution moved off the dead CDN to GitHub Releases, with no shared
  secret in the client and every archive checked against a pinned SHA-256.
- PostGIS on Linux has a PostgreSQL 18 build, and bundles the libraries it links
  against so it loads on a machine with nothing installed.
- An unrecognised query operator, or `$in` with a non-array, is an error. Both used
  to contribute no condition, so `delete_many` removed every row.
- Identifiers are quoted; a field name can no longer terminate one and inject SQL.
- `fuzzy_search` and `create_index` resolve the collection's actual layout.
- `query(sql, params)` in Python binds its parameters. It used to discard them.
- `query()` no longer returns null for NUMERIC, timestamps, bytea and arrays.
- TCP listening works on Unix, auth fails closed, and the engine starts offline.
- The workspace compiles: the Java and Node bindings had drifted out of sync with
  `DBCenter::new` and nothing built them.

## What is still wrong

**PyPI is at 0.3.0**, published 8 September 2026, with wheels for five
platforms. Verified from a real `pip install` in a clean environment: the sync
round trip keeps the document, a synced update does not lose the row, the
Parquet has a column per field, and `install_extension("pg_duckdb")` downloads
from the public release in about five seconds and gives a columnar view 19x
faster than the collection on a 200,000-row aggregate. 0.1.0 and 0.2.0 are still
on PyPI; 0.2.0 loses rows on sync and 0.1.0 cannot load any extension on Linux.

**Linux PostGIS is being rebuilt with bundled dependencies.** The published
archives carry the modules alone, so they only load where GEOS and PROJ already
are. PostgreSQL 12, 13, 14, 16 and 18 are rebuilt; 15 fails to compile and 17 is
still going. `build-systems/postgis-pg18` in the extensions repo is the recipe.

**`vector-0.8.1-windows-amd64-pg15` is quarantined.** It shipped an import library
instead of a DLL. Windows PostgreSQL 15 resolves to pgvector 0.8.0 instead.

**Object storage is columnar now, and it was the whole read story.** Everything
synced used to go into four Parquet columns with the document serialised into
one of them as JSON. That left Parquet nothing to be columnar about: measured
over five million rows, reads off object storage were 1.8x *slower* than the
PostgreSQL they were meant to beat, at 2.1x the stored bytes. It is one column
per document field now, with the type the field had, and
`<table>_columnar` publishes those files as an ordinary PostgreSQL view through
pg_duckdb - 45.7x on a million-row aggregate, end to end through DBCenter's own
API, with both paths returning identical results. `docs/columnar-reads.md` has
the numbers and the method.

Three separate places wrote or read the four-column shape and two were losing
data outright: `sync_table`'s move to `_base` inserted four columns, so a synced
row arrived locally with no document at all; `vacuum`'s base export did the same,
so a compaction replaced the deltas with an empty base; and `pull_and_apply` read
columns by position. All three are fixed, and
`synced_parquet_has_a_column_per_field` pins the shape.

Worth knowing why it went unnoticed for so long: the cluster tests wrote through
psql directly into `data`, which is the one path that worked.

**Reading remotely still costs more than it should.** A single-id lookup now reads
31% of a 15MB table instead of 100%, and the fraction falls as the table grows
because a row group holds a fixed number of rows. Two things would take it
further: reading only the `id` column to locate a match before fetching the row,
and covering the delta objects rather than only the newest base snapshot.
`scan_smart` is also reachable only from Python's `smart_query(table, id)` and
only does equality on `id`, so the mechanism works but almost nothing can reach
it. `CountingStorage` and `tests/lazy_read.rs` are how any of this is measured.

**Conflict resolution is last-write-wins on `updated_at`.** Two nodes editing one
row concurrently: the later edit wins and the earlier is lost. Fine for
append-heavy and partitioned work, wrong for a shared counter. `DBCENTER_PULL` is
off by default for this reason.

**The bindings other than Python are barely exercised.** Java and Node compile
again but have no tests. R is not in the workspace.

**The test suite was the reason most of this survived.** The Python suite has 27
assertions against 83 prints, and never calls fuzzy_search, delete_many,
update_many, find, create_index or lazy at all. It checks that things do not
crash. Every bug found in this work was either in a function it never called or
on a path no test provokes: an unrecognised operator, a type with no mapping, a
machine without GIS libraries, a document written through the API rather than as
JSONB. When adding a feature here, add the test that would fail without it, and
one that provokes the wrong input.

**Credentials are still in git history.** `client_secret.json` and
`token_cache.json` were removed from the tree but remain in earlier commits. The
OAuth client they belong to has been deleted upstream, so they are inert, but
purging them means rewriting history and force-pushing, which nobody has approved.
A PyPI API token also sits in the owner's Drive as `dbcenter_archive/pypi-*.txt`.

## Next, in order

1. **Decide whether the columnar view should be automatic.** It exists, it is
   refreshed when the files change, and both `install_extension("pg_duckdb")`
   and `ensure_columnar_view` are reachable from Python - but a user still has
   to ask for both. Doing it on their behalf means installing a 50 MB extension
   and restarting their database, which is not something to do unasked.
2. **`scan_smart` is now the slow way to do what pg_duckdb does better.** It is
   a hand-rolled Parquet reader that only does equality on `id`, reachable only
   from Python's `smart_query`. Either point it at the columnar view or drop it.
3. Rebuild the pg12-pg17 Linux PostGIS archives with bundled dependencies.
4. Build pg_duckdb from source rather than repackaging upstream's binary.
   `build-systems/pg_duckdb` is written and correct; DuckDB wants more memory to
   compile than this machine has spare, and it fails whenever anything else is
   running.
5. Decide what "lazy sync" should mean, and either make the restore path lazy or
   stop claiming it.

## How the two read paths fit together

This is the thing to understand before changing anything about storage.

`<name>` is the collection: an ordinary PostgreSQL view over `_base` and `_adds`.
Writes go here, point lookups go here - 0.2 ms against 38 ms off object storage -
and it is what a client connected with a plain connection string sees.

`<name>_columnar` is the same rows read from Parquet by DuckDB. Aggregates go
here.

They are deliberately not one view. pg_duckdb takes a whole query or none of it,
and DuckDB cannot use PostgreSQL's indexes, so a view unioning the two sends
every query through DuckDB and turns a 0.1 ms lookup into 55 ms - a 550x
regression on the most common operation in the product. Two names, each good at
what it is for.

## Deliberately parked

Fly.io. `dbcenter-serve`, `Dockerfile`, `fly.toml` and the two docker compose
scripts are written and pass - one node's state survives destroying that node,
and two live nodes converge through the bucket. It is parked because it competes
with Neon, costs money to run, and nothing depends on it yet.
