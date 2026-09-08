# Where the read advantage went

Measured on this machine: PostgreSQL 18.6, 5,000,000 rows, five fields
(`id`, `customer_id`, `amount`, `region`, `created_at`), Parquet on a local
MinIO reached over HTTP, DuckDB 1.4 through pg_duckdb 1.1. Best of three runs.

## The claim being tested

Keeping data as Parquet on object storage was supposed to make reads *faster*
than PostgreSQL, not just cheaper. It does - but only if the Parquet is
actually columnar.

## What the numbers say

| Aggregate over 5M rows | PostgreSQL heap | Parquet, one column per field | Parquet, DBCenter's shape today |
|---|---|---|---|
| `sum(amount)`             | 586 ms | 46 ms | 1070 ms |
| `group by region`         | 2336 ms | 17 ms | 968 ms |
| `count(distinct cust_id)` | 4084 ms | 18 ms | 1025 ms |
| bytes on object storage   | -      | 70 MiB | 150 MiB |

Against PostgreSQL, columnar Parquet is **12.7x faster**. DBCenter's Parquet as
written today is **0.55x** - that is, 1.8x *slower* than the database it was
supposed to beat, while costing 2.1x the storage.

## Why

`sync_engine::rows_to_record_batch` writes four columns:

```
id: Utf8, data: Utf8, created_at: Utf8, updated_at: Utf8
```

The entire document is serialised into `data` as one JSON string. Parquet then
has nothing to be columnar about. Every query has to read every byte of every
document and parse JSON out of it, so none of what the format is for - reading
only the columns a query names, per-column compression, statistics that let a
row group be skipped unread - can happen. The 23x to 58x gap between the two
Parquet shapes above is entirely that.

## What this means for the architecture

The columnar path is worth having: 12.7x on aggregates, off object storage, on
data PostgreSQL never has to hold. That is the low-cost-Postgres story working
as intended.

But it is not a free upgrade to every query. Reading the same 5M rows:

| | |
|---|---|
| point lookup by id, PostgreSQL index scan | 0.2 ms |
| point lookup by id, Parquet on object storage | 38 ms |

PostgreSQL is 222x faster at the thing it is good at. So the two paths have to
stay separate, and something has to choose between them.

They cannot be merged into one view. A view that unions a PostgreSQL table with
a `read_parquet` call routes the *whole* query to DuckDB, and DuckDB scans the
PostgreSQL side sequentially because it cannot use PostgreSQL's indexes:

```
point lookup on the hot table directly        0.1 ms
the same lookup through a unified view       55.2 ms
```

`EXPLAIN` shows why: `PGDUCKDB_POSTGRES_SCAN ... ~100,000 rows` where an index
scan would have touched one. A single unified view would be a 550x regression
on the most common operation in the product, so the collection's own view stays
pure PostgreSQL - which is also what keeps a plain client like DBeaver seeing an
ordinary database.

## End to end, through DBCenter's own API

`cargo run -p dbcenter-core --example columnar_e2e 1000000` writes a million
documents through the collection API, syncs them to a MinIO bucket, installs
pg_duckdb, publishes the columnar view and queries both paths from the same
connection:

```
PostgreSQL, the collection's own view             1039.8 ms
DuckDB, the same rows as Parquet                    22.7 ms
  -> columnar is 45.74x the speed of postgres
both paths return identical results over 12 groups
```

The client sees two ordinary views on one ordinary PostgreSQL connection. The
`read_parquet` call and DuckDB's untyped `r['col']` accessor are inside the
view definition, so what a person writes in DBeaver is plain SQL over plain
columns.

## What deduplication costs, and when it is needed

A row can appear in two Parquet files: an update writes it to `_adds` again, and
the next sync puts it in a new file. Within a single file it cannot, because
`_adds` has `id` as its primary key. So the view is built from what is actually
in storage:

| in storage | view does | cost |
|---|---|---|
| one file | nothing | 1x |
| compacted base + deltas | rank the deltas, anti-join their ids out of the base | 2.3x |
| several deltas, no base | rank the deltas | up to 12.9x |

Ranking every row - the obvious "one window over all the files" - was the first
version, and it cost 27x on the million-row run above: 22.7 ms became 608.5 ms.
Sorting a million rows to discover that none of them were duplicates is most of
the query.

## What the columnar view does not do

It is as current as the last sync. The view reads Parquet, Parquet holds what
`sync_table` wrote, so a row inserted since then is in the collection and not in
the view:

```
writing a row without syncing...
  collection sees it: 1, columnar sees it: 0
```

There is no cheap way around this. Reading the unsynced rows means putting a
PostgreSQL table into the view, and that sends the whole query back through
DuckDB - the 550x problem above, for the sake of rows the collection can answer
for in 0.2 ms. Aggregate over the collection when the answer has to include this
second's writes; use the columnar view for the history, which is what it is
large enough to be worth reading fast.

## One difference to expect between the two paths

`sum()` over a float column does not come back bit-identical. PostgreSQL and
DuckDB aggregate in different orders, and floating-point addition is not
associative:

```
postgres: 330013.1999999999
columnar: 330013.2
```

That is 1e-10 on a sum of 16,667 values, and it is arithmetic, not a bug in
either. Round before comparing the two paths, or use `numeric` for anything
where the last bits matter - money, most obviously, which is why the sync writes
`numeric` as text rather than quietly turning it into a double.

## Turning it on

```python
db = dbcenter.DBCenter(path="/data", multi_user_sync=True, columnar=True)
```

That is the whole thing. pg_duckdb is put in place before PostgreSQL starts, so
`shared_preload_libraries` is read at the first startup and there is no restart
to absorb; `<name>_columnar` appears beside each collection as soon as there is
something synced to read, and is rebuilt whenever the set of Parquet objects
changes.

| | |
|---|---|
| first database on a machine | 26.3 s - downloading and unpacking 50 MB |
| every database after that | 2.7 s, the same as without it |
| resident memory | about 250 MB against 128 MB |

It is off by default because of that last row. Doubling the footprint of an
embedded database is not something to do to someone who never runs an aggregate.
