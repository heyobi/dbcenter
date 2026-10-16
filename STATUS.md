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

**Linux PostGIS bundles its dependencies on every major.** Checked against the
published release: PostGIS 3.5.0 for PostgreSQL 12 through 17 and 3.6.1 for 18 -
which is what the version table picks by default for every major - all carry the
43 to 45 libraries they link against. Only an explicitly requested older version
(3.1.11, say) still ships the module alone and needs GEOS and PROJ on the host.

**PostGIS's libraries now live in `postgis-libs/`, like pg_duckdb's.** The seven
bundles that carry libraries - 3.5.0 for PostgreSQL 12 to 17, 3.6.1 for 18 - were
repacked with their 43 dependencies in a directory of their own and RUNPATH
`$ORIGIN/postgis-libs`, re-pinned, and published to `extensions-v1`. Nothing
shares the module directory any more, so the next bundle that ships a library
has nothing to collide with.

Two things had to change for a republished bundle to take effect at all, and
both were found by publishing this one. The download cache keyed on file name,
so a machine that had ever installed the old archive kept serving it from disk
and failed verification for good; a cached file that no longer matches its
pinned digest is now fetched again. And the "already unpacked" marker recorded
the archive's name, so the new layout would never have been extracted over the
old one; it records the digest. Installing over an old install also deletes the
copies the previous layout left behind. Checked from the public release with an
empty cache on PostgreSQL 17 and 18, both extensions, both orders: every
download matched its pinned digest, PostGIS answers 349.4 km and DuckDB answers
42, and the only library left loose in the module directory is PostgreSQL's own
`libpqwalreceiver.so`.

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

**`scan_smart` stays, as the lookup that needs no extension.** It reads Parquet
over range requests and only does equality on `id`, which the columnar view beats
on both counts - but the view needs pg_duckdb, which is published for Linux on
PostgreSQL 14 to 18 and adds 120 MB of resident set. `scan_smart` is pure Rust
and works on every platform DBCenter runs on, so it is the fallback rather than a
duplicate. It is not going to grow: anything past a point lookup belongs in the
view.

Writing its tests turned up that the footer cache it advertised never worked. It
saved only when a read ended at the end of the file and was longer than eight
bytes, and Parquet's metadata block ends eight bytes before the end, so the file
was never written and every lookup re-read the footer. The cache now records the
byte range it holds.

**Conflict resolution is still last-write-wins, but it no longer loses quietly
and no longer diverges.** Two nodes editing one row: the later edit wins and the
earlier one is written to `<collection>_conflicts` with both versions and which
one was kept, so it can be found afterwards. Nothing is recorded unless the two
versions actually differ, so re-applying a file leaves no rows behind.

