# Can a small machine and a bucket stand in for a database server?

That is the question behind DBCenter, and it deserves an answer with numbers in
it rather than an argument. This is the answer as of 9 September 2026, measured
on one machine with `scripts/cost-experiment/`, which anyone can re-run.

## The rig

Three shapes, one workload, the same client, over the PostgreSQL wire protocol
so the client cannot tell which is which:

| | what it is |
|---|---|
| **postgres** | a plain PostgreSQL 18 with the whole table on its volume - what you rent from a managed provider |
| **dbcenter** | DBCenter with the whole table locally, plus a columnar view of the same rows in object storage |
| **lazy** | DBCenter restored from the deltas only, `DBCENTER_LAZY_RESTORE=1` - the history stays in the bucket |

Both database containers get **1 CPU and 1 GB**, which is Fly's shared-cpu-1x
with 1GB, the cheapest tier that can hold a working set at all. MinIO stands in
for R2 or Tigris. The table is 200,000 rows of five columns - two integers, a
float, a short string and a 128-character random payload - with the same indexes
on both sides. The payload is md5 output rather than repeated characters on
purpose: a column of `xxxx` compresses to nothing and would flatter Parquet.

## What came out

```
| | postgres | dbcenter | lazy |
|---|---|---|---|
| rows held locally | 200,000 | 200,000 | 200 |
| local disk | 154 MB | 399 MB | 39 MB |
| object storage | 0 MB | 315 MB | 136 MB |
| resident memory | 40 MB | 127 MB | 80 MB |
| cold start | 2.1 s | 6.3 s | 4.7 s |
| aggregate over all history, collection | 188.7 ms | 684.5 ms | - |
| aggregate over all history, columnar view | - | 19.7 ms | 46.0 ms |
| recent window, 100 rows by timestamp | 0.7 ms | 103.8 ms | 45.3 ms |
| point lookup by indexed column | 0.4 ms | 0.9 ms | 36.1 ms |
| single-row inserts per second | 120 | 109 | 110 |
```

Loading took 3.8s for PostgreSQL and 6.7s plus a 20.3s push and an 80.8s
compaction for DBCenter.

## What that says

**The columnar view is the win, and it is a real one.** An aggregate over the
whole table takes 188.7 ms against PostgreSQL's own storage and **19.7 ms**
against the same rows as Parquet in the bucket - 9.6x, on a machine with one
core. The lazy node, which holds none of the table, answers the same question in
46 ms: still 4x faster than the server that has the data on its own disk.

**The lazy node's disk is a quarter of the size.** 39 MB against 154 MB, and the
39 MB is mostly PostgreSQL's own cluster rather than data. That is what makes a
1 GB volume plausible where the managed database needs one sized to the table.

**Point lookups are where you pay.** 36 ms against 0.4 ms - ninety times - when
the row is only in the bucket. For a request path that reads one row per page
view, that is the difference between fine and not fine. The answer is not to
make object storage faster; it is to keep the rows a request path actually
touches in the local delta tables and let the history live in the bucket.

**The delta layer costs more than expected on ordinary queries.** A hundred rows
by timestamp takes 0.7 ms against a plain table and **103.8 ms** against the
collection, with the same index on both sides. The collection is a view -
`adds UNION ALL (base minus anything in adds or dels)` - and PostgreSQL will not
push the ordering and the limit through it, so it sorts everything. This is the
most valuable thing the experiment found and it is not fixed: it is a tax on
every query that is not a point lookup, paid by every DBCenter deployment
whether or not it syncs anything.

**Writes are a wash.** 109 against 120 single-row inserts a second, both bounded
by the round trip and the commit rather than by the engine.

## What did not fit

At **1,000,000 rows the compaction did not survive 1 GB**. The container reached
its memory limit and stalled at under 1% CPU: a compaction reads the whole base
snapshot back to rewrite it, and that does not stream. So the cheap tier can
serve a large table but cannot compact one - compaction needs a bigger machine,
or needs to be made incremental. Fixing this is the difference between "1 GB
works" and "1 GB works up to a few hundred thousand rows".

Getting even that far took three fixes that the rig turned up, each of which
would have made the claim untestable: a one-core machine deadlocked its
connection pool on the first sync; a restore ran two `ALTER TABLE` statements per
field per row; and every row was sent as a fresh SQL string, so PostgreSQL
planned a four-CTE statement a million times over. The last one alone had a
half-million-row compaction running for three hours without finishing. Cold start
went from 41.1 s to 6.3 s once it was fixed.

## The money

List prices, checked 8 September 2026. Assume a table that would occupy 100 GB
in PostgreSQL; the measurements above put the same rows at 88 GB as Parquet.

**Supabase Pro** - $25/month including a Micro instance and 8 GB of disk, then
$0.125 per GB-month:

    25.00  plan, with Micro compute
    11.50  92 GB of extra disk
    ------
    36.50  per month

**DBCenter on Fly.io with Cloudflare R2** - shared-cpu-1x/1GB at $5.92/month,
volumes at $0.15/GB-month, R2 at $0.015/GB-month with no egress charge:

     5.92  one machine, running all month
     0.45  a 3 GB volume
     1.32  88 GB in R2
    ------
     7.69  per month

At a terabyte the gap widens: Supabase is about $152/month, this is about
$20/month. Two things make the comparison honest rather than flattering:

- **The 1 GB machine cannot compact a large table today.** Budget shared-cpu-1x
  with 2 GB ($11.11) for a node that compacts, or run compaction somewhere else.
  That still lands near $13/month against $36.50.
- **Supabase is not only a database.** The $25 buys authentication, a REST and
  realtime API, a dashboard, backups and point-in-time recovery, and someone to
  call. DBCenter is a database. Comparing the two on storage price alone is
  comparing a component to a product.

Where the money actually goes is worth saying plainly: managed PostgreSQL
charges for disk sized to your whole table, at roughly eight times the price of
object storage, and charges it whether the data is hot or cold. Keeping the cold
part in a bucket and reading it as Parquet is the entire arbitrage. The columnar
numbers above say that reading it that way is not a compromise for analytics -
it is faster.

## Re-running it

```sh
docker compose -f scripts/cost-experiment/docker-compose.yml up -d storage storage-init
python scripts/cost-experiment/run.py --rows 200000
python scripts/cost-experiment/report.py
```

The numbers above are one machine's; the shape of them is what matters.
