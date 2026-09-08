#!/usr/bin/env python3
"""Measure what a small machine plus object storage actually buys.

Three shapes, one workload, the same client:

  postgres  a plain PostgreSQL holding the whole table on its volume
  dbcenter  DBCenter with the whole table locally, plus a columnar view of the
            same rows in object storage
  lazy      DBCenter restored from the deltas only - the history is in the
            bucket and is read from there

Everything is measured through the PostgreSQL wire protocol, so the client does
not know or care which one it is talking to. That is the claim being tested.

    python scripts/cost-experiment/run.py --rows 5000000

Results land in results.json next to this file.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time

import psycopg

HERE = os.path.dirname(os.path.abspath(__file__))
COMPOSE = ["docker", "compose", "-f", os.path.join(HERE, "docker-compose.yml")]

TARGETS = {
    "postgres": {"port": 25432, "engine": "postgres", "data_dir": "/var/lib/postgresql"},
    "dbcenter": {"port": 25433, "engine": "dbcenter", "data_dir": "/data"},
    "lazy": {"port": 25434, "engine": "dbcenter", "data_dir": "/data"},
}

# One field per column on both sides, so the comparison is not measuring a JSON
# blob against a schema. md5 padding rather than repeated characters: a column
# of 'xxxx' compresses to nothing and would flatter the Parquet side.
ROW_SQL = """
    (random() * 100000)::bigint AS user_id,
    (ARRAY['north','south','east','west','central'])[1 + floor(random() * 5)] AS region,
    (random() * 1000)::double precision AS amount,
    1700000000 + g AS ts_epoch,
    md5(g::text) || md5((g * 7)::text) || md5((g * 13)::text) || md5((g * 17)::text) AS payload
"""


def compose(*args, **kwargs):
    return subprocess.run(COMPOSE + list(args), check=kwargs.pop("check", True), **kwargs)


def container_id(service):
    out = subprocess.run(
        COMPOSE + ["ps", "-q", service], capture_output=True, text=True, check=True
    )
    return out.stdout.strip()


def connect(port, timeout=300):
    """Wait for the server to answer, and hand back a connection."""
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            conn = psycopg.connect(
                host="127.0.0.1", port=port, user="postgres",
                password="dbcenter123", dbname="postgres", connect_timeout=5,
            )
            conn.autocommit = True
            with conn.cursor() as cur:
                cur.execute("SELECT 1")
            return conn
        except Exception as e:  # not up yet
            last = e
            time.sleep(1)
    raise RuntimeError(f"nothing answered on {port} within {timeout}s: {last}")


def wait_for_relation(conn, name, timeout=900):
    """PostgreSQL answers before DBCenter has finished setting a table up.

    `connect` only proves the server is listening; dbcenter-serve creates the
    collections, restores from the bucket and publishes the columnar view after
    that, so a client that starts working immediately races it.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        with conn.cursor() as cur:
            cur.execute("SELECT to_regclass(%s) IS NOT NULL", (name,))
            if cur.fetchone()[0]:
                return
        time.sleep(2)
    raise RuntimeError(f"{name} never appeared")


def wait_for_collection(conn, name, timeout=900):
    """Wait for a whole collection, not just the first table of one.

    A collection is three tables, a view and the triggers that route writes
    into it, created in that order. Waiting for any one of them means racing
    the rest.
    """
    for relation in (f"{name}_base", f"{name}_adds", f"{name}_dels", name):
        wait_for_relation(conn, relation, timeout)

    deadline = time.time() + timeout
    while time.time() < deadline:
        with conn.cursor() as cur:
            cur.execute(
                "SELECT count(*) FROM pg_trigger WHERE tgname = %s", (f"trg_insert_{name}",)
            )
            if cur.fetchone()[0]:
                return
        time.sleep(2)
    raise RuntimeError(f"{name} never got its write triggers")


def timed(conn, sql, params=None, runs=5):
    """Median and best of `runs`, in milliseconds, with the first run discarded."""
    samples = []
    with conn.cursor() as cur:
        cur.execute(sql, params)
        cur.fetchall()
        for _ in range(runs):
            started = time.perf_counter()
            cur.execute(sql, params)
            rows = cur.fetchall()
            samples.append((time.perf_counter() - started) * 1000)
    return {
        "median_ms": round(statistics.median(samples), 2),
        "best_ms": round(min(samples), 2),
        "rows": len(rows),
    }


def disk_bytes(service, path):
    cid = container_id(service)
    if not cid:
        return None
    out = subprocess.run(
        ["docker", "exec", cid, "du", "-sb", path], capture_output=True, text=True
    )
    if out.returncode != 0:
        return None
    return int(out.stdout.split()[0])


