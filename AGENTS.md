# Repository Guidelines

Contributor guide for **TiyGate** — an open-source AI Gateway written in Rust. Read the architecture doc (`docs/ai-gateway-architecture-design.md`) before non-trivial changes; the layering is opinionated.

## Project Structure & Module Organization

A Cargo workspace of 8 crates plus an embedded admin console:

- `crates/core` — Canonical IR, traits, pipeline. **Zero I/O, zero concrete deps.**
- `crates/protocols` — Protocol codecs (chat_completions, messages, responses, gemini, embeddings).
- `crates/providers` — Built-in provider metadata + auth (OpenAI, Anthropic, generic OpenAI-compatible).
- `crates/provider-bedrock` — AWS Bedrock SDK provider (heavy deps isolated here).
- `crates/store` — Config OLTP (SQLite/Postgres) + pluggable log sinks.
- `crates/cache` — Embedding cache (LLM chat/completion is intentionally **not** cached).
- `crates/admin` — Admin REST API + OAuth flows.
- `crates/server` — Ingress, data/control plane assembly, the `tiygate` binary.
- `webui/` — React + TypeScript + Vite admin console, embedded via `rust-embed` and served at `/admin/ui`.
- `docs/` — Architecture design + protocol capability matrix.
- `scripts/` — `build-with-webui.sh`, `verify-deps.sh`.
- Tests live in `crates/<crate>/tests/` (integration) and inline `#[cfg(test)]` modules (unit).

## Build, Test, and Development Commands

A top-level `Makefile` wraps the common workflows. Run `make help` to list all targets.

```bash
make build         # Release build (builds webui first for rust-embed, then cargo build --release)
make build-debug   # Debug build
make dev           # Build webui, then cargo run -p tiygate-server --features webui
make dev-server    # Rust server only (default features, no webui embed)
make dev-web       # WebUI dev server only (cd webui && npm run dev)
make test          # cargo test --workspace --all-features
make test-cov      # cargo llvm-cov coverage report (needs cargo-llvm-cov)
make lint          # cargo fmt --check + clippy -D warnings + webui tsc --noEmit
make fmt           # Format Rust (cargo fmt) + webui (prettier)
make check         # cargo check --workspace --all-targets --all-features (faster than build)
make audit         # cargo audit + npm audit (dependency security)
make doc           # Generate and open Rust docs
make clean         # Clean Rust + webui build artifacts
```

Direct equivalents: `cargo test --all-features`, `cargo clippy --all-features -- -D warnings`, `cargo fmt --all -- --check`.

## Coding Style & Naming Conventions

- **Rust**: `rustfmt` defaults, enforced via `cargo fmt --all -- --check`. `clippy` with `-D warnings` is mandatory.
- **Workspace lints** (in `Cargo.toml`): `unsafe_code` is **forbidden**; `unwrap_used`, `expect_used`, and `panic` are **denied** in library code. No `#[allow(...)]` workarounds, no dead code.
- **WebUI**: TypeScript with `tsc --noEmit` (`make webui-lint`) and Prettier (`make webui-fmt`). Uses Tailwind CSS v4 + Radix UI primitives.
- Crate names follow the `tiygate-<module>` pattern; internal crate deps are declared in `[workspace.dependencies]` and referenced as `tiygate-<module>.workspace = true`.

## Testing Guidelines

- Framework: Rust's built-in `#[test]`, plus `wiremock`, `mockall`, `proptest`, `insta`, and `criterion` (benchmarks) from workspace deps.
- Integration tests: `crates/<crate>/tests/*.rs`. Use `serial_test` for tests that race on environment variables (see `crates/admin/tests/integration.rs`, `crates/store/src/config_store.rs`).
- Run everything: `make test` (or `cargo test --workspace --all-features`).
- Coverage: `make test-cov` (requires `cargo-llvm-cov`; the Makefile auto-detects Homebrew LLVM paths).
- When adding a provider/protocol, add the corresponding `crates/protocols/tests/` and `crates/server/tests/` cases — cross-protocol translation lossiness must be made explicit against the capability matrix in `docs/protocol-capability-matrix.md`.

## Commit & Pull Request Guidelines

- **Commit messages** follow Conventional Commits with a gitmoji, e.g. `feat: ✨ ...`, `fix(webui): 🐛 ...`, `docs: 📝 ...`, `ci: 👷 ...`. Match the existing history style.
- **Layering is enforced**: never add a concrete provider/protocol/DB dependency to `core`, and never introduce `allow_lossy`. Verify with `scripts/verify-deps.sh` (e.g. `cargo tree -p tiygate-core | grep -i aws` must be empty).
- **PRs**: include a description of what changed and why; link any related issue. For features touching the data path or protocols, attach test output demonstrating the behavior. Keep the `webui` feature opt-in — do not add it to `default` (it embeds `webui/dist` at compile time).
- CI baseline is strict: no `unwrap/expect/panic!` in library code, no `#[allow(...)]`, no dead code, no unchecked warnings.

## Security & Configuration Tips

- All knobs are environment variables (see the README table). `.env` is auto-loaded when the `dotenv` feature is on.
- `TIYGATE_ADMIN_TOKEN` gates the Admin API; leave unset to disable it. `TIYGATE_MASTER_KEY` drives AES-GCM at-rest encryption of provider secrets.
- Never commit real provider keys. Use `.env.example` as the template and keep `.env` out of version control (it is gitignored).
