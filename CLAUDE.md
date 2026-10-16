# DBCenter

PostgreSQL embedded in a process. A Rust wrapper starts a real `postgres` binary
against a data directory, exposes a MongoDB-shaped API over it, and can sync the
result to object storage. It is an integration layer, not a database: storage,
planning and transactions are all PostgreSQL's.

Read `STATUS.md` first. It says what is done, what is broken, and what is next.

## Layout

| | |
|---|---|
| `dbcenter-core` | the engine: process lifecycle, collections, delta sync, storage adapters |
| `dbcenter-ffi` | C ABI |
| `dbcenter-python` | PyO3 bindings, the binding that matters |
| `dbcenter-go` / `-java` / `-node` / `-r` | thin, barely exercised; treat as experimental |
| `dbcenter-watcher` | sidecar that shuts PostgreSQL down when the parent dies |
| `dbcenter-serve` | the engine as a container workload, with object storage behind it |

## Things that will bite you

**Two physical layouts, and code has to know which.** A plain collection is
relational: each document field becomes its own column, added by `ALTER TABLE` on
insert. Delta-sync and geo collections instead keep the body in a `data` JSONB
column. Anything writing SQL against a collection must resolve which it is -
`Collection::field_as_text` does this. Assuming one layout is how `fuzzy_search`
and `create_index` came to be broken for every ordinary collection.

**In delta mode a collection is a partitioned table**, `<name>`, with
`<name>_base` (synced) and `<name>_adds` (pending) as its partitions on a
`_layer` column, and `<name>_dels` beside it. It used to be a view, which made
every `ORDER BY ... LIMIT` sort the whole collection. Derive collection names from
the `_adds` tables: that worked for the view and still does, and the sync loop
once found nothing to push because it went looking elsewhere.

**An id must be in only one partition.** A partitioned table's key has to include
the partition column, so nothing in PostgreSQL stops an id from being in both -
and then the collection shows the row twice. Triggers keep it out of both for an
update or an insert through `<name>`, and the restore skips any id pending in
`_adds` or deleted here (`_dels`). Anything new that writes to `_base` or `_adds`
directly has to do the same; the sync engine sets `dbcenter.replicating` so the
triggers stand aside for its own moves. `tests/delta_layout.rs` provokes each case.

**Columns are added to `<name>`, never to a partition.** PostgreSQL refuses the
latter and propagates the former.

**There are two read paths and they are separate on purpose.** `<name>` is the
collection - ordinary PostgreSQL, where writes and point lookups go.
`<name>_columnar` is the same rows read from Parquet by DuckDB, where aggregates
go: 45.7x on a million-row aggregate. Do not be tempted to union them into one
view. pg_duckdb takes a whole query or none of it and DuckDB cannot use
PostgreSQL's indexes, so a unified view sends every query through DuckDB and
turns a 0.1 ms point lookup into 55 ms. `docs/columnar-reads.md` has the
measurements.

**Anything written to Parquet gets one column per document field.** The
temptation is to serialise the document into a single `data` column, because
that is what a document store does and the code is shorter. It was that way, and
it made reads off object storage 1.8x *slower* than PostgreSQL at 2.1x the
bytes, because Parquet then has nothing to be columnar about. `sync_columns`
reads the real columns from the catalog; three separate code paths used to
hard-code the four-column shape and two of them silently dropped the document.

**Extensions are per-PostgreSQL-major and there is no fallback.** PostgreSQL
stamps every loadable module with the major version it was built against and
refuses to load one that disagrees. Asking for a version with no published build
is an error, deliberately, not a download of the nearest older build.

