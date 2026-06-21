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

## Quick Start

Choose the path that matches your use case:

- **Desktop:** 🖥️  download the latest macOS or Windows installer from [Releases](https://github.com/tiylabs/tiygate/releases), launch TiyGate, then configure providers and virtual models in the Admin Console.
- **Docker:** 🐳 run `docker run -d -p 3000:3000 jorbenzhu/tiygate:latest`, then follow [Deployment and Operations](docs/deployment-operations.md) for production configuration.
- **From source:** 🦀 use Rust 1.88+ and Node.js 20+.

```bash
git clone https://github.com/tiylabs/tiygate.git
cd tiygate
cp .env.example .env
make dev
```

Set at least `TIYGATE_DATABASE_URL` and `TIYGATE_ADMIN_TOKEN` in `.env`; set `TIYGATE_MASTER_KEY` to encrypt provider keys, OAuth tokens, and S3 credentials at rest. After startup, open **`http://localhost:3000/admin/ui`** and paste the admin token to manage providers, routes, API keys, runtime settings, logs, and analytics.

## Documentation

- [Deployment and Operations](docs/deployment-operations.md) covers deployment modes, health probes, configuration, graceful drain, caching, S3 payload archive, and tracing.
- [Protocol Capability Matrix](docs/protocol-capability-matrix.md) documents protocol conversion behavior and lossy-field handling.
- [Request Logging](docs/request-logging.md) explains request/response capture and replay details.

## Development

```bash
make check        # cargo check --workspace --all-targets --all-features
make test         # cargo test --workspace --all-features
make lint         # rustfmt check, clippy -D warnings, and webui tsc
make fmt          # Format Rust and WebUI code
```

See [AGENTS.md](AGENTS.md) for contributor rules, layering constraints, and coding standards. See [webui/README.md](webui/README.md) for Admin Console development.

## Contributing

Issues and pull requests are welcome. The design is opinionated, and contributions that fight the layering (e.g. adding a concrete provider dependency to `core`, or introducing `allow_lossy`) will be declined.

## License

[Apache-2.0](LICENSE)

---

<div align="center">
<sub>Built by <a href="https://github.com/tiylabs">tiylabs</a> · <a href="docs/protocol-capability-matrix.md">Capability Matrix</a></sub>
</div>
