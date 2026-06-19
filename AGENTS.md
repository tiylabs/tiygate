# Repository Guidelines

Contributor guide for **TiyGate**, an open-source Rust AI Gateway. Preserve the crate layering described below; it is intentional and enforced.

## Project Structure & Module Organization

This repository is a Cargo workspace with 8 crates and an embedded admin console:

- `crates/core` contains canonical IR, traits, and pipeline code. It must stay zero-I/O and free of concrete provider, protocol, or database dependencies.
- `crates/protocols` implements protocol codecs for chat completions, messages, responses, Gemini, and embeddings.
- `crates/providers` stores built-in provider metadata and auth; `crates/provider-bedrock` isolates heavy AWS Bedrock SDK dependencies.
- `crates/store`, `crates/cache`, `crates/admin`, and `crates/server` cover config storage, embedding cache, Admin REST/OAuth, and the `tiygate` binary.
- `webui/` is the React + TypeScript + Vite admin console, embedded with `rust-embed` and served at `/admin/ui`.
- `docs/` holds architecture and protocol capability docs; `scripts/` contains helper checks such as `verify-deps.sh`.
- Tests live in `crates/<crate>/tests/` for integration tests and inline `#[cfg(test)]` modules for unit tests.

## Build, Test, and Development Commands

Run commands from the repository root. Use `make help` for the full list.

```bash
make build        # Build webui, then release Rust binary
make build-debug  # Build webui, then debug Rust binary
make dev          # Build webui and run tiygate-server with webui feature
make dev-server   # Run Rust server only, without webui embedding
make dev-web      # Start the WebUI Vite dev server
make check        # cargo check --workspace --all-targets --all-features
make test         # cargo test --workspace --all-features
make lint         # rustfmt check, clippy -D warnings, and webui tsc
make fmt          # Format Rust and WebUI code
make audit        # Run cargo audit and npm audit
```

## Coding Style & Naming Conventions

Rust uses `rustfmt` defaults and mandatory `clippy --all-targets --all-features -- -D warnings`. Workspace lints in `Cargo.toml` forbid `unsafe_code` and deny `unwrap_used`, `expect_used`, and `panic` in library code. Do not add `#[allow(...)]` workarounds or dead code. Crate names follow `tiygate-<module>`, and internal dependencies should use `[workspace.dependencies]` entries with `.workspace = true`. WebUI code uses TypeScript, Prettier, Tailwind CSS v4, and Radix UI primitives.

## Testing Guidelines

Use Rust `#[test]` plus workspace test tools including `wiremock`, `mockall`, `proptest`, `insta`, `criterion`, and `serial_test`. Use `serial_test` for tests that race on environment variables, as in admin and store tests. Run `make test` before submitting. For provider or protocol changes, add coverage in `crates/protocols/tests/` and `crates/server/tests/`, and document lossy translation behavior against `docs/protocol-capability-matrix.md`.

## Commit & Pull Request Guidelines

Commit history uses Conventional Commits with gitmoji, for example `feat: ✨ ...`, `fix(webui): 🐛 ...`, and `chore(ci): 🔧 ...`. PRs should describe what changed and why, link related issues, and include test output for data-path or protocol changes. Keep the `webui` feature opt-in; do not add it to default features. Verify layering with `scripts/verify-deps.sh` when touching crate dependencies.

## Security & Configuration Tips

Configuration is split into two layers. **Startup-only environment variables** (`TIYGATE_LISTEN_ADDR`, `TIYGATE_MODE`, `TIYGATE_DATABASE_URL`, `TIYGATE_ADMIN_TOKEN`, `TIYGATE_MASTER_KEY`, `TIYGATE_REDIS_URL`, `RUST_LOG`) are read once at process start and require a restart to change. `.env` is loaded when the `dotenv` feature is enabled, but real provider keys must never be committed — use `.env.example` as the template. **Runtime-tunable settings** (routing strategy, ingress limits, upstream streaming/pool tuning, header-forwarding deny-lists, payload-archive to S3, background-task intervals, etc.) are managed through the Admin Console at `/admin/ui/settings` (backed by the `settings` table, API `GET/PUT /admin/v1/settings`); env values only seed the initial defaults on first start, after which the settings table is the single source of truth and changes hot-reload without a restart. `TIYGATE_ADMIN_TOKEN` gates the Admin API, and `TIYGATE_MASTER_KEY` controls AES-GCM encryption for stored provider secrets, OAuth tokens, and S3 credentials.