**Extension archives are verified against digests pinned in
`EXTENSION_CHECKSUMS`.** They are unpacked into the PostgreSQL installation and
loaded into the server process, so a substituted one is arbitrary code execution.
An unpinned or mismatched asset is refused. Publishing a new build means
uploading it to the `extensions-v1` release on
[dbcenter-extensions](https://github.com/heyobi/dbcenter-extensions) *and* pinning
its SHA-256 here; `every_offered_version_has_a_published_asset` fails otherwise.

**A bundled extension's libraries go in a directory of its own.** Every archive
carries the libraries its module links against, and they used to be unpacked
straight into PostgreSQL's module directory - which every extension shares.
PostGIS and pg_duckdb bundle twenty-six of the same sonames from different base
images, so installing both left one of them with a library it was never linked
against, and the server refused to start on an undefined symbol. The module
still goes in the module directory, because `$libdir/<name>` has to find it; its
dependencies go in `<name>-libs/` beside it, with the module's RUNPATH pointing
there. `examples/two_extensions.rs` installs both, in both orders, and checks
they both still answer.

**Linux PostGIS bundles its own GEOS and PROJ**, with an `$ORIGIN` RUNPATH. Without
that it does not load on a machine that has no GIS libraries installed - which is
most machines. Test extension work in a container with nothing installed, never
only on this box.

**Never interpolate an identifier into SQL.** Use `query::quote_ident`. Field
names reach the query builder from user data.

**An unrecognised query operator is an error, not a dropped condition.** Dropping
it turned `delete_many({"age": {"$typo": 1}})` into `DELETE FROM t`.

**Republishing an extension archive under the same name needs two things to
notice.** The download cache and the "already unpacked" marker both keyed on the
file name, so a machine that had installed the old bytes would fail verification
for good and would never extract the new layout. Both key on the pinned digest
now - if you add another cache along this path, do the same.

**Anything in a conflict tie-break has to be identical on every node.**
`created_at` is not: the row an upsert tries to insert takes the column default,
which is the local clock. Including it made both nodes accept the other's row,
which is the same divergence in the opposite direction.

**Never hold a pooled connection while calling something that takes another.**
The pool is sized from `available_parallelism`, which is 1 inside a one-core
container - the cheap tier this is built for - and one connection is enough to
deadlock the engine against itself. A sync held its transaction guard past the
commit and then refreshed the columnar view, and the node hung. The floor is now
two connections, but the floor is a seatbelt, not the fix: drop the guard first.

**A wire-protocol client cannot add a field to a collection.** Schema evolution
happens inside `insert_one`; a client connected over TCP can only insert into
the view, whose columns were frozen when it was created. Everything it writes
then lands in the `data` JSONB column - which works, and quietly costs the
columnar read path its whole advantage, because the Parquet holds one JSON
string per row instead of one column per field. `dbcenter-serve` takes
`DBCENTER_SCHEMA` for this reason.

**A Python `Collection` reads any unknown attribute as a column.** `coll.name`
does not give you the collection's name, it gives you the values of a field
called "name". Tests keep the name in a variable.

## Working on it

Rust is at `~/.cargo/bin` (rustup). Always:

    cargo fmt --all
    cargo clippy -p dbcenter-core -p dbcenter-serve --all-targets -- -D warnings
    cargo test --workspace -- --test-threads=1

`--test-threads=1` matters: the integration tests each boot a real PostgreSQL.
`scripts/verify.sh` runs all of it, including bringing up MinIO for the S3 tests.

**This machine has 11 GB and 4 cores.** The release profile uses fat LTO and one
codegen unit, which gets killed by the OOM reaper - `dbcenter-python` especially.
Use a debug build to verify, or set `CARGO_PROFILE_RELEASE_LTO=thin` and
`CARGO_BUILD_JOBS=2` as the Dockerfile does. Long builds and container work in
parallel will get background tasks killed; run them one at a time.

Python bindings, for testing a change end to end:

    VIRTUAL_ENV=<venv> maturin develop        # debug; --release will OOM
    PATH=$PWD/target/debug:$PATH python ...   # the watcher binary must be findable

## Traps in the environment, not the code

- `initdb` and `postgres` refuse to run as root, and a cloud container runs you as
  root: every integration test then fails with "cannot be run as root". Build as
  root, run the test binaries as `nobody` (`setpriv --reuid=nobody
  --regid=nogroup --clear-groups`) with a `HOME` it can write to, and copy the
  proxy's CA bundle somewhere it can read, or downloads fail.
- `pkill -f <pattern>` matches the shell running it. Use `ps -eo pid,args | grep '[p]attern'`.
- A Unix socket path cannot exceed 107 bytes. The engine falls back to a short
  temp directory now, but deep paths are still worth avoiding for scratch data.
- The S3 tests skip themselves without `DBCENTER_S3_TEST_ENDPOINT`, so a green run
  does not mean they ran. `scripts/s3-test-env.sh` points them at a local MinIO.
- MinIO keeps its data across `compose down`, so give each cluster-test run its own
  key prefix or it restores the previous run's rows and proves nothing.
