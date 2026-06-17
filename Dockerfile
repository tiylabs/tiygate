# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.88
ARG NODE_VERSION=20

FROM node:${NODE_VERSION}-bookworm-slim AS webui-builder
WORKDIR /app/webui
COPY webui/package*.json ./
RUN npm ci
COPY webui/ ./
RUN npm run build

FROM rust:${RUST_VERSION}-bookworm AS rust-builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY --from=webui-builder /app/webui/dist ./webui/dist

# Build a production all-in-one gateway with embedded WebUI and Redis-backed quota support.
# Bedrock remains opt-in to keep the image smaller; pass --build-arg SERVER_FEATURES="webui tiygate-core/redis-quota bedrock" if needed.
ARG SERVER_FEATURES="webui tiygate-core/redis-quota"
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build -p tiygate-server --release --features "${SERVER_FEATURES}" && \
    cp /app/target/release/tiygate /usr/local/bin/tiygate

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl tini && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --system --gid 10001 tiygate && \
    useradd --system --uid 10001 --gid tiygate --home-dir /app --shell /usr/sbin/nologin tiygate

COPY --from=rust-builder /usr/local/bin/tiygate /usr/local/bin/tiygate
# Runtime migrations are loaded from the store crate's CARGO_MANIFEST_DIR-relative path.
COPY crates/store/migrations ./crates/store/migrations

RUN mkdir -p /data && chown -R tiygate:tiygate /app /data

USER tiygate
EXPOSE 3000

ENV TIYGATE_LISTEN_ADDR=0.0.0.0:3000 \
    TIYGATE_MODE=all \
    TIYGATE_DATABASE_URL=sqlite:///data/tiygate.db?mode=rwc \
    RUST_LOG=info

HEALTHCHECK --interval=15s --timeout=3s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:3000/readyz || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["/usr/local/bin/tiygate"]
