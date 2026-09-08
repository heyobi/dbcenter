# DBCenter Node bindings

> **Experimental.** CI keeps this compiling and nothing more. There are no tests. The
> supported binding is Python (`dbcenter-python`), which has a test suite and is
> published to PyPI. If you need this language in production today, connect to
> DBCenter over the PostgreSQL wire protocol with your language's ordinary
> driver instead - that path is tested.

A thin wrapper over `dbcenter-ffi`, which is the same C surface every
non-Python binding uses. It builds as part of the workspace, so it cannot
silently drift out of sync with the core API - that has happened before, which
is why `cargo check --workspace` runs on every push.

## What would make it supported

A test suite that asserts what the calls return, run in CI, the way
`python_tests/` is. Until then the honest description is "it compiles".