The tie was the real bug. The comparison was `EXCLUDED.updated_at > base.updated_at`,
so on equal timestamps each node kept whatever it already had and the two stayed
different for good - a wrong answer, not a lost update. Ties are now broken on a
hash of the row (minus `created_at`, which is this node's clock), so every node
picks the same version. `tests/conflicts.rs` pins all three cases.

This is resolution, not merging. Two nodes incrementing a shared counter still
end at one increment. `DBCENTER_PULL` stays off by default.

**Everything except Python is marked experimental.** Go has one test file; Java,
Node and R have none. They are thin wrappers over `dbcenter-ffi` and CI keeps
them compiling, which is all anyone knows about them, and each one now says so in
its own README. Python is the supported binding: it has the test suite, it is
what is on PyPI, and features land there first. Anyone who needs another language
today should connect over the PostgreSQL wire protocol with an ordinary driver -
that path is tested.

**The Python suite asserts things now.** It was 27 assertions against 83 prints
in one file, with five more files that only checked that nothing raised. It is
now 70 tests across `test_collection.py`, `test_sync.py` and `test_columnar.py`,
plus the original `test_final.py`, and CI runs them on every push. Writing them
found four bugs in an afternoon - `db.port` raising on every default instance,
`$set` unable to introduce a field that `insert_one` would have added,
`create_index(field, "gin")` failing outright, and the dead footer cache - which
is the argument for them. Rust is at 48 tests across eleven files.

The pattern in every bug found this month: it was on a path no test called, or in
a call whose result nothing checked. So assert the value, not that the call
returned, and give each new feature a test that provokes the wrong input.

**Credentials are still in git history.** `client_secret.json` and
`token_cache.json` were removed from the tree but remain in earlier commits. The
OAuth client they belong to has been deleted upstream, so they are inert, but
purging them means rewriting history and force-pushing, which nobody has approved.
A PyPI API token also sits in the owner's Drive as `dbcenter_archive/pypi-*.txt`.

**A small machine and a bucket are cheaper, and the read path is faster - but the
delta view taxes ordinary queries.** Measured on one core and 1 GB against a
plain PostgreSQL 18 with the same limits and the same indexes, 200,000 rows:
an aggregate over the whole table is 188.7 ms against PostgreSQL's storage and
19.7 ms against the same rows as Parquet, and a lazy node holding none of the
table answers it in 46 ms on 39 MB of disk against PostgreSQL's 154 MB. On list
prices that is about $7.69 a month against Supabase Pro's $36.50 for the same
100 GB.

With the bucket a network away - 20 ms a round trip, which is what R2 or Tigris
costs from a Fly machine - the columnar view still wins, 77.4 ms against 192.8 ms,
but the lazy node stops being fast (225.5 ms) and a point lookup that reaches
object storage takes 216.4 ms against 0.4 ms. Lazy restore is for archival
tables, not for the table an API reads.

Read load scales by adding machines rather than by buying a bigger one: one core
answers as many queries as PostgreSQL's three (26.4 against 24.7 a second), and
three one-core nodes sharing a bucket reach 54.3 - 2.2x the same total compute
spent on one PostgreSQL, with less than half the latency at eight clients. The
nodes hold no data, so the read tier costs machines rather than machines plus a
volume each.

Two things came out of it that are not good news. A hundred rows by timestamp
takes 0.7 ms against a plain table and **103.8 ms against the collection**, with
the same index on both sides: the collection is a view over three tables and
PostgreSQL will not push the ordering and the limit through it. Every DBCenter
deployment pays that, synced or not. And at a million rows **a compaction does
not fit in 1 GB** - it reads the whole base back to rewrite it, and the container
stalls at its memory ceiling. The full write-up, with the rig, is in
`docs/cost-and-scale.md`.

## Handover: the `delta-inheritance` branch

Picked up on 16 October 2026 and left unfinished because the machine it was
being built on shuts itself down. Everything is pushed to
`origin/delta-inheritance`. It builds, `cargo fmt` is clean, and nothing is
merged to `main`.

### What the branch does

Item 1 below, built. A synced collection stops being a `UNION ALL` view over
`_base`, `_adds` and `_dels` and becomes a **table those two inherit from**, so
an ordered, limited read is a `Merge Append` over one index per child that stops
when it has enough rows.

**Inheritance, not declarative partitioning**, and the reason is worth keeping:
a partitioned table's key has to be a real column, which would put a `part_key`
into every `SELECT *` a caller writes, and its primary key has to contain that
key, so `ON CONFLICT (id)` - which the restore and the compaction both lean on -
would stop meaning what it says. Children of an inherited parent have exactly
the parent's columns and a primary key on `id` alone, which is why almost every
existing `INSERT INTO x_adds` / `SELECT FROM x_base` statement needed no change
at all. Measured side by side in `scripts/shape-experiment/run.sh`: inheritance
0.152 ms on the ordered read against partitioning's 0.134 ms and the view's
295.7 ms, and inheritance was the faster of the two on the aggregate.

### Where to turn it on

Off by default, so `main`'s behaviour is untouched until it is flipped.

| | |
|---|---|
| `DBCenter::builder().inherited_delta(true)` | per instance |
| `DBCENTER_INHERITED_DELTA=1` / `=0` | forces either way, overrides the builder |
| `DBCENTER_MIGRATE_DELTA=1` | `dbcenter-serve` converts existing collections at startup |

A collection keeps the shape it was created with. `DeltaManager::shape_on_disk`
reads the catalog, caches the answer, and `migrate_to_inherited` is the only
thing that changes it - so a data directory written by an older release still
opens and still works.

### The four things that had to change, and why

1. **The restore refuses rows the local node knows better about.** A row in
   `_adds` is an edit made here and not pushed; a row in `_dels` was deleted
   here. The view hid both of those behind its anti-joins. Nothing hides them
   now, so the upsert into `_base` carries `NOT EXISTS (_adds) AND NOT EXISTS
   (_dels)` and the discarded incoming version is written to `_conflicts`
   instead of silently appearing as a second copy of the row.
2. **Sync writes run as replication.** `ON CONFLICT (id) DO UPDATE` on `_base`
   is an UPDATE, so it fired the trigger that promotes an edited row into
   `_adds` - and the restore moved every row it touched straight back out of
   `_base`. Both sync transactions now `SET LOCAL session_replication_role =
   'replica'`. This cost two red tests in `conflicts.rs` to find and is the
   single least obvious thing on the branch.
3. **Columns go on the parent.** `ALTER TABLE parent ADD COLUMN` cascades to
   both children; altering the children would leave the parent - the table every
   read goes through - without the column. `Collection::ensure_columns` and
   `sync_engine::ensure_restored_column` both branch on the shape.
4. **Indexes go on the children.** An index never cascades, and in the old shape
   `create_index` aimed at the collection, which was a view - so it had simply
   been broken in delta mode. It now builds one index per child, which is also
   where the merge append needs it.

A latent bug in the **old** shape turned up on the way and is fixed on this
branch too: `before` in the restore's conflict CTE read the collection, and a
view freezes its column list when it is created, so a restore that was adding
columns tripped over its own view. It reads `_base` and `_adds` directly now,
with the same precedence a read would give them.

### What is verified and what is not

- **Full `dbcenter-core` suite in the new shape: green.** 55 tests, every file -
  `lib`, `collection_regression`, `concurrency`, `conflicts`, `geo_lookup`,
  `lazy_frame`, `lazy_read`, `offline_start`, `shape`, `value_mapping`. The S3
  tests self-skip without `DBCENTER_S3_TEST_ENDPOINT`, as always.
- Full suite in the **old** shape: green, except
  `shape.rs::an_ordered_limited_query_stops_when_it_has_enough_rows`, which is
  the test that describes the new shape and is meant to fail there. Decide
  whether to gate it on the flag or let flipping the default fix it.
- `cargo clippy -D warnings` and `cargo fmt`: clean.
- An unlooked-for result: `lazy_read` runs in **70.9 s** against 98.5 s in the
  old shape. The view tax was being paid inside the test suite too.
- Not run in the new shape at all: the Python suite, `dbcenter-serve`, the cost
  rig, and `migrate_to_inherited`, which has no test of any kind.

### Next, on a machine that stays up

```sh
export PATH="$HOME/.cargo/bin:$PATH"

# 1. the Python bindings, both ways
VIRTUAL_ENV=<venv> maturin develop
cd python_tests && python -m pytest
DBCENTER_INHERITED_DELTA=1 python -m pytest

# 2. a migration test that does not exist yet: build a collection in view
#    shape, put rows in all three tables, migrate, assert the same rows come
#    back exactly once. `migrate_to_inherited` is written and unexercised.

# 3. the fly.io rig, in the new shape, which is the number the change is for
docker compose -f scripts/cost-experiment/docker-compose.yml up -d storage storage-init
python scripts/cost-experiment/run.py --rows 200000      # old shape, for the baseline
DBCENTER_INHERITED_DELTA=1 python scripts/cost-experiment/run.py --rows 200000
#    `recent_window` is the row to watch: 103.8 ms today, and the scratch
#    experiment says it should land near PostgreSQL's 0.7 ms.
#    Then the 20 ms latency profile, then scale.py for the read tier.

# 4. only then flip the default to true in DBCenterConfig::default and
#    DBCenterBuilder::new, update README/CLAUDE.md/docs, and merge.
```

### Traps this branch adds

- **`write_table()` still writes straight to `_adds`**, bypassing the parent's
  route trigger. That is deliberate and fast, and it is safe only because
  `insert_one` strips a caller's `id`. Anything that starts honouring a
  caller-supplied id has to go through the parent instead, or it will put the
  same id in both children and the collection will show the row twice.
- **No trigger on `_adds`, on purpose.** The compaction deletes from it a batch
  at a time, and a per-row trigger there is exactly the cost this layer spent a
  week removing. Tombstones are written by a trigger on `_base`, which is the
  only table a pull can resurrect a row into.
- **`_dels` is local and is never pushed.** It was like that before; the
  inherited shape makes it load-bearing for the restore guard rather than for
  every read, so a deletion still does not propagate between nodes. Worth
  deciding on deliberately rather than inheriting by accident.

## Next, in order

1. **Finish the `delta-inheritance` branch.** Built, pushed, unmerged; the
   handover section above says exactly what is left. The prize, measured in
   `scripts/shape-experiment/run.sh` over 200,000 rows, best of seven on 1 core
   and 1 GB:

   | | plain table | view (today) | inherited (branch) |
   |---|---|---|---|
   | 100 rows by timestamp | 0.09 ms | 295.7 ms | **0.15 ms** |
   | count+avg over the table | 61.3 ms | 264.6 ms | **56.9 ms** |
   | point lookup, indexed | 0.10 ms | 0.21 ms | **0.12 ms** |
   | insert | - | 0.038 ms | 0.040 ms |
   | update a row held in base | - | 0.083 ms | **0.069 ms** |
   | delete | - | 0.068 ms | **0.034 ms** |

   Which is to say: the view tax disappears rather than shrinks, aggregates and
   point lookups are unaffected, and writes come out even or slightly better.
   This is the largest single gap between DBCenter and the PostgreSQL
   underneath it and every deployment pays it, synced or not.
2. **Make compaction stream.** It reads the whole base snapshot into memory to
   rewrite it, so a 1 GB machine cannot compact a million-row table - which is
   the machine the whole argument rests on.
3. **Give the Go, Java, Node and R bindings a test each, or stop shipping them.**
   Marked experimental is honest; it is not a resting place.
4. Build pg_duckdb from source rather than repackaging upstream's binary.
   `build-systems/pg_duckdb` is written and correct; DuckDB wants more memory to
   compile than this machine has spare.
5. Nothing more on "lazy sync": it means a pull that reads the deltas and skips
   the base snapshot, which is what `SyncMode::Partial` has always done. It is
   written down now, in the enum and in the README, so it stops being a phrase
   that means whatever the reader assumes.
6. `vector-0.8.1-windows-amd64-pg15` is still quarantined, and Windows
   PostgreSQL 15 silently gets pgvector 0.8.0 instead.

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
