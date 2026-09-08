#!/usr/bin/env bash
# Check the tree the way CI does, including the parts that skip themselves.
#
# The S3 tests do nothing without an endpoint, so a green `cargo test` on its own
# does not mean they ran. This brings up MinIO for them and takes it down after.
set -uo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

fail=0
step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }
check() { if [ "$1" -eq 0 ]; then printf '\033[32mok\033[0m\n'; else printf '\033[31mFAILED\033[0m\n'; fail=1; fi; }

step "Formatting"
cargo fmt --all -- --check; check $?

step "Every crate compiles"
# The bindings drifted out of sync with the core once because nothing built them.
cargo check --workspace --all-targets; check $?

step "Clippy"
cargo clippy -p dbcenter-core -p dbcenter-serve --all-targets -- -D warnings; check $?

step "Object storage for the S3 tests"
if docker info >/dev/null 2>&1; then
  docker rm -f dbcenter-verify-minio >/dev/null 2>&1
  docker run -d --name dbcenter-verify-minio -p 19000:9000 \
    -e MINIO_ROOT_USER=dbcenter -e MINIO_ROOT_PASSWORD=dbcenter123 \
    minio/minio:latest server /data >/dev/null 2>&1
  for _ in $(seq 1 30); do
    curl -sf http://127.0.0.1:19000/minio/health/live >/dev/null 2>&1 && break
    sleep 1
  done
  docker exec dbcenter-verify-minio mc alias set local http://127.0.0.1:9000 dbcenter dbcenter123 >/dev/null 2>&1
  docker exec dbcenter-verify-minio mc mb --ignore-existing local/dbcenter-test >/dev/null 2>&1
  . scripts/s3-test-env.sh
  echo "MinIO up at $DBCENTER_S3_TEST_ENDPOINT"
else
  echo "no docker; the S3 tests will skip themselves"
fi

step "Tests"
# Each integration test boots a real PostgreSQL, so they cannot run in parallel.
cargo test --workspace -- --test-threads=1; check $?

docker rm -f dbcenter-verify-minio >/dev/null 2>&1

step "Result"
if [ $fail -eq 0 ]; then
  printf '\033[32meverything passed\033[0m\n'
else
  printf '\033[31msomething failed, see above\033[0m\n'
fi
exit $fail
