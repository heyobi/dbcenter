#!/usr/bin/env python3
"""Turn results.json into the table that goes in the write-up."""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))


def mb(n):
    return "-" if n is None else f"{n / 1024 / 1024:.0f} MB"


def ms(entry, key):
    v = entry.get(key)
    return "-" if not v else f"{v['median_ms']:.1f} ms"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "results.json")
    data = json.load(open(path))
    targets = data["targets"]
    names = [n for n in ("postgres", "dbcenter", "lazy") if n in targets]

    rows = [
        ("rows held locally", lambda e: f"{e.get('rows_held_locally', 0):,}"),
        ("local disk", lambda e: mb(e.get("disk_bytes"))),
        ("object storage", lambda e: mb(e.get("bucket_bytes"))),
        ("resident memory", lambda e: mb(e.get("memory_bytes"))),
        ("cold start", lambda e: f"{e.get('cold_start_s', 0):.1f} s"),
        ("aggregate, collection", lambda e: ms(e, "aggregate_collection")),
        ("aggregate, columnar view", lambda e: ms(e, "aggregate_columnar")),
        ("recent window", lambda e: ms(e, "recent_window")),
        ("point lookup", lambda e: ms(e, "point_lookup")),
        ("inserts per second", lambda e: str(e.get("inserts_per_second", "-"))),
    ]

    print(f"{data['rows']:,} rows\n")
    header = "| | " + " | ".join(names) + " |"
    print(header)
    print("|" + "---|" * (len(names) + 1))
    for label, render in rows:
        cells = [render(targets[n]) for n in names]
        if all(c == "-" for c in cells):
            continue
        print(f"| {label} | " + " | ".join(cells) + " |")

    print("\nLoading:")
    for n in names:
        print(f"  {n}: {targets[n].get('load')}")


if __name__ == "__main__":
    main()
