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

## The same thing again, with the network in it

MinIO next door answers in about a tenth of a millisecond. R2 or Tigris from a
Fly machine does not: it is tens of milliseconds, and every read the local
tables cannot answer pays it. So the rig can hold the storage container's
packets - `LATENCY_MS=10 docker compose --profile latency up -d latency` puts a
netem delay of 10 ms each way on it, 20 ms per round trip, which is a fair
stand-in for object storage in the same region.

Everything else identical:

```
| | postgres | dbcenter | lazy |
|---|---|---|---|
| aggregate over all history, collection | 192.8 ms | 688.7 ms | - |
| aggregate over all history, columnar view | - | 77.4 ms | 225.5 ms |
| recent window, 100 rows by timestamp | 0.6 ms | 100.1 ms | 222.3 ms |
| point lookup by indexed column | 0.4 ms | 0.9 ms | 216.4 ms |
| cold start | 2.1 s | 6.4 s | 4.7 s |
| compaction | - | 90.9 s | - |
```

Three things change, and one does not.

**The columnar view still wins.** 77.4 ms against PostgreSQL's 192.8 ms - 2.5x
rather than 9.6x, but still faster than the database that has the rows on its own
disk. That is the result worth having: the analytical advantage is not an
artefact of the bucket being local.

**The lazy node stops being fast.** Its aggregate goes from 46.0 ms to 225.5 ms,
which is slower than plain PostgreSQL. Holding none of the table is a disk and
cost story, not a speed story, the moment the bucket is a network away.

**A point lookup that reaches object storage becomes unusable for a request
path.** 216.4 ms against 0.4 ms. At 20 ms a round trip that is about eleven round
trips for one row - the footer, the metadata, the row group, and DuckDB's own
back and forth. Nothing about a request path survives that.

**Cold start barely moves** - 4.7 s either way - because it is dominated by
starting PostgreSQL rather than by talking to the bucket.

The practical reading: keep what a request path touches in the local delta
tables, and let the bucket hold the history that gets aggregated rather than
looked up one row at a time. `DBCENTER_LAZY_RESTORE` is for archival tables, not
for the table your API reads.

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

The same sum at four table sizes, and with self-managed PostgreSQL on the same
provider added so that only the storage differs:

```
| table in PostgreSQL | Supabase Pro | PostgreSQL on Fly, yourself | DBCenter on Fly + R2 |
|---|---|---|---|
| 10 GB | $25.25 | $7.72 | $6.50 |
| 100 GB | $36.50 | $23.92 | $7.69 |
| 500 GB | $86.50 | $95.92 | $12.97 |
| 1 TB | $152.00 | $190.24 | $19.89 |
```

Every column is one shared-cpu-1x/1GB machine at $5.92 where a machine is
involved. What moves is the disk: Supabase bills $0.125/GB-month, a Fly volume
$0.15/GB-month sized to the table plus 20% headroom, and R2 $0.015/GB-month for
0.88x the bytes. The middle column is the one to look at - same provider, same
machine, same PostgreSQL, and it crosses Supabase at half a terabyte purely on
storage price. That is the arbitrage in one row.

Two costs the table does not show:

- **A node that compacts needs 2 GB**, at $11.11 instead of $5.92. Add $5.19 to
  the last column if the same machine has to compact: $12.88 at 100 GB, $25.08
  at a terabyte. Still under Supabase everywhere.
- **R2 bills requests, not egress.** Class B reads are $0.36 per million. An
  uncached query costs roughly ten of them, so a million queries a month is
  about $4 - immaterial next to the disk saving, but not zero, and it grows with
  traffic rather than with the table. A local cache of hot row groups removes
  most of it.

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

## Adding a machine instead of a bigger one

The read path is a bucket, and a bucket has no primary. So the question is
whether a second machine adds read throughput the way a PostgreSQL replica would
- except that a replica needs its own copy of the table and one of these needs
39 MB.

`scripts/cost-experiment/scale.py` runs the aggregate flat out from 1, 2, 4 and 8
clients, against one PostgreSQL given **three cores** and against one, two and
three DBCenter reader nodes given **one core each**. Same total compute at three
nodes, so the comparison is about shape rather than hardware. The readers are
lazy: none of them holds the table.

```
| clients | postgres, 3 cores | 1 node | 2 nodes | 3 nodes |
|---|---|---|---|---|
| 1 | 23.5 q/s, 41 ms | 25.5 q/s, 38 ms | 24.6 q/s, 38 ms | 25.7 q/s, 38 ms |
| 2 | 21.1 q/s, 95 ms | 26.4 q/s, 83 ms | 41.4 q/s, 48 ms | 41.9 q/s, 47 ms |
| 4 | 22.1 q/s, 176 ms | 22.5 q/s, 190 ms | 47.5 q/s, 88 ms | 54.3 q/s, 65 ms |
| 8 | 24.7 q/s, 298 ms | 21.5 q/s, 382 ms | 44.5 q/s, 190 ms | 53.2 q/s, 136 ms |
```

**One core answers as many queries as PostgreSQL's three.** 26.4 against 24.7 at
their respective peaks. Both saturate - the difference is that one of them
saturates a third of the hardware.

**Adding a machine adds throughput.** 26 to 47 to 54 queries a second, and the
latency under eight clients falls from 382 ms on one node to 136 ms on three
while PostgreSQL sits at 298 ms. Nothing coordinates: each node opens the same
Parquet and reads it, and there is no primary to ask.

