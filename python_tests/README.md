# Python tests

These need a built extension module, not the published wheel:

```sh
python -m venv .venv
.venv/bin/pip install maturin
VIRTUAL_ENV=$PWD/.venv .venv/bin/maturin develop --manifest-path dbcenter-python/Cargo.toml
cd python_tests && ../.venv/bin/python -m unittest discover -v -p 'test_*.py'
```

Each test class boots its own embedded PostgreSQL in a directory of its own, so
a class costs a few seconds and no test can be broken by another one's mess. The
first run also downloads the PostgreSQL binaries into `~/.dbcenter`.

| file | what it covers |
|---|---|
| `test_collection.py` | insert, find with every operator, update, delete, indexes, fuzzy search, lazy frames, and the errors each one raises on bad input |
| `test_sync.py` | the delta layer, the Parquet round trip through object storage, and `smart_query` |
| `test_columnar.py` | the `<name>_columnar` view, and that nothing about DuckDB appears without the flag |
| `test_drive.py` | Google Drive, skipped unless `DBCENTER_DRIVE_CLIENT_SECRET` is set |
| `test_final.py` | the original end-to-end suite |

`test_columnar.py` skips itself where pg_duckdb is not published - it is built
for Linux on PostgreSQL 14 to 18.

## What a test here is for

Every bug found in the September 2026 pass was on a path no test called, or in
a call whose result nothing checked: a synced row that arrived with only its
id, an updated row that vanished at the next sync, `db.port` raising on every
default instance, `create_index(field, "gin")` failing outright, a footer cache
that was never written. The suite printed all of it and asserted none of it.

So: assert the value, not that the call returned. When adding a feature, add
the test that fails without it, and one that gives it the wrong input.
