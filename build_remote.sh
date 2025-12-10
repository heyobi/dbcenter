#!/bin/bash
set -e

# Base directory on remote (where we copied source)
ROOT=/tmp/dbcenter_build

echo "--- Starting Remote Build on $(hostname) ---"

# Ensure clean slate for artifacts
mkdir -p $ROOT/dist/linux
chmod 777 $ROOT/dist/linux

# 1. Build Rust FFI .so (needed for Go)
echo "[Remote] Building Rust FFI (via Docker: rust:1-bookworm)..."
# We mount current dir to /volume. 
# Note: User IDs might be an issue with Docker mounts, but typically root in container works.
docker run --rm -v $ROOT:/volume -w /volume/dbcenter-ffi rust:1-bookworm cargo build --release
# Copy .so to dist
cp $ROOT/dbcenter-ffi/target/release/libdbcenter_ffi.so $ROOT/dist/linux/ || cp $ROOT/target/release/libdbcenter_ffi.so $ROOT/dist/linux/

# 1b. Build dbcenter-watcher
echo "[Remote] Building dbcenter-watcher..."
docker run --rm -v $ROOT:/volume -w /volume/dbcenter-watcher rust:1-bookworm cargo build --release
cp $ROOT/dbcenter-watcher/target/release/dbcenter-watcher $ROOT/dist/linux/ || cp $ROOT/target/release/dbcenter-watcher $ROOT/dist/linux/

# 2. Build Python Wheel
echo "[Remote] Building Python Wheel (via Docker: ghcr.io/pyo3/maturin)..."
# This image is designed for maturin builds.
# We mount root to /io. Output goes to /io/dist/linux via --out
# Specify manifest path to dbcenter-python/Cargo.toml
docker run --rm -v $ROOT:/io ghcr.io/pyo3/maturin build --release --manifest-path /io/dbcenter-python/Cargo.toml --out /io/dist/linux

# 3. Build Go Binary
echo "[Remote] Building Go Binary (via Docker: golang:latest)..."
# We need the .so file accessible to the Go linker.
# We'll copy it to /usr/lib inside the container.
docker run --rm -v $ROOT:/volume -w /volume/examples golang:latest bash -c "
    echo 'Installing libdbcenter_ffi.so...'
    cp ../dist/linux/libdbcenter_ffi.so /usr/lib/
    
    echo 'Initializing Go module...'
    go mod init example_build || true
    go mod edit -replace github.com/heyobi/dbcenter-go=../dbcenter-go
    go mod tidy
    
    echo 'Building binary...'
    go build -o ../dist/linux/test_go_linux test_go.go
"

# 4. Build C Test Binary
echo "[Remote] Building C Test Binary (via Docker: gcc)..."
docker run --rm -v $ROOT:/volume -w /volume/examples gcc:latest bash -c "
    echo 'Compiling test_c.c...'
    gcc -o ../dist/linux/test_c_linux test_c.c -I../dbcenter-ffi -L../dist/linux -ldbcenter_ffi -Wl,-rpath,. -Wl,-rpath,/work
    echo 'C test binary built successfully.'
"

# Copy header file for reference
cp $ROOT/dbcenter-ffi/dbcenter.h $ROOT/dist/linux/

echo "--- Remote Build Complete ---"
ls -l $ROOT/dist/linux
