# TiyGate WebUI

The operator console for TiyGate — a React + TypeScript + Vite single-page
app that drives the admin REST API (`/admin/v1/*`). It is compiled into the
`tiygate` binary via `rust-embed` and served at **`/admin/ui`**.

## Stack

- **React 18 + TypeScript + Vite 6**
- **TanStack Query** for server state and cache invalidation
- **Tailwind CSS v4** (`@tailwindcss/vite`) with a small hand-rolled UI kit
- **react-router-dom** (SPA, `basename="/admin/ui"`)
- **react-i18next** — bilingual (English / 简体中文)

## Authentication

The UI holds a single super-user admin token (`TIYGATE_ADMIN_TOKEN`). On the
login screen, paste the token; it is validated with a probe request to
`GET /admin/v1/audit?limit=1`, then stored in `sessionStorage` (or
`localStorage` when "remember" is checked). Every request attaches
`Authorization: Bearer <token>`. A `401` clears the token and bounces back to
login; a `503` means the server has no admin token configured.

## Development

```bash
cd webui
npm install
npm run dev      # Vite dev server on :5173, proxies /admin/v1 → :3000
```

Run a TiyGate gateway (`all` mode with a DB and admin token) on
`localhost:3000` so the dev server can reach the API:

```bash
TIYGATE_DATABASE_URL=sqlite://tiygate.db \
TIYGATE_ADMIN_TOKEN=dev-secret \
cargo run -p tiygate-server
```

## Production build

```bash
npm run build    # type-check + bundle into webui/dist
```

`webui/dist` is what `rust-embed` embeds. **The frontend must be built before
the Rust crate is compiled in release mode.** Use the helper:

```bash
../scripts/build-with-webui.sh
```

## Pages

- **Dashboard** — request/error/token aggregates by model, provider, and API
  key, plus circuit-breaker status. Cost is hidden until a price provider is
  configured.
- **Providers / Routes** — full CRUD. Provider secrets are write-only (blank
  keeps the existing encrypted value); the GET response is always redacted.
- **API Keys** — create (one-time secret modal), edit quota with live usage
  bars, disable (PUT), and delete.
- **OAuth** — run the authorization-code `start` flow and `refresh` tokens.
- **Request Logs** — filter, paginate, and drill into the replay envelope
  (headers redacted, oversized bodies truncated).
- **Audit** — recent control-plane mutations.