def memory_bytes(service):
    cid = container_id(service)
    if not cid:
        return None
    out = subprocess.run(
        ["docker", "stats", "--no-stream", "--format", "{{.MemUsage}}", cid],
        capture_output=True, text=True,
    )
    if out.returncode != 0 or "/" not in out.stdout:
        return None
    used = out.stdout.split("/")[0].strip()
    # Longest suffix first: "MiB" also ends in "B", and matching that leaves
    # "39.82Mi" to be parsed as a number.
    for suffix, factor in (("GiB", 1024**3), ("MiB", 1024**2), ("KiB", 1024), ("B", 1)):
        if used.endswith(suffix):
            return int(float(used[: -len(suffix)]) * factor)
    return None


def bucket_bytes():
    cid = container_id("storage")
    out = subprocess.run(
        ["docker", "exec", cid, "du", "-sb", "/data"], capture_output=True, text=True
    )
    return int(out.stdout.split()[0]) if out.returncode == 0 else None


# ---------------------------------------------------------------- loading

def load_postgres(conn, rows):
    with conn.cursor() as cur:
        cur.execute("DROP TABLE IF EXISTS events")
        cur.execute("""
            CREATE TABLE events (
                id BIGSERIAL PRIMARY KEY,
                user_id BIGINT,
                region TEXT,
                amount DOUBLE PRECISION,
                ts_epoch BIGINT,
                payload TEXT
            )
        """)
        started = time.perf_counter()
        cur.execute(f"""
            INSERT INTO events (user_id, region, amount, ts_epoch, payload)
            SELECT {ROW_SQL} FROM generate_series(1, {rows}) g
        """)
        load = time.perf_counter() - started

        started = time.perf_counter()
        cur.execute("CREATE INDEX idx_events_user ON events (user_id)")
        cur.execute("CREATE INDEX idx_events_ts ON events (ts_epoch)")
        cur.execute("ANALYZE events")
        index = time.perf_counter() - started
    return {"insert_s": round(load, 1), "index_s": round(index, 1)}


def load_dbcenter(conn, rows):
    """Write straight into the pending table, which is what a sync then pushes.

    Going through the collection view row by row would measure psycopg's round
    trips rather than anything about the database.
    """
    with conn.cursor() as cur:
        cur.execute("TRUNCATE events_adds, events_base, events_dels")
        started = time.perf_counter()
        cur.execute(f"""
            INSERT INTO events_adds
                (id, user_id, region, amount, ts_epoch, payload, created_at, updated_at)
            SELECT gen_random_uuid(), {ROW_SQL},
                   now() + (g || ' microseconds')::interval,
                   now()
            FROM generate_series(1, {rows}) g
        """)
        load = time.perf_counter() - started

        started = time.perf_counter()
        # The same indexes the plain PostgreSQL gets, on both delta tables -
        # the collection is a view over them, so an index on one side only
        # would make half the rows a sequential scan.
        for table in ("events_base", "events_adds"):
            cur.execute(f"CREATE INDEX IF NOT EXISTS idx_{table}_user ON {table} (user_id)")
            cur.execute(f"CREATE INDEX IF NOT EXISTS idx_{table}_ts ON {table} (ts_epoch)")
        cur.execute("ANALYZE events_adds")
        cur.execute("ANALYZE events_base")
        index = time.perf_counter() - started
    return {"insert_s": round(load, 1), "index_s": round(index, 1)}


def wait_for_sync(conn, rows, timeout=1800):
    """Block until the node has pushed everything it has pending."""
    deadline = time.time() + timeout
    started = time.perf_counter()
    while time.time() < deadline:
        with conn.cursor() as cur:
            cur.execute("SELECT count(*) FROM events_adds")
            pending = cur.fetchone()[0]
        if pending == 0:
            return round(time.perf_counter() - started, 1)
        time.sleep(5)
    raise RuntimeError(f"{pending} rows still pending after {timeout}s")


# ---------------------------------------------------------------- workload

def measure(conn, engine, rows, local_history=True):
    """Run the workload.

    `local_history` is false for the lazy node, which holds none of the table:
    asking it for an aggregate over `events` would measure an empty table. Its
    history is reachable only through the columnar view, which is the point.
    """
    out = {}
    with conn.cursor() as cur:
        cur.execute("SELECT count(*) FROM events")
        out["rows_held_locally"] = cur.fetchone()[0]

    history = "events" if local_history else "events_columnar"

    # An aggregate over the whole history: the query a dashboard runs.
    agg = ("SELECT region, count(*) AS n, sum(amount) AS total "
           "FROM {t} GROUP BY region ORDER BY region")
    if local_history:
        out["aggregate_collection"] = timed(conn, agg.format(t="events"), runs=3)
    if engine == "dbcenter":
        out["aggregate_columnar"] = timed(conn, agg.format(t="events_columnar"), runs=3)

    # The most recent rows, which is most of what an application reads.
    recent = ("SELECT ts_epoch FROM {t} WHERE ts_epoch > %s "
              "ORDER BY ts_epoch DESC LIMIT 100")
    cutoff = 1700000000 + rows - 1000
    out["recent_window"] = timed(conn, recent.format(t=history), (cutoff,), runs=5)

    # A point lookup, indexed where there is a local table to index.
    point = "SELECT count(*) FROM {t} WHERE user_id = %s"
    out["point_lookup"] = timed(conn, point.format(t=history), (12345,), runs=5)

    # Writes, through the collection, one round trip each.
    with conn.cursor() as cur:
        started = time.perf_counter()
        for i in range(200):
            cur.execute(
                "INSERT INTO events (user_id, region, amount, ts_epoch, payload) "
                "VALUES (%s, %s, %s, %s, %s)",
                (i, "north", 1.0, 1600000000 + i, "written by the benchmark"),
            )
        elapsed = time.perf_counter() - started
    out["insert_200_rows_ms"] = round(elapsed * 1000, 1)
    out["inserts_per_second"] = round(200 / elapsed)
    return out


