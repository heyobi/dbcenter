# Where things stand

Last updated 8 September 2026.

Everything below is on `fix/linux-extensions-and-query-safety`, **not merged to
`main`**. Twelve commits. Each one says what was wrong and how it was checked.

## What was broken and now is not

The published product did not work. `pip install dbcenter` raised on the first
call, because the wheel does not carry the `dbcenter-watcher` binary the engine
looks for. On Linux no extension could load at all: `dynamic_library_path` was
joined with `;`, the Windows separator, so PostgreSQL read the whole list as one
nonexistent directory. PostGIS and pgvector were served from a CDN that is off.

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

**The PyPI package is stale and broken.** 0.1.0, published 12 December 2025,
predates all of this. It has no macOS or arm64 wheel, an empty description, and
does not carry the watcher binary, so it raises on construction. Republishing is
the single highest-leverage thing left: everything above is invisible until
someone can install it.

**No Linux PostGIS below PostgreSQL 18 bundles its dependencies.** Only the 18
build was rebuilt. The pg12-pg17 archives still carry the modules alone, so they
only load where GEOS and PROJ are already installed. `build-systems/postgis-pg18`
in the extensions repo is the recipe; the others need the same treatment.

**`vector-0.8.1-windows-amd64-pg15` is quarantined.** It shipped an import library
instead of a DLL. Windows PostgreSQL 15 resolves to pgvector 0.8.0 instead.

**Restoring from object storage downloads everything.** `pull_and_apply` fetches
every Parquet object in full - and fetches each one twice, once whole just to
learn its length and then again in ranges. Cold start therefore grows with the
data, which contradicts the "query 300 GB without downloading it" claim. The
smart reader that would make that true (`scan_smart`, row-group pruning) is
reachable only from Python's `smart_query(table, id)`, a single-key lookup.

**Conflict resolution is last-write-wins on `updated_at`.** Two nodes editing one
row concurrently: the later edit wins and the earlier is lost. Fine for
append-heavy and partitioned work, wrong for a shared counter. `DBCENTER_PULL` is
off by default for this reason.

**The bindings other than Python are barely exercised.** Java and Node compile
again but have no tests. R is not in the workspace.

**Credentials are still in git history.** `client_secret.json` and
`token_cache.json` were removed from the tree but remain in earlier commits. The
OAuth client they belong to has been deleted upstream, so they are inert, but
purging them means rewriting history and force-pushing, which nobody has approved.
A PyPI API token also sits in the owner's Drive as `dbcenter_archive/pypi-*.txt`.

## Next, in order

1. Republish to PyPI: bundle the watcher, add macOS and arm64 wheels, fill in the
   description, and add an install smoke test to CI that does `pip install`,
   constructs a database and calls `fuzzy_search` in a clean container.
2. Merge this branch.
3. Rebuild the pg12-pg17 Linux PostGIS archives with bundled dependencies.
4. Decide what "lazy sync" should mean, and either make the restore path lazy or
   stop claiming it.

## Deliberately parked

Fly.io. `dbcenter-serve`, `Dockerfile`, `fly.toml` and the two docker compose
scripts are written and pass - one node's state survives destroying that node,
and two live nodes converge through the bucket. It is parked because it competes
with Neon, costs money to run, and nothing depends on it yet.
