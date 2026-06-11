<div align="center">

# TiyGate

**An open-source AI Gateway built for stability, extensibility, and operability.**

Multi-provider / multi-model access with first-class observability, dynamic configuration, and graceful operations.

[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Rust: 1.78+](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)
[![Edition: 2021](https://img.shields.io/badge/edition-2021-orange.svg)](https://doc.rust-lang.org/edition-guide/)
[![Version: 0.1.0](https://img.shields.io/badge/version-0.1.0-lightgrey.svg)](Cargo.toml)
[![Workspace: 8 crates](https://img.shields.io/badge/workspace-8%20crates-blueviolet.svg)](Cargo.toml)

English | [简体中文](README_zh.md)

</div>

---

## What is TiyGate?

TiyGate is an **independent AI Gateway product** written in Rust. It sits between your applications and upstream LLM providers (OpenAI, Anthropic, Bedrock, and any OpenAI-compatible service) and gives you a single, stable control point for routing, observability, and policy.

The two things it does best:

1. **Multi-backend / multi-model access** — one canonical entry, many providers. Cross-protocol translation (e.g. OpenAI `chat_completions` → Anthropic `messages`) is a first-class capability, not a hack.
2. **Logs and analytics** — every request is captured, structured, and routed to a hot-path-safe async pipeline. No blocking the request path. No silent drops.

## Why TiyGate?

Most gateways optimize for one dimension. TiyGate is engineered to hold three at once.

| Quality goal | What carries it |
|---|---|
| **Stability** | Per-instance circuit breaker + fine-grained `FallbackPolicy` (error classification, retry vs. failover separated, global attempt/time budget, idempotency gate), respect for upstream `Retry-After`, ingress body/slow-read/concurrency limits, SIGTERM graceful drain, telemetry off the hot path |
| **Extensibility** | Trait + `inventory` decentralized registration (adding a provider = new file + one `submit!`); hook pipeline; `Executor` escape hatch for SDK-style providers; three-segment protocol identity; pluggable strategies, cache, and log sinks |
| **Maintainability** | `core` has zero dependencies on concrete providers/protocols/DB; canonical IR collapses N×N protocol translation to N; field-level capability matrix makes lossiness explicit; heavy dependencies isolated in dedicated crates |

The full design rationale lives in [`docs/ai-gateway-architecture-design.md`](docs/ai-gateway-architecture-design.md). The field-level lossiness matrix used by `lossy_default_reject` lives in [`docs/protocol-capability-matrix.md`](docs/protocol-capability-matrix.md).

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
├── docs/                   # Architecture design + protocol capability matrix
└── scripts/                # Operational scripts
```

## Quick Start

### Prerequisites

- **Rust 1.78+** (`rustup update stable`)
- An upstream provider key, e.g. `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`

### Build and run (zero-config bootstrap)

```bash
# Clone and build
git clone https://github.com/tiylabs/tiygate.git
cd tiygate
cargo build --release

# Set a provider key — gateway will auto-detect on first request
export OPENAI_API_KEY="sk-..."

# Start the gateway (default mode: all-in-one, default port: 8080)
./target/release/tiygate
```

You should see structured JSON logs including `TiyGate AI Gateway v0.1.0` and `Listening on ...`.

### Smoke test

```bash
curl -sS http://localhost:8080/v1/chat/completions \
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

## Operations

### Graceful drain

Send `SIGTERM` (or K8s `preStop`) and the gateway:

1. Flips `/readyz` to `503` so the load balancer removes it from the pool
2. Refuses new requests with `503 + Retry-After`
3. Lets in-flight requests (including long SSE streams) finish naturally
4. On `drain_timeout` (default 30s, must be ≥ single-request `deadline`), sends a **protocol-native error frame** to any still-open streams and runs `UsageAccumulator` to prevent billing drift. The streaming path is implemented in `crates/server/src/ingress.rs::drive_upstream_stream` — it also adds a 120s idle timer (configurable via `TIYGATE_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS`), an opt-in total wall-clock budget (`TIYGATE_UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS`, default disabled), and a 30s SSE keepalive (`SseKeepaliveStream`) so middleboxes do not silently drop long-quiet streams
5. Flushes the telemetry channel, releases resources, exits

### Environment variables

All TiyGate knobs are read from environment variables. Unknown keys are ignored. The gateway also loads `.env` from the working directory at startup (when the `dotenv` feature is on).

#### Server core

| Variable | Default | Purpose |
| --- | --- | --- |
| `TIYGATE_LISTEN_ADDR` | `0.0.0.0:3000` | Listen address for the HTTP server. |
| `TIYGATE_MODE` | `all` | Deployment mode. `all` (data + control in one process), `proxy` (data plane only), `admin` (control plane only). |
| `TIYGATE_MAX_BODY_BYTES` | `10485760` (10 MiB) | Per-request body size limit for plain text / JSON. |
| `TIYGATE_MAX_INFLIGHT` | `1024` | Maximum concurrent in-flight requests. Beyond this, additional requests queue and are eventually rejected with `503 + Retry-After`. |
| `TIYGATE_ROUTING_STRATEGY` | `weighted` | Routing strategy across targets. `weighted` (default §3.4), `priority`, `cooldown`, `latency`. |

#### Streaming lifecycle (see `crates/server/src/ingress.rs::drive_upstream_stream`)

| Variable | Default | Purpose |
| --- | --- | --- |
| `TIYGATE_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` | `120` | Idle window for upstream streaming responses. If no chunk arrives for this long, the stream is closed with a protocol-native end frame. |
| `TIYGATE_UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS` | `0` (disabled) | Wall-clock budget for upstream streaming responses. When it elapses, the stream is closed with a protocol-native error frame. Set to `0` to opt out. |

#### Providers — auto-routing on first boot

| Variable | Purpose |
| --- | --- |
| `OPENAI_API_KEY` | If set, auto-registers routes for `gpt-4o`, `gpt-4o-mini`, and `gpt-3.5-turbo` pointing at `https://api.openai.com/v1`. |
| `ANTHROPIC_API_KEY` | If set, auto-registers a route for `claude-sonnet-4-20250514` pointing at `https://api.anthropic.com/v1`. |
| `OPENAI_COMPATIBLE_BASE_URL` | Base URL of a generic OpenAI-compatible provider (Ollama, vLLM, DeepSeek, Moonshot, etc.). Required for the openai-compatible provider to register. |
| `OPENAI_COMPATIBLE_API_KEY` | API key for the generic provider above. Defaults to `not-needed` when omitted (suitable for local / unauthenticated endpoints). |

#### Security

| Variable | Default | Purpose |
| --- | --- | --- |
| `TIYGATE_ADMIN_TOKEN` | unset | Bearer token required by the Admin API. When unset, Admin API requests are rejected. |
| `TIYGATE_MASTER_KEY` | unset | Master key used to AES-GCM-encrypt provider keys/tokens at rest. **Planned for the DB-backed phase; the in-memory config store does not yet read it.** Treat unset as "not encrypted" today. |

#### Observability

| Variable | Default | Purpose |
| --- | --- | --- |
| `RUST_LOG` | `info` | `tracing` / `tracing-subscriber` filter. Examples: `info`, `tiygate=debug`, `tiygate_server::ingress=trace`. |

### Configuration

- **Zero-config bootstrap**: env vars like `OPENAI_API_KEY` are auto-detected
- **DB-driven config** (OLTP): provider / route / API key CRUD via Admin API, no restart required
- **Epoch versioning**: data plane polls for config changes, atomically switches to the new snapshot; in-flight requests keep the old epoch until they finish — no half-old, half-new state mid-request
- **Secret encryption**: provider keys/tokens are AES-GCM encrypted at rest; the master key is read from `TIYGATE_MASTER_KEY`

### Caching

Only **embedding** requests are cached. LLM chat/completion is **not** cached — by design (non-determinism makes response caching value-low and risk-high). The cache is pluggable: process-local LRU by default, Redis shared backend for multi-replica deployments.

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

**Defaults:** `admin`, `cache`, `providers`, `tracing`, `dotenv` — the common case. **`bedrock` is opt-in** (it pulls the heavy AWS SDK) — add it explicitly if you need AWS Bedrock routes.

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

## Project Status

TiyGate is at **v0.1.0** and the public API is not yet stable. The architecture is fully designed in [`docs/ai-gateway-architecture-design.md`](docs/ai-gateway-architecture-design.md), broken into 5 phases:

| Phase | Theme | Exit signal |
|---|---|---|
| 1 | Core kernel + minimal proxy | Cross-protocol translation works, zero-config boots |
| 2 | Reliability layer | Circuit breaker, failover, timeouts, ingress guards |
| 3 | Breadth | 5 protocols, multi-provider, Executor escape hatch |
| 4 | Productization | Dynamic config, log dashboards, quotas, key encryption |
| 5 | Scale | Multi-replica, split deployment, probes, graceful drain |

Each phase is independently demonstrable. See the design doc for the full delivery list, acceptance criteria, and risks.

## Contributing

Issues and pull requests are welcome. For non-trivial changes, please read the architecture doc first — the design is opinionated, and contributions that fight the layering (e.g. adding a concrete provider dependency to `core`, or introducing `allow_lossy`) will be declined.

## License

[Apache-2.0](LICENSE)

---

<div align="center">
<sub>Built by <a href="https://github.com/tiylabs">tiylabs</a> · <a href="docs/ai-gateway-architecture-design.md">Architecture</a> · <a href="docs/protocol-capability-matrix.md">Capability Matrix</a></sub>
</div>