**It is not linear, and the reason matters.** The second node gives 1.8x, the
third only 2.1x. On this rig the bucket is one MinIO container on a host that was
already running everything else, so the storage side is the ceiling rather than
the nodes. R2 and Tigris do not have that ceiling; what they have instead is the
latency measured above, which raises the cost of every query rather than capping
the total.

**What it does not scale.** Writes still go through one node - this is read
scaling, not multi-master - and point lookups do not benefit, because they are
fast locally and slow remotely whatever the node count.

The money: three machines that hold no data are 3 x $5.92 = $17.76 a month and
three volumes at $0.45. Three PostgreSQL read replicas each need a volume the
size of the table, which at 100 GB is $11.50 a month each before compute. The
read tier is where "the bucket is the durable copy" turns into a price
difference rather than an architecture diagram.

## Can this be better than PostgreSQL, and how

It already is at one thing and cannot be at another, and being clear about which
is which is most of the strategy.

**Where it wins, and can win by more.** Analytical reads: 2.5x over the network,
9.6x when the bucket is close, and 2.2x against three times the hardware once
three nodes share a bucket. That gap comes from reading five columns of
Parquet instead of whole rows out of a heap, and it widens with the table rather
than narrowing - a row group holds a fixed number of rows, so the same query
against ten times the data reads the same fraction. Three things would widen it
further: writing the Parquet sorted on the column people filter by, so row-group
statistics prune more; keeping a small local cache of hot row groups, so a repeat
query pays no round trip; and pushing projections down so a five-column query
never fetches the sixth.

**Where it wins structurally.** Scale to zero - PostgreSQL cannot be stopped and
restarted in 4.7 s with its data intact, because its data is the thing on the
disk you just released. And cost, which is not an optimisation but an arbitrage:
managed disk is roughly eight times the price of object storage, billed on the
whole table whether it is hot or cold.

**Where it should aim for parity, not victory.** Point lookups and small writes
on hot data. 0.9 ms against 0.4 ms, and 95 against 109 inserts a second, is
PostgreSQL against PostgreSQL with a view in the way. The goal there is to get
the overhead to zero, not to beat the engine we are running on.

**The one thing standing in the way of parity is ours, not PostgreSQL's.** A
hundred rows by timestamp: 0.6 ms against 100.1 ms, same index both sides. The
collection is `adds UNION ALL (base minus adds minus dels)` and PostgreSQL will
not push an ordering and a limit through that shape, so it sorts the whole table
to return a hundred rows. Making `_base` and `_adds` partitions of one table
instead of arms of a view would let the planner do a merge append and stop after
a hundred rows. That single change is worth more than everything else on this
list, because it is paid by every query in every deployment, synced or not.

So: better than PostgreSQL on analytics and on cost, indistinguishable from it on
hot reads once the view tax is gone, and never better on a single indexed row -
that is the shape to aim for, and three of the four are already true.

## Speed, price, and what you give up

One table, the network case - 20 ms to the bucket, which is what a Fly machine
and R2 in the same region actually look like. Money at a 100 GB table.

```
| | PostgreSQL | DBCenter | verdict |
|---|---|---|---|
| aggregate over the whole table | 192.8 ms | 77.4 ms | 2.5x ours - the reason to do this |
| aggregate, bucket next door | 188.7 ms | 19.7 ms | 9.6x ours |
| read throughput per core | 24.7 q/s on 3 cores | 26.4 q/s on 1 core | 3x per core, ours |
| read scale-out | replica needs a full copy of the disk | +$5.92 and 39 MB per node | ours, structurally |
| 100 rows by timestamp | 0.6 ms | 100.1 ms | 167x theirs - our view tax, fixable |
| point lookup, row held locally | 0.4 ms | 0.9 ms | near parity |
| point lookup, row only in the bucket | 0.4 ms | 216.4 ms | theirs, structurally |
| single-row inserts | 120/s | 109/s | a wash |
| cold start | 2.1 s | 4.7-6.4 s | theirs, and it stops mattering after the first request |
| scale to zero | not possible | 4.7 s back, data intact | ours |
| local disk for a 100 GB table | a 120 GB volume | a 3 GB volume + 88 GB in the bucket | ours |
| cost at 100 GB | $23.92 self-managed, $36.50 Supabase | $7.69 | 3-5x cheaper |
| cost at 1 TB | $190.24 self-managed, $152.00 Supabase | $19.89 | 8x cheaper |
| compaction at 1M rows on 1 GB | n/a | does not fit | theirs, until compaction streams |
```

**Advantages**, in the order they are worth money: analytical reads are faster
than the database that owns the disk; the read tier scales by adding $6 machines
that hold nothing; storage costs an eighth of managed disk and is billed on cold
bytes at cold prices; and a node can be switched off and back on in five seconds
because the durable copy was never on it.

**Disadvantages**, in the order they will bite: an ordered, limited query pays a
100 ms tax that PostgreSQL does not, and every deployment pays it; a row that
lives only in the bucket takes 216 ms to fetch, which no request path survives;
compaction reads the whole snapshot into memory, so the cheap tier tops out
around a few hundred thousand rows; writes go through one node; and the whole
thing is one more moving part than `postgres`.

The first two are ours to fix and are described below. The third is scheduled
work. The fourth is a design choice. The fifth is the price of admission.

## Re-running it

```sh
docker compose -f scripts/cost-experiment/docker-compose.yml up -d storage storage-init

# optional: put the bucket a network away, 10 ms each way
LATENCY_MS=10 docker compose -f scripts/cost-experiment/docker-compose.yml \
  --profile latency up latency

python scripts/cost-experiment/run.py --rows 200000
python scripts/cost-experiment/report.py
```

The numbers above are one machine's; the shape of them is what matters.
