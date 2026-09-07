# DBCenter as a scale-to-zero container.
#
# The image ships with the PostgreSQL distribution already unpacked, so a cold
# start is process startup plus whatever has to come back from object storage -
# not a 60MB download from Maven Central. That is the difference between a
# machine that can be stopped when idle and one that cannot.

# ---- build -------------------------------------------------------------
FROM rust:1-bookworm AS builder

WORKDIR /src

# The release profile uses fat LTO and a single codegen unit, which needs more
# memory than a small build machine has. Thin LTO gives almost the same binary
# for a fraction of the peak.
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16

COPY . .

# Only the pieces the container runs: the bindings need Python, JNI and Node
# toolchains that have no business in this image.
#
# Copying manifests first to cache dependencies does not work here - the
# workspace declares examples and a staticlib whose sources cargo insists on
# seeing before it will even read the manifests - so the layer cache covers the
# whole build or nothing.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p dbcenter-serve -p dbcenter-watcher \
 && mkdir -p /out \
 && cp target/release/dbcenter-serve target/release/dbcenter-watcher /out/

# ---- postgres distribution --------------------------------------------
# Fetched in its own stage so a source change does not re-download it.
FROM debian:bookworm-slim AS postgres-dist

ARG POSTGRES_VERSION=18.0.0
ENV DBCENTER_POSTGRES_VERSION=${POSTGRES_VERSION}

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/dbcenter-serve /usr/local/bin/dbcenter-serve
RUN /usr/local/bin/dbcenter-serve --prefetch

# ---- runtime -----------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates locales \
 && rm -rf /var/lib/apt/lists/* \
 # PostgreSQL refuses to initialise a cluster as root.
 && useradd --create-home --uid 10001 dbcenter \
 && mkdir -p /data/db /var/lib/dbcenter/storage \
 && chown -R dbcenter:dbcenter /data /var/lib/dbcenter

COPY --from=builder /out/dbcenter-serve /usr/local/bin/dbcenter-serve
COPY --from=builder /out/dbcenter-watcher /usr/local/bin/dbcenter-watcher
COPY --from=postgres-dist --chown=dbcenter:dbcenter /root/.dbcenter /home/dbcenter/.dbcenter

USER dbcenter
WORKDIR /home/dbcenter

# dirs::home_dir() is how the engine finds the unpacked distribution.
ENV HOME=/home/dbcenter \
    DBCENTER_DATA_DIR=/data/db \
    DBCENTER_LISTEN=* \
    DBCENTER_PORT=5432 \
    DBCENTER_STORAGE=none \
    RUST_LOG=info

EXPOSE 5432
VOLUME ["/data"]

# Run as PID 1 and take SIGTERM directly: the final flush to object storage
# happens in the signal path, so the signal must not be swallowed by a shell.
ENTRYPOINT ["/usr/local/bin/dbcenter-serve"]
