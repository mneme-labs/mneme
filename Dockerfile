# Mneme — Production Dockerfile
# =============================================================================
# Single multi-stage build. Each component is a separate --target:
#
#   mnemelabs/core    — mneme-core (solo + cluster + HA)
#   mnemelabs/keeper  — mneme-keeper (persistence node)
#   mnemelabs/cli     — mneme-cli (management tool)
#   mnemelabs/bench   — mneme-bench (load testing)
#
# Build individual images:
#   docker build --target core   -t mnemelabs/core:0.1.0   .
#   docker build --target keeper -t mnemelabs/keeper:0.1.0 .
#   docker build --target cli    -t mnemelabs/cli:0.1.0    .
#   docker build --target bench  -t mnemelabs/bench:0.1.0  .
#
# Build all at once (via docker-compose):
#   docker compose build
#
# Multi-arch:
#   docker buildx build --platform linux/amd64,linux/arm64 --target core \
#     -t mnemelabs/core:0.1.0 --push .
# =============================================================================

# Latest stable Rust on Debian 12 (bookworm) — tracks stable channel, not nightly
ARG RUST_IMAGE=rust:bookworm
# Pinned Debian 12 slim runtime — same Debian version as builder for glibc compat
ARG RUNTIME_IMAGE=debian:bookworm-slim

# ── Stage 1: cargo-chef base ──────────────────────────────────────────────────
FROM ${RUST_IMAGE} AS chef

RUN apt-get update && apt-get install -y --no-install-recommends \
        linux-libc-dev \
        clang \
        libclang-dev \
        pkg-config \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --locked

WORKDIR /build

# ── Stage 2: Compute dependency recipe ────────────────────────────────────────
FROM chef AS planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: Build all workspace binaries ─────────────────────────────────────
FROM chef AS builder

COPY --from=planner /build/recipe.json recipe.json

# Cook dependencies (this layer is cached until Cargo.lock changes)
RUN cargo chef cook --release --recipe-path recipe.json --workspace

# Copy full source and rebuild only changed crates
COPY . .
RUN find . -name '*.rs' -exec touch {} +

RUN RUSTFLAGS="-C target-cpu=native" cargo build --release --workspace

# ── Stage 4: Run tests ────────────────────────────────────────────────────────
FROM builder AS tester

RUN cargo test --lib -p mneme-common -- --test-threads=4 2>&1 | tee /tmp/test-common.log
RUN cargo test --lib -p mneme-client -- --test-threads=4 2>&1 | tee /tmp/test-client.log
RUN cargo test --test integration_solo \
        -p mneme-core -- --test-threads=1 --nocapture 2>&1 | tee /tmp/test-solo.log
RUN cargo test --test integration_data_types \
        -p mneme-core -- --test-threads=1 --nocapture 2>&1 | tee /tmp/test-datatypes.log
RUN echo "=== TEST RESULTS ===" && \
    grep -E "^(test |FAILED|ok|error)" /tmp/test-*.log | tail -40 && \
    echo "=== ALL TESTS PASSED ==="

# ── Stage 5: Development image ────────────────────────────────────────────────
# docker build --target dev -t mnemelabs/dev:latest .
FROM builder AS dev

RUN cargo install cargo-watch cargo-nextest --locked 2>/dev/null || true
WORKDIR /build
CMD ["cargo", "watch", "-x", "check"]

# ── Common runtime base (not a final target) ──────────────────────────────────
FROM ${RUNTIME_IMAGE} AS base-runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        bash \
        curl \
    && rm -rf /var/lib/apt/lists/*

# Non-root daemon user shared across all component images
RUN groupadd -r mneme && \
    useradd -r -g mneme -s /bin/false -d /var/lib/mneme -c "Mneme daemon" mneme && \
    install -d -m 755 -o mneme -g mneme /etc/mneme && \
    install -d -m 750 -o mneme -g mneme /var/lib/mneme

# ── mnemelabs/core ────────────────────────────────────────────────────────────
# Runs solo mode (default), cluster core, or HA core depending on entrypoint.
FROM base-runtime AS core

COPY --from=builder /build/target/release/mneme-core  /usr/local/bin/
COPY --from=builder /build/target/release/mneme-cli   /usr/local/bin/

# All TOML configs — entrypoint selects the right one via MNEME_CONFIG
COPY docker/configs/ /etc/mneme/

# All entrypoints and test scripts
COPY docker/entrypoint-solo.sh    /docker/entrypoint-solo.sh
COPY docker/entrypoint-core.sh    /docker/entrypoint-core.sh
COPY docker/entrypoint-replica.sh /docker/entrypoint-replica.sh
COPY docker/smoke-test.sh         /docker/smoke-test.sh
COPY docker/integration-test.sh   /docker/integration-test.sh
RUN chmod +x /docker/*.sh

EXPOSE 6379 7379 9090

ENV MNEME_CONFIG=/etc/mneme/solo.toml
ENV MNEME_ADMIN_PASSWORD=""
ENV MNEME_LOG_LEVEL=info

HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=12 \
    CMD curl -sf http://127.0.0.1:9090/metrics > /dev/null || exit 1

USER mneme
WORKDIR /var/lib/mneme

# Default: solo mode (Core + embedded persistence)
ENTRYPOINT ["/bin/bash", "/docker/entrypoint-solo.sh"]

# ── mnemelabs/keeper ──────────────────────────────────────────────────────────
# Persistence node: WAL + snapshots + cold store (redb). Connects to Core.
FROM base-runtime AS keeper

COPY --from=builder /build/target/release/mneme-keeper /usr/local/bin/

COPY docker/configs/keeper-1.toml /etc/mneme/keeper-1.toml
COPY docker/configs/keeper-2.toml /etc/mneme/keeper-2.toml
COPY docker/configs/keeper-3.toml /etc/mneme/keeper-3.toml
COPY docker/entrypoint-keeper.sh  /docker/entrypoint-keeper.sh
RUN chmod +x /docker/entrypoint-keeper.sh

EXPOSE 7379 9090

ENV KEEPER_NODE_ID=keeper-1
ENV KEEPER_POOL_BYTES=2gb
ENV CORE_ADDR=mneme-core:7379
ENV MNEME_LOG_LEVEL=info

HEALTHCHECK --interval=10s --timeout=3s --start-period=60s --retries=6 \
    CMD curl -sf http://127.0.0.1:9090/metrics > /dev/null || exit 1

USER mneme
WORKDIR /var/lib/mneme

ENTRYPOINT ["/bin/bash", "/docker/entrypoint-keeper.sh"]

# ── mnemelabs/cli ─────────────────────────────────────────────────────────────
# Management CLI. Connects to any Core or replica via mTLS.
FROM base-runtime AS cli

COPY --from=builder /build/target/release/mneme-cli /usr/local/bin/

ENV MNEME_HOST=mneme-core:6379
ENV MNEME_LOG_LEVEL=warn

USER mneme
WORKDIR /var/lib/mneme

ENTRYPOINT ["mneme-cli"]
CMD ["--help"]

# ── mnemelabs/bench ───────────────────────────────────────────────────────────
# Load testing tool. Targets any running Core or replica.
FROM base-runtime AS bench

COPY --from=builder /build/target/release/mneme-bench /usr/local/bin/

USER mneme
WORKDIR /var/lib/mneme

ENTRYPOINT ["mneme-bench"]
CMD ["--help"]
