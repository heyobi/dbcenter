# DBCenter - Rust Edition

**PostgreSQL as a SQLite/MongoDB** - Embedded PostgreSQL with MongoDB-style API, now written in Rust for maximum performance and safety.

## Features

- 🚀 **Embedded PostgreSQL** - No external dependencies, runs locally
- 🔥 **MongoDB-style API** - Familiar NoSQL interface with PostgreSQL power
- 🌍 **Multi-language support** - C, Python, Go bindings
- 📊 **PostGIS** - Full geospatial database support
- ⚡ **High Performance** - Rust's zero-cost abstractions
- 🔒 **Memory Safe** - No segfaults, no data races
- 🎯 **Type Safe** - Compile-time guarantees

## Language Bindings

### Python
```python
from dbcenter import DBCenter

db = DBCenter(path="./data")
db.users.insert_one({"name": "Ali", "age": 25})
users = db.users.find({"age": {"$gt": 18}})
```

### C
```c
#include "dbcenter.h"

DBCenter* db = dbcenter_new("./data", true);
dbcenter_insert_one(db, "users", "{\"name\":\"Ali\",\"age\":25}");
```

### Go
```go
import "github.com/heyobi/dbcenter-go"

db := dbcenter.New("./data")
db.Collection("users").InsertOne(map[string]interface{}{
    "name": "Ali",
    "age": 25,
})
```

## Architecture

- `dbcenter-core`: Core Rust library
- `dbcenter-ffi`: C FFI bindings
- `dbcenter-python`: Python bindings (PyO3)
- `dbcenter-go`: Go bindings (cgo)
- `dbcenter-serve`: the same engine as a container workload, with its durable
  copy on object storage

## Running as a container

The engine is meant to be embedded in a process, with object storage holding
the durable copy. That is also the shape a scale-to-zero platform wants: the
machine is disposable, the bucket is not.

`dbcenter-serve` supplies the lifecycle. On boot it restores every table it
finds in the bucket, then starts PostgreSQL on TCP so ordinary clients - psql,
DBeaver, any driver - connect straight to it. While running it pushes pending
changes periodically, and on SIGTERM it pushes whatever is left before shutting
down, so nothing written since the last pass is lost when the machine stops.

The image ships with the PostgreSQL distribution already unpacked, so a cold
start does not wait on a 60MB download from Maven Central, and it starts with
no network access at all.

```bash
docker build -t dbcenter .

docker run --rm -p 5432:5432 \
  -e DBCENTER_STORAGE=s3 \
  -e DBCENTER_BUCKET=my-bucket \
  -e AWS_ENDPOINT_URL_S3=https://fly.storage.tigris.dev \
  -e AWS_ACCESS_KEY_ID=... -e AWS_SECRET_ACCESS_KEY=... \
  -e DBCENTER_PASSWORD=... \
  -e DBCENTER_COLLECTIONS=users,orders \
  dbcenter
```

| Variable | Default | Meaning |
|---|---|---|
| `DBCENTER_STORAGE` | `none` | `s3`, `local` or `none` |
| `DBCENTER_BUCKET` | `$BUCKET_NAME` | Bucket holding the durable copy |
| `DBCENTER_PREFIX` | *(none)* | Key prefix, so several databases share a bucket |
| `DBCENTER_DATA_DIR` | `/data/db` | Local cluster directory |
| `DBCENTER_LISTEN` | `*` | TCP bind address; empty disables TCP |
| `DBCENTER_PORT` | `5432` | |
| `DBCENTER_PASSWORD` | *(none)* | Required for TCP clients; without it only the local socket is usable |
| `DBCENTER_COLLECTIONS` | *(none)* | Comma-separated collections to create at boot |
| `DBCENTER_SYNC_INTERVAL_SECS` | `30` | How often to push pending changes |
| `DBCENTER_NODE_ID` | `$FLY_MACHINE_ID` | Identifies this writer in storage locks |

`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION` and
`AWS_ENDPOINT_URL_S3` are read the same way every other S3 tool reads them, so
`fly storage create` and `aws configure` both just work.

### Fly.io

`fly.toml` sets `auto_stop_machines` with `min_machines_running = 0`: with no
connections Fly stops the machine and the bill goes to nothing, and the next
client to connect starts it again. The volume is a warm cache that saves
re-running `initdb` on wake, not the source of truth - losing it costs time,
not data.

### Checking it locally

`docker-compose.yml` stands MinIO in for Tigris and runs two nodes against one
bucket. `scripts/cluster-test.sh` writes through one node, destroys that node
and its disk, and checks the row comes back on the other:

```bash
./scripts/cluster-test.sh
```

## Building

```bash
# Core library
cargo build --release -p dbcenter-core

# Python bindings
cd dbcenter-python
maturin build --release

# C library
cargo build --release -p dbcenter-ffi
```

## Performance

Rust implementation provides:
- **3-5x faster** startup time
- **2x lower** memory footprint
- **Zero-cost** abstractions
- **Safe concurrency** with no data races

## Original Python Version

This is a Rust rewrite of the original Python dbcenter library, maintaining API compatibility while adding performance and safety.