def cold_start(service, port, engine):
    """How long from `start` to a query being answered.

    For DBCenter that includes restoring from object storage and publishing the
    columnar view, which is the part that decides whether a machine can be
    stopped when idle.
    """
    compose("stop", service)
    started = time.perf_counter()
    compose("start", service)
    conn = connect(port, timeout=900)
    if engine == "dbcenter":
        wait_for_relation(conn, "events_columnar")
    with conn.cursor() as cur:
        cur.execute("SELECT count(*) FROM events")
        cur.fetchone()
    elapsed = time.perf_counter() - started
    return round(elapsed, 1), conn


def wait_for_base(timeout=1800):
    """Block until a compaction has written a base snapshot.

    Without one there is nothing for a lazy restore to skip: it would read every
    delta and end up with the whole table, which is the opposite of the point.
    """
    cid = container_id("storage")
    deadline = time.time() + timeout
    started = time.perf_counter()
    while time.time() < deadline:
        out = subprocess.run(
            # MinIO lays objects out as /data/<bucket>/<key>, and the keys are
            # flat under the prefix rather than in a directory per table.
            ["docker", "exec", cid, "sh", "-c", "ls /data/events/bench/ 2>/dev/null"],
            capture_output=True, text=True,
        )
        names = out.stdout.split()
        if any("_BASE_" in n for n in names):
            deltas = [n for n in names if "_DELTA_" in n]
            return round(time.perf_counter() - started, 1), len(deltas)
        time.sleep(10)
    raise RuntimeError("no base snapshot appeared; is compaction enabled?")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rows", type=int, default=5_000_000)
    ap.add_argument("--only", nargs="*", default=list(TARGETS))
    args = ap.parse_args()

    results = {"rows": args.rows, "targets": {}}
    out_path = os.path.join(HERE, "results.json")

    for name in args.only:
        spec = TARGETS[name]
        print(f"\n=== {name} ===", flush=True)
        compose("up", "-d", name)
        conn = connect(spec["port"], timeout=900)
        if spec["engine"] == "dbcenter":
            # The collection has to exist before anything can be written to it.
            # The columnar view cannot: it is published by the first sync,
            # because until then there is no Parquet for it to read. The lazy
            # node comes up against a bucket that is already full, so it has one
            # from the start.
            wait_for_collection(conn, "events")
            if name == "lazy":
                wait_for_relation(conn, "events_columnar")

        entry = {}
        if name == "postgres":
            entry["load"] = load_postgres(conn, args.rows)
        elif name == "dbcenter":
            entry["load"] = load_dbcenter(conn, args.rows)
            print("  waiting for the sync to push everything...", flush=True)
            entry["load"]["sync_s"] = wait_for_sync(conn, args.rows)
            print("  waiting for the columnar view...", flush=True)
            wait_for_relation(conn, "events_columnar")
            print("  waiting for a compaction to write the base...", flush=True)
            compact_s, deltas_left = wait_for_base()
            entry["load"]["compact_s"] = compact_s
            entry["load"]["deltas_left"] = deltas_left
        else:
            # The lazy node loads nothing: it comes up against a bucket the
            # other node filled, and takes only the deltas.
            entry["load"] = {"note": "restored from the bucket, deltas only"}

        entry["disk_bytes"] = disk_bytes(name, spec["data_dir"])
        entry["bucket_bytes"] = bucket_bytes()
        conn.close()

        elapsed, conn = cold_start(name, spec["port"], spec["engine"])
        entry["cold_start_s"] = elapsed

        entry.update(measure(conn, spec["engine"], args.rows, local_history=(name != "lazy")))
        entry["memory_bytes"] = memory_bytes(name)
        conn.close()

        results["targets"][name] = entry
        json.dump(results, open(out_path, "w"), indent=2)
        print(json.dumps(entry, indent=2), flush=True)
        compose("stop", name)

    print(f"\nwritten to {out_path}")


if __name__ == "__main__":
    main()
