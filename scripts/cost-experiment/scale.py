#!/usr/bin/env python3
"""Does adding a machine add read throughput?

A managed PostgreSQL answers a heavy read load by getting bigger, or by adding
replicas that each need their own copy of the table. DBCenter's read path is a
bucket, and a bucket has no primary: a node that reads it needs no copy of
anything. So the question is whether N nodes actually give N times the
throughput, and how that compares to spending the same compute on one bigger
PostgreSQL.

Run `run.py` first - this reads the data it leaves in the bucket.

    python scripts/cost-experiment/scale.py
"""

import argparse
import json
import os
import statistics
import subprocess
import threading
import time

import psycopg

HERE = os.path.dirname(os.path.abspath(__file__))
COMPOSE = ["docker", "compose", "-f", os.path.join(HERE, "docker-compose.yml")]

READERS = [25441, 25442, 25443]
POSTGRES_BIG = 25444

AGGREGATE = ("SELECT region, count(*) AS n, sum(amount) AS total "
             "FROM {t} GROUP BY region ORDER BY region")

ROW_SQL = """
    (random() * 100000)::bigint AS user_id,
    (ARRAY['north','south','east','west','central'])[1 + floor(random() * 5)] AS region,
    (random() * 1000)::double precision AS amount,
    1700000000 + g AS ts_epoch,
    md5(g::text) || md5((g * 7)::text) || md5((g * 13)::text) || md5((g * 17)::text) AS payload
"""


def compose(*args):
    subprocess.run(COMPOSE + list(args), check=True)


def connect(port, timeout=900):
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
        except Exception as e:
            last = e
            time.sleep(1)
    raise RuntimeError(f"nothing answered on {port}: {last}")


def wait_for_relation(conn, name, timeout=900):
    deadline = time.time() + timeout
    while time.time() < deadline:
        with conn.cursor() as cur:
            cur.execute("SELECT to_regclass(%s) IS NOT NULL", (name,))
            if cur.fetchone()[0]:
                return
        time.sleep(2)
    raise RuntimeError(f"{name} never appeared")


def hammer(ports, table, clients, seconds):
    """`clients` threads, spread over `ports`, running the aggregate flat out.

    Returns queries a second and the median latency a client saw. Throughput is
    what scaling is about; latency is what says whether the extra throughput
    came from parallelism or from queueing.
    """
    conns = [connect(ports[i % len(ports)]) for i in range(clients)]
    latencies = [[] for _ in conns]
    stop = time.time() + seconds
    barrier = threading.Barrier(clients)

    def work(i):
        barrier.wait()
        with conns[i].cursor() as cur:
            while time.time() < stop:
                started = time.perf_counter()
                cur.execute(AGGREGATE.format(t=table))
                cur.fetchall()
                latencies[i].append((time.perf_counter() - started) * 1000)

    threads = [threading.Thread(target=work, args=(i,)) for i in range(clients)]
    started = time.perf_counter()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.perf_counter() - started

    for c in conns:
        c.close()
    flat = [v for per_client in latencies for v in per_client]
    return {
        "clients": clients,
        "nodes": len(ports),
        "queries": len(flat),
        "per_second": round(len(flat) / elapsed, 1),
        "median_ms": round(statistics.median(flat), 1) if flat else None,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=int, default=20)
    ap.add_argument("--rows", type=int, default=200_000)
    ap.add_argument("--nodes", type=int, nargs="*", default=[1, 2, 3])
    ap.add_argument("--skip-postgres", action="store_true",
                    help="the reader sweeps only, when PostgreSQL has already been measured "
                         "and its container is in the way of the memory the readers need")
    args = ap.parse_args()

    results = {"seconds": args.seconds, "rows": args.rows, "runs": []}
    out_path = os.path.join(HERE, "scale-results.json")

    if args.skip_postgres:
        print("=== skipping PostgreSQL ===", flush=True)
    else:
        run_postgres(args, results)

    print("\n=== DBCenter reader nodes, one core each ===", flush=True)
    for count in args.nodes:
        names = [f"reader{i + 1}" for i in range(count)]
        compose("--profile", "readers", "up", "-d", *names)
        ports = READERS[:count]
        for port in ports:
            c = connect(port)
            wait_for_relation(c, "events_columnar")
            c.close()

        for clients in (1, 2, 4, 8):
            run = hammer(ports, "events_columnar", clients, args.seconds)
            run["target"] = f"dbcenter-{count}-node"
            results["runs"].append(run)
            print(f"  {run}", flush=True)

    json.dump(results, open(out_path, "w"), indent=2)
    print(f"\nwritten to {out_path}")


def run_postgres(args, results):
    print("=== one PostgreSQL with three cores ===", flush=True)
    compose("--profile", "readers", "up", "-d", "postgres-big")
    conn = connect(POSTGRES_BIG)
    with conn.cursor() as cur:
        cur.execute("SELECT to_regclass('events') IS NOT NULL")
        if not cur.fetchone()[0]:
            print("  loading...", flush=True)
            cur.execute("""
                CREATE TABLE events (
                    id BIGSERIAL PRIMARY KEY, user_id BIGINT, region TEXT,
                    amount DOUBLE PRECISION, ts_epoch BIGINT, payload TEXT
                )""")
            cur.execute(f"INSERT INTO events (user_id, region, amount, ts_epoch, payload) "
                        f"SELECT {ROW_SQL} FROM generate_series(1, {args.rows}) g")
            cur.execute("CREATE INDEX ON events (user_id)")
            cur.execute("CREATE INDEX ON events (ts_epoch)")
            cur.execute("ANALYZE events")
    conn.close()

    for clients in (1, 2, 4, 8):
        run = hammer([POSTGRES_BIG], "events", clients, args.seconds)
        run["target"] = "postgres-3-cores"
        results["runs"].append(run)
        print(f"  {run}", flush=True)


if __name__ == "__main__":
    main()
