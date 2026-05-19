# TesseraGraph Enterprise — Multi-stage Docker build
# ===================================================
# Build context must be the parent directory (tessera-ecosystem/)
# so that both tessera-graph and tessera-graph-enterprise are available.
#
#   docker build -t tesseraio/tessera-graph-enterprise:latest \
#     -f tessera-graph-enterprise/Dockerfile .
#
# Or use the Makefile / docker-compose target.

ARG RUST_VERSION=1.93

# =======================================================
# Stage 1: Builder
# =======================================================
FROM rust:${RUST_VERSION}-bookworm AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy both repos (build context is tessera-ecosystem/)
COPY tessera-graph/ tessera-graph/
COPY tessera-graph-enterprise/ tessera-graph-enterprise/

# Remove [[bench]] blocks from Cargo manifests (bench .rs files excluded by .dockerignore)
RUN find tessera-graph/ -name Cargo.toml -exec sed -i '/^\[\[bench\]\]/,/^$/d' {} + \
    && sed -i '/"crates\/tessera-graph-benchmark",/d' tessera-graph-enterprise/Cargo.toml

# Build server (enterprise workspace) and CLI (MIT core workspace) in release
# mode. The enterprise CLI crate has been deleted as part of the 0.5.0 sync —
# the MIT core CLI is a strict superset (admin commands, layout migration).
RUN cd tessera-graph-enterprise \
    && cargo build --release -p tessera-graph-server \
    && cd ../tessera-graph \
    && cargo build --release -p tessera-graph-cli

# Verify binaries
RUN ls -lh tessera-graph-enterprise/target/release/tessera-graph-server \
           tessera-graph/target/release/tessera-graph-cli

# Generate self-signed TLS certificate for development
RUN apt-get update && apt-get install -y openssl && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /build/certs \
    && openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
       -keyout /build/certs/server.key \
       -out /build/certs/server.pem \
       -days 3650 -nodes \
       -subj "/CN=tessera-graph/O=BelowZero Security/C=EE" \
       -addext "subjectAltName=DNS:localhost,DNS:tessera-graph,IP:127.0.0.1"

# =======================================================
# Stage 2: Runtime
# =======================================================
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user
RUN useradd -m -u 1001 tessera \
    && mkdir -p /var/lib/tessera/data /etc/tessera/certs /var/log/tessera \
    && chown -R tessera:tessera /var/lib/tessera /etc/tessera /var/log/tessera

# Copy binaries
COPY --from=builder /build/tessera-graph-enterprise/target/release/tessera-graph-server /usr/local/bin/tessera-graph-server
COPY --from=builder /build/tessera-graph/target/release/tessera-graph-cli /usr/local/bin/tessera-graph-cli

# Copy TLS certificates
COPY --from=builder /build/certs/server.pem /etc/tessera/certs/server.pem
COPY --from=builder /build/certs/server.key /etc/tessera/certs/server.key

RUN chown -R tessera:tessera /etc/tessera/certs \
    && chmod 600 /etc/tessera/certs/server.key

USER tessera
WORKDIR /var/lib/tessera

# Bolt 4.4 protocol port
EXPOSE 7687
# Prometheus metrics (optional)
EXPOSE 9090

# Environment defaults
ENV TESSERA_BIND=0.0.0.0:7687
ENV TESSERA_TLS_CERT=/etc/tessera/certs/server.pem
ENV TESSERA_TLS_KEY=/etc/tessera/certs/server.key
ENV TESSERA_DATA_DIR=/var/lib/tessera/data
ENV TESSERA_DEFAULT_TENANT=default
ENV TESSERA_MEMORY_LIMIT_MB=256
ENV TESSERA_MAX_CONNECTIONS=256
ENV TESSERA_IDLE_TIMEOUT_SECS=300
ENV TESSERA_AUDIT_PATH=/var/log/tessera/audit.ndjson
ENV TESSERA_AUDIT_ROTATION_MAX_MB=100
ENV TESSERA_METRICS_BIND=0.0.0.0:9090
ENV RUST_LOG=tessera_graph_server=info,tessera_graph_audit=info

# Data and log volumes
VOLUME ["/var/lib/tessera/data", "/var/log/tessera"]

# Health check via /health endpoint (metrics port).
# If TESSERA_METRICS_TOKEN is set, /health requires auth and this check will
# fail — override the healthcheck in docker-compose.yml to include the token.
HEALTHCHECK --interval=30s --timeout=5s --start-period=15s --retries=3 \
  CMD bash -c '</dev/tcp/localhost/9090' || exit 1

STOPSIGNAL SIGTERM

ENTRYPOINT ["/usr/local/bin/tessera-graph-server"]
