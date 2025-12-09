# DBCenter - Rust Edition

## Building

### Prerequisites

- Rust 1.70+ (`rustup install stable`)
- For Python bindings: Python 3.8+, maturin (`pip install maturin`)
- For Go bindings: Go 1.21+
- For C examples: GCC or MSVC

### Build Core Library

```bash
cd dbcenter-core
cargo build --release
```

### Build C FFI Library

```bash
cd dbcenter-ffi
cargo build --release
```

This creates:
- Windows: `target/release/dbcenter.dll`
- Linux: `target/release/libdbcenter.so`
- macOS: `target/release/libdbcenter.dylib`

### Build Python Bindings

```bash
cd dbcenter-python
maturin develop  # For development
maturin build --release  # For distribution
```

### Test Python

```bash
python examples/test_python.py
```

### Build C Example

**Windows (MSVC):**
```powershell
cl examples\test_c.c /I. /link target\release\dbcenter.lib
```

**Windows (MinGW):**
```bash
gcc examples/test_c.c -I. -L./target/release -ldbcenter -o test_c.exe
```

**Linux/macOS:**
```bash
gcc examples/test_c.c -I. -L./target/release -ldbcenter -o test_c
export LD_LIBRARY_PATH=./target/release:$LD_LIBRARY_PATH
./test_c
```

### Build Go Example

```bash
cd dbcenter-go
go build ../examples/test_go.go
./test_go  # or test_go.exe on Windows
```

## Running Tests

### Rust Tests
```bash
cargo test --all
```

### Python Tests
```bash
cd dbcenter-python
maturin develop
python ../examples/test_python.py
```

### Go Tests
```bash
cd dbcenter-go
go test -v
```

## Benchmarks

Compare performance with Python version:

```bash
# Python version
cd ../  # Original Python dbcenter
python -m timeit -s "from dbcenter import DBCenter; db=DBCenter()" "db.users.insert_one({'test':1})"

# Rust version
cd rust-dbcenter/dbcenter-python
maturin develop
python -m timeit -s "from dbcenter import DBCenter; db=DBCenter()" "db.collection('users').insert_one({'test':1})"
```

Expected improvements:
- Startup time: 3-5x faster
- Memory usage: 2x lower
- Query performance: 1.5-2x faster

## Deployment

### Python Package

```bash
cd dbcenter-python
maturin build --release --target x86_64-pc-windows-msvc  # Windows
maturin build --release --target x86_64-unknown-linux-gnu  # Linux
maturin build --release --target x86_64-apple-darwin  # macOS

# Upload to PyPI
maturin publish
```

### C Library Distribution

The compiled library can be distributed directly:
- `dbcenter.h` - Header file
- `libdbcenter.{so,dll,dylib}` - Shared library

### Go Package

Publish to GitHub and users can import:
```go
import "github.com/heyobi/dbcenter-go"
```

## Cross-Platform Build

### Using Docker for Linux builds (on Windows/macOS)

```bash
docker run --rm -v $(pwd):/io ghcr.io/pyo3/maturin build --release
```

### Using GitHub Actions

See `.github/workflows/build.yml` for automated multi-platform builds.



cd rust-dbcenter; maturin develop --release -m dbcenter-python/Cargo.toml
C:\Users\Dell\.cargo\bin\maturin.exe develop --release -m dbcenter-python/Cargo.toml

C:\Users\Dell\.conda\envs\gis\Scripts\pip.exe install maturin

C:\Users\Dell\.conda\envs\gis\Scripts\maturin.exe develop --release -m dbcenter-python/Cargo.toml