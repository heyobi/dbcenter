# Where things stand

Last updated 16 October 2026.

Each commit says what was wrong and how it was checked.

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
- A compaction streams. It read the whole base into memory, copied it to sort it
  and built one Arrow batch from the copy, so a million rows peaked at 1,563 MB;
  it reads 50,000 rows at a time through a cursor now, sorted by PostgreSQL, and
  peaks at 215 MB. Its bloom filters were sized for a million values per
  20,000-row group and it wrote no compression, so the base file for those rows
  was 586 MB, 450 MB of it empty filters; it is 68 MB. A lookup against it reads
  the footer and the one row group that can hold the id, in three requests.

**A synced collection reads like a table now.** It was a view, `_adds UNION ALL
(_base minus _adds minus _dels)`, and PostgreSQL cannot push an ORDER BY ...
LIMIT through the anti-join, so a hundred rows by timestamp sorted the whole
collection. It is now one table partitioned on a `_layer` column into `_base` and
`_adds`, and the same query is `Limit -> Merge Append` over two index scans.

Measured on the same 200,000 rows (180,000 synced) built by 0.3.0 from PyPI and
then opened by this build, server-side, best of seven, 4 cores and no memory
limit - so not comparable with the 1-core rig below:

| | 0.3.0 (view) | now (partitioned) |
|---|---|---|
| 100 rows by timestamp | 46.7 ms | **0.07 ms** |
| count+avg over the table | 110.4 ms | **18.9 ms** |
| point lookup by id | 0.027 ms | 0.019 ms |

Both builds return the same 200,000 rows, the same sum and the same newest
hundred ids. The aggregate gets faster as well, as `scripts/shape-experiment`
predicted: the anti-join was costing every scan, not only the ordered ones.

The cost is that a partitioned table's key must include the partition column,
so nothing stops an id from being in both partitions, and then the collection
shows it twice. That is guarded where it can happen, and
`tests/delta_layout.rs` provokes each case. Each of the four guards was removed
in turn, and each time exactly its own test failed:

- an update through the collection moves a synced row from `_base` to `_adds`
  (a trigger sets `_layer`; PostgreSQL moves the row), so it is synced again -
  updated in place it would have stayed in `_base` and never been sent;
- an insert through the collection with an id already in `_base` takes that row
  out, as the view used to by preferring `_adds`;
- a pull skips an id with a pending local edit, recording the incoming version
  in `_conflicts`, and skips an id deleted here. The view hid a deleted row
  while leaving it in `_base`; now the row is really gone and its id in `_dels`
  is the only thing stopping a pull from bringing it back.

The insert guard is a statement trigger on the collection only. As a row
trigger it doubled the cost of an insert; `insert_one` writes to `_adds` by name
and never passes an id, so it now pays nothing (18.2 us a row on the server). A
client inserting through the collection over SQL pays one trigger call a
statement: 36.0 us against 19.5 for a single row, 267 ms against 209 for 20,000.

Existing databases convert themselves when they open, one transaction per
collection: 112 ms for 200,000 rows. On opening rather than on first use,
because a wire-protocol client never goes through `collection()`. Checked on
PostgreSQL 18 and 12, the newest and oldest majors offered.

## What is still wrong

**`fuzzy_search` finds nothing on a synced collection, and `create_index`
indexes nothing.** `field_as_text` sees the `data` column and addresses
`data->>'field'`, but `insert_one` stores fields in their own columns, so `data`
is null. Verified: the same document is found with sync off and not with it on.
Not new - it was the same through the view.

**Delta mode on PostgreSQL 12 needs pgcrypto** for `gen_random_uuid()`, which
became built-in in 13. Nothing installs it. Not new either; found while checking
the partitioned shape on 12.

**PyPI is at 0.4.0**, published 16 October 2026, with wheels for five
platforms: the partitioned collection. Verified from a real `pip install` in a
clean environment, outside the repository: a synced collection is partitioned,
an ordered `LIMIT` plans as a merge append, an update to a synced row - over SQL
and through the API - is pending again and synced again, `find` does not show
`_layer`, and no row is lost on the way.

0.3.0, published 8 September, was verified the same way for the sync round trip
and the columnar view (19x on a 200,000-row aggregate); it is the view-shaped
collection, and a database it wrote converts itself when 0.4.0 opens it. 0.1.0
and 0.2.0 are still on PyPI; 0.2.0 loses rows on sync and 0.1.0 cannot load any
extension on Linux.

**Windows CI is red, and the bug it found is real.** PostgreSQL refuses to run
under an administrator token, and the engine starts `postgres.exe` directly
instead of through `pg_ctl`, which drops to a restricted token on Windows - so a
Windows user running as administrator cannot start a database. 0.3.0 has it too.
The job had been failing at clippy since at least September, so its tests never
got far enough to show it. Left for the next release.

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

**A small machine and a bucket are cheaper, and the read path is faster.**
Measured on one core and 1 GB against a plain PostgreSQL 18 with the same limits and the same indexes, 200,000 rows:
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

Two things came out of it that were not good news. A hundred rows by timestamp
took 0.7 ms against a plain table and 103.8 ms against the collection, with
the same index on both sides, because the collection was a view - fixed since,
above, though this rig has not been re-run. And at a million rows a compaction
did not fit in 1 GB - it read the whole base back to rewrite it - fixed since as
well: it streams the base through a cursor now, and peaked at 215 MB where it
took 1,563 MB, measured on the same million rows. The full write-up, with the
rig, is in `docs/cost-and-scale.md`.

## Next, in order

1. **Give the Go, Java, Node and R bindings a test each, or stop shipping them.**
   Marked experimental is honest; it is not a resting place.
2. Build pg_duckdb from source rather than repackaging upstream's binary.
   `build-systems/pg_duckdb` is written and correct; DuckDB wants more memory to
   compile than this machine has spare.
3. Nothing more on "lazy sync": it means a pull that reads the deltas and skips
   the base snapshot, which is what `SyncMode::Partial` has always done. It is
   written down now, in the enum and in the README, so it stops being a phrase
   that means whatever the reader assumes.
4. `vector-0.8.1-windows-amd64-pg15` is still quarantined, and Windows
   PostgreSQL 15 silently gets pgvector 0.8.0 instead.

## How the two read paths fit together

This is the thing to understand before changing anything about storage.

`<name>` is the collection: an ordinary PostgreSQL table partitioned into `_base`
and `_adds`.
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
