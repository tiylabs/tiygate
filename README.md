<div align="center">

# TiyGate

**A lightweight gateway for highly available LLM services.**

Connect OpenAI-compatible, Responses, Messages, and Gemini protocols through one control plane. Route virtual models across providers by policy, capture detailed request/response logs, and run locally as a zero-config desktop app or at scale in containers.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org)
[![Edition: 2024](https://img.shields.io/badge/edition-2024-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![Version: 0.1.0](https://img.shields.io/badge/version-0.1.0-lightgrey.svg)](Cargo.toml)
[![Workspace: 8 crates](https://img.shields.io/badge/workspace-8%20crates-blueviolet.svg)](Cargo.toml)

English | [简体中文](README_zh.md)

</div>

---

<div align="center">
  <img width="1546" height="1079" alt="TiyGate Dashboard 截图" src="https://github.com/user-attachments/assets/c95d421a-9274-4804-a160-a7c9cee7e36c" />
</div>

## What is TiyGate?

TiyGate is an **open-source AI gateway written in Rust** for individuals and teams that use more than one LLM provider, subscription, protocol, or API key. It sits between your applications and upstream providers such as OpenAI, Anthropic, Bedrock, Gemini, and OpenAI-compatible services, turning fragmented provider access into one stable control plane.

Use TiyGate when you want to:

1. **Stop switching between provider subscriptions manually** — connect multiple providers once, then route requests by policy.
2. **Recover automatically from unstable upstream models** — virtual models can fail over and recover across providers / models by priority, weight, throughput, and latency.
3. **Debug unexplained request failures quickly** — every request can be captured with detailed client ↔ gateway ↔ provider request/response logs.
4. **Understand usage across providers, models, and API keys** — analytics aggregate multi-dimensional usage instead of leaving data scattered across dashboards.

## Why TiyGate?

| Capability | What you get |
|---|---|
| **Unified access** | One gateway for OpenAI-compatible, Responses, Messages, Gemini, and embeddings protocols, with extensible N×N protocol translation through a canonical IR. |
| **Policy failover** | Virtual model routing across multiple providers / backend models with priority, weight, throughput, and latency-aware strategies, plus automatic failover and recovery. |
| **Data capture** | Real-time request/response detail capture across the client → gateway → provider path, with retention policy cleanup and optional S3-compatible payload archive. |
| **Lightweight deployment** | A zero-config desktop app for personal macOS / Windows use, and containerized deployment modes for enterprise-scale data plane / control plane separation. The desktop app can manage local and cloud instances. |
| **Security** | Provider API keys are encrypted at rest with `TIYGATE_MASTER_KEY`, and sensitive request/response log fields are redacted. |
| **Backup & restore** | Configuration can be exported and imported with encryption support, making instance migration, backup, and recovery straightforward. |
| **Usage analytics** | Usage statistics are aggregated across providers, models, and API keys for operational visibility. |

## Engineering Principles

TiyGate is designed to keep the hot path reliable while preserving extensibility and maintainability.

| Quality goal | What carries it |
|---|---|
| **Stability** | Per-instance circuit breaker + fine-grained `FallbackPolicy` (error classification, retry vs. failover separated, global attempt/time budget, idempotency gate), respect for upstream `Retry-After`, ingress body/slow-read/concurrency limits, SIGTERM graceful drain, telemetry off the hot path |
| **Extensibility** | Trait + `inventory` decentralized registration (adding a provider = new file + one `submit!`); hook pipeline; `Executor` escape hatch for SDK-style providers; three-segment protocol identity; pluggable strategies, cache, and log sinks |
| **Maintainability** | `core` has zero dependencies on concrete providers/protocols/DB; canonical IR collapses N×N protocol translation to N; field-level capability matrix makes lossiness explicit; heavy dependencies isolated in dedicated crates |

The field-level lossiness matrix used by `lossy_default_reject` lives in [`docs/protocol-capability-matrix.md`](docs/protocol-capability-matrix.md).

## Workspace Layout

```
tiygate/
├── crates/
│   ├── core/               # Canonical IR, traits, pipeline. Zero I/O, zero concrete deps.
│   ├── protocols/          # Protocol codecs (chat_completions, messages, responses, gemini, embeddings)
│   ├── providers/          # Built-in provider metadata + auth
│   ├── provider-bedrock/   # SDK-shape provider (Executor escape hatch), heavy deps isolated
│   ├── store/              # Config OLTP (SQLite/Postgres) + pluggable log sinks
│   ├── cache/              # Embedding cache (deterministic, LLM chat/completion are NOT cached)
│   ├── admin/              # Admin REST API + OAuth flows
│   └── server/             # Ingress, data/control plane assembly, deployment modes
├── webui/                  # Embedded admin console (React + TS + Vite, served at /admin/ui)
├── docs/                   # Architecture design + protocol capability matrix
└── scripts/                # Operational scripts
```

## Choose Your Edition

| Edition | Best for | How to get it |
|---|---|---|
| 🖥️ **Desktop** (recommended for personal use) | Individual users who want a one-click local gateway with a native UI — no Docker, no server setup. macOS (Apple Silicon / Intel) and Windows installers are published on the [Releases](https://github.com/tiylabs/tiygate/releases) page. | Download the installer for your platform from the latest [Release](https://github.com/tiylabs/tiygate/releases) and run it. |
| 🐳 **Docker** (recommended for enterprise / production) | Teams and production deployments that need horizontal scaling, multi-node data/control plane separation, and container orchestration (K8s, Swarm, etc.). | `docker run -d -p 3000:3000 jorbenzhu/tiygate:latest` — see the [Docker image](https://hub.docker.com/r/jorbenzhu/tiygate) and deployment modes below. |

> Don't want to choose? Both editions share the same core engine and Admin Console — you can start with Desktop for local exploration and switch to Docker when you're ready to scale.

## Quick Start

### Prerequisites

- **Rust 1.88+** (`rustup update stable`)
- **Node.js 20+** (for building the embedded WebUI)
- No upstream provider key needed to start — providers are configured in the Admin Console after launch

### Build and run

```bash
git clone https://github.com/tiylabs/tiygate.git
cd tiygate
```

Configure environment variables by copying the template, then fill in the required values:

```bash
cp .env.example .env
```

Edit `.env` — the three variables you must set for a working WebUI:

```bash
# SQLite is the easiest local backend (file is created on first run)
TIYGATE_DATABASE_URL=sqlite://./tiygate.db?mode=rwc

# Admin API token — the WebUI login screen asks for this exact value
TIYGATE_ADMIN_TOKEN=dev-admin-token-change-me

# (Optional but recommended) AES-GCM master key to encrypt provider keys
# / OAuth tokens / S3 credentials at rest. See the Security section below.
# TIYGATE_MASTER_KEY=4f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8
```

Everything else — listen address, deployment mode, logging level — is covered in `.env.example`. **Runtime-tunable parameters** (routing strategy, ingress limits, upstream streaming timeouts, connection-pool tuning, header-forwarding deny-lists, payload-archive to S3, background-task intervals, etc.) are **managed through the Admin Console** at `/admin/ui/settings`. On first start the env values are seeded into the `settings` table as initial defaults; after that the settings table is the single source of truth and changes apply without a restart. The server loads `.env` automatically at startup when the `dotenv` feature is on.

Start the gateway with the embedded WebUI:

```bash
make dev
```

`make dev` builds the frontend first (so `rust-embed` can embed it), then runs the server with the `webui` feature. The default listen address is `0.0.0.0:3000`.

### Open the Admin Console

Once the server is running, open **`http://localhost:3000/admin/ui`** in your browser. Paste your `TIYGATE_ADMIN_TOKEN` on the login screen to enter the console. From there you can manage providers, routes, API keys, runtime settings, and view analytics.

### Smoke test

```bash
curl -sS http://localhost:3000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role": "user", "content": "Say hi in one short sentence."}]
  }'
```

For streaming, add `"stream": true`. The server speaks Server-Sent Events end-to-end.

### Cross-protocol translation

The same gateway will accept `chat_completions` and translate it to `messages` (Anthropic) when you route to that provider — the field-level capability matrix decides what's lossless and rejects combinations that aren't.

## Deployment Modes

The `tiygate` binary supports three modes (selected via `--mode` / env / config):

| Mode | What it runs | When to use |
|---|---|---|
| `all` | Data plane + control plane + DB in one process | Local dev, single-node, small teams |
| `proxy` | Data plane only (stateless, horizontally scalable) | Production data plane |
| `admin` | Control plane only (Admin API + WebUI) | Production control plane |

Health probes are wired by default:

- `GET /healthz` — liveness, returns 200 even while draining (so you don't get killed mid-roll)
- `GET /readyz` — readiness, returns 503 once the pod enters draining (so the load balancer stops sending traffic)

### Admin console (WebUI)

In `all` / `admin` modes the binary serves an embedded React console at **`/admin/ui`** (e.g. `http://localhost:8080/admin/ui`). It covers the full control plane — providers, routes, API keys (with one-time secret + quota editing and live usage), the OAuth authorization-code flow, runtime settings (routing, ingress, upstream, header forwarding, payload archive, background tasks) — plus analytics: per-model / provider / API-key stats, circuit-breaker status, request-log drill-down with replay, and the audit trail. It is bilingual (English / 简体中文).

Authentication reuses the single `TIYGATE_ADMIN_TOKEN`: paste it on the login screen (validated against the Admin API, stored in the browser). The UI is compiled into the binary via `rust-embed` (the opt-in `webui` feature), so the frontend must be built before the Rust crate — run `scripts/build-with-webui.sh`, or `cd webui && npm install && npm run build` followed by `cargo build -p tiygate-server --features webui`. See `webui/README.md` for development details.

## Operations

### Graceful drain

Send `SIGTERM` (or K8s `preStop`) and the gateway:

1. Flips `/readyz` to `503` so the load balancer removes it from the pool
2. Refuses new requests with `503 + Retry-After`
3. Lets in-flight requests (including long SSE streams) finish naturally
4. On `drain_timeout` (default 30s, must be ≥ single-request `deadline`), sends a **protocol-native error frame** to any still-open streams and runs `UsageAccumulator` to prevent billing drift. The streaming path is implemented in `crates/server/src/ingress.rs::drive_upstream_stream` — it also adds a 120s idle timer (tunable via the Admin Console's Upstream settings), an opt-in total wall-clock budget (default disabled), and a 30s SSE keepalive (`SseKeepaliveStream`) so middleboxes do not silently drop long-quiet streams
5. Flushes the telemetry channel, releases resources, exits

### Configuration

TiyGate configuration is split into two layers:

**1. Startup-only environment variables** — read once at process start, require a restart to change:

| Variable | Default | Purpose |
| --- | --- | --- |
| `TIYGATE_LISTEN_ADDR` | `0.0.0.0:3000` | Listen address for the HTTP server. |
| `TIYGATE_MODE` | `all` | Deployment mode. `all` (data + control in one process), `proxy` (data plane only), `admin` (control plane only). |
| `TIYGATE_DATABASE_URL` | unset | Database connection string (SQLite or Postgres). When unset, the server falls back to a legacy in-memory config store with no Admin API. |
| `TIYGATE_ADMIN_TOKEN` | unset | Bearer token required by the Admin API. When unset, Admin API requests are rejected. |
| `TIYGATE_MASTER_KEY` | unset | AES-256-GCM master key used to encrypt provider keys, OAuth tokens, and S3 credentials at rest. Accepts 64 hex chars or standard base64. When unset, secrets are stored in cleartext (the server logs a warning; acceptable for local dev only). |
| `TIYGATE_REDIS_URL` | unset | When set (and built with the `redis-quota` feature), quota counters are shared across replicas via Redis instead of per-replica in-memory. |
| `RUST_LOG` | `info` | `tracing` / `tracing-subscriber` filter. Examples: `info`, `tiygate=debug`, `tiygate_server::ingress=trace`. |

**2. Runtime-tunable settings** — managed through the Admin Console at **`/admin/ui/settings`** (backed by the `settings` table, exposed via `GET/PUT /admin/v1/settings`). These are hot-reloaded: the data plane polls for changes and atomically switches to the new snapshot without a restart.

On first start, the env values below are seeded into the `settings` table as initial defaults; after that, **the settings table is the single source of truth** — editing `.env` again has no effect unless the `settings` table is cleared.

The Settings page is organized into five cards:

| Card | What it controls | Seeded from env |
| --- | --- | --- |
| **Routing & Ingress** | Default routing strategy, max body bytes, max in-flight, max queue depth, acquire timeout, raw-envelope capture media types | `TIYGATE_ROUTING_STRATEGY`, `TIYGATE_MAX_BODY_BYTES`, `TIYGATE_MAX_INFLIGHT`, `TIYGATE_RAW_ENVELOPE_CAPTURE_MEDIA` |
| **Upstream** | Stream idle / total timeouts, TCP keepalive, pool idle timeout, TCP nodelay | `TIYGATE_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS`, `TIYGATE_UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS`, `TIYGATE_UPSTREAM_TCP_KEEPALIVE_SECS`, `TIYGATE_UPSTREAM_POOL_IDLE_TIMEOUT_SECS`, `TIYGATE_UPSTREAM_TCP_NODELAY` |
| **Header Forwarding** | Request / response header deny-lists (comma-separated) | `TIYGATE_FORWARD_REQUEST_HEADER_DENY`, `TIYGATE_FORWARD_RESPONSE_HEADER_DENY` |
| **Payload Archive** | S3-compatible object-storage archiving of full request/response payloads (enabled flag, endpoint, region, bucket, credentials, prefix, force-path-style, scan interval, batch size, concurrency, timeout, max retries) | `TIYGATE_PAYLOAD_ARCHIVE_*` family |
| **Background Tasks** | Log retention interval & days, epoch poll interval, token-stats interval & lookback days | `TIYGATE_LOG_RETENTION_*`, `TIYGATE_EPOCH_POLL_INTERVAL_SECS`, `TIYGATE_TOKEN_STATS_*` |

- **Epoch versioning**: the data plane polls for config changes and atomically switches to the new snapshot; in-flight requests keep the old epoch until they finish — no half-old, half-new state mid-request.
- **Secret encryption**: provider keys / OAuth tokens / encrypted S3 settings are AES-GCM encrypted at rest using `TIYGATE_MASTER_KEY`. Encrypted settings are redacted on `GET /admin/v1/settings`.

### Caching

Only **embedding** requests are cached. LLM chat/completion is **not** cached — by design (non-determinism makes response caching value-low and risk-high). The cache is pluggable: process-local LRU by default, Redis shared backend for multi-replica deployments.

### Payload archive to S3

When enabled, a background worker gzip-compresses the full request/response payload detail of each request (8 objects per request — raw body + parsed metadata for each of the 4 hops: client→gateway, gateway→provider, provider→gateway, gateway→client), uploads them to S3-compatible object storage, verifies sha256/size, and then clears the payload text from the database in the same transaction. This keeps the DB lean for high-volume deployments while preserving full replay fidelity.

The Admin Console's request replay feature transparently hydrates archived objects back from S3 on demand (verify → decompress → return), so the user experience is unchanged whether a request's payloads live in the DB or in object storage.

Object lifecycle is decoupled from DB retention — the worker never deletes from S3; use bucket lifecycle policies for expiry.

Enable and configure payload archiving in the Admin Console under **Settings → Payload Archive**. The env variables (`TIYGATE_PAYLOAD_ARCHIVE_*`) only seed the initial defaults on first start; after that the settings table is authoritative and changes apply without a restart. See `.env.example` for the full variable list.

### Distributed tracing

W3C `traceparent` / `tracestate` are extracted from the inbound request and re-injected on the upstream call. The gateway span attaches to the caller's trace as a parent. Logs and traces are cross-linkable by `trace_id`.

## Development

```bash
# Run the full test suite
cargo test --all-features

# Lint (workspace lints forbid unsafe_code and deny unwrap/expect/panic in libs)
cargo clippy --all-features -- -D warnings

# Format check
cargo fmt --all -- --check

# Workspace-wide dependency tree
cargo tree --workspace

# Verify a heavy-dep crate is isolated (e.g. AWS SDK stays out of core)
cargo tree -p tiygate-core | grep -i aws        # should be empty
cargo tree -p tiygate-provider-bedrock | head   # AWS SDK lives here only
```

The CI baseline is strict: no `#[allow(...)]` workarounds, no `unwrap/expect/panic!` in library code, no dead code.

### Build matrix (`tiygate-server` features)

The `tiygate` binary is feature-gated. Pick the smallest set that matches your deployment so you don't pay compile time or binary size for components you don't ship.

| Feature | What it pulls in | When you need it |
|---|---|---|
| `admin` | `tiygate-admin` (control plane, Admin API, OAuth) | `admin` / `all` deploy mode |
| `cache` | `tiygate-cache` (in-memory response cache) | Anywhere that benefits from caching |
| `providers` | `tiygate-providers` (OpenAI / Anthropic / generic OpenAI-compatible) | Any non-Bedrock LLM traffic |
| `bedrock` | `tiygate-provider-bedrock` (AWS SDK) | Routes that target AWS Bedrock |
| `tracing` | `tracing-subscriber` with JSON formatter | The default `tiygate` binary |
| `dotenv` | `dotenvy` — auto-load `.env` at startup | Local development |
| `webui` | `rust-embed` — embeds `webui/dist` and serves the admin console at `/admin/ui` | `admin` / `all` deploy mode with a UI |

**Defaults:** `admin`, `cache`, `providers`, `tracing`, `dotenv` — the common case. **`bedrock` is opt-in** (it pulls the heavy AWS SDK) — add it explicitly if you need AWS Bedrock routes. **`webui` is also opt-in**: it embeds `webui/dist` at compile time, so **build the frontend first** (`cd webui && npm install && npm run build`) and then build with `--features webui`, or just run `scripts/build-with-webui.sh` which does both in order.

```bash
# Default build (everything except Bedrock — that's now opt-in)
cargo build -p tiygate-server --release

# Add Bedrock back when you need it
cargo build -p tiygate-server --release --features bedrock

# Minimal data-plane proxy — drop admin / cache / bedrock
cargo build -p tiygate-server --release \
  --no-default-features --features "providers,tracing,dotenv"

# Bedrock-only — skip OpenAI / Anthropic to keep the binary lean
cargo build -p tiygate-server --release \
  --no-default-features --features "bedrock,tracing,dotenv"

# Control-plane only — for the `admin` deploy mode
cargo build -p tiygate-server --release \
  --no-default-features --features "admin,tracing,dotenv"

# Inspect what's actually compiled in
cargo tree -p tiygate-server -e features --depth 1
```

> **`bedrock` is opt-in by design.** Compiling the AWS SDK is the single biggest hit to your cold-build time, so we keep it out of the default. If you route to Bedrock, opt in explicitly:
>
> ```bash
> cargo build -p tiygate-server --release --features bedrock
> ```
>
> **CI smoke matrix** — `bash scripts/verify-deps.sh` will still pass under any feature combination, because dependency isolation lives in `core` / `providers` and is enforced separately from the `server` build matrix.

## Contributing

Issues and pull requests are welcome. The design is opinionated, and contributions that fight the layering (e.g. adding a concrete provider dependency to `core`, or introducing `allow_lossy`) will be declined.

## License

[Apache-2.0](LICENSE)

---

<div align="center">
<sub>Built by <a href="https://github.com/tiylabs">tiylabs</a> · <a href="docs/protocol-capability-matrix.md">Capability Matrix</a></sub>
</div>
