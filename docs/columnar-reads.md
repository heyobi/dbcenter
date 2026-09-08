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
