# TiyGate Admin API

The Admin API is the operator surface for `tiygate`. It manages
**providers**, **routes**, **api-keys**, exposes the **stats**
endpoints for the dashboard, and is the source of truth for the
**audit log**.

All endpoints live under `/admin/v1` and require a bearer token
in the `Authorization` header:

```
Authorization: Bearer $TIYGATE_ADMIN_TOKEN
```

## Authentication

* The bearer token is supplied via the `TIYGATE_ADMIN_TOKEN` env
  variable. The token is compared in constant time using the
  `subtle` crate.
* A missing token returns `401 Unauthorized`. An invalid token
  returns `401 Unauthorized`. A request that arrives when the
  env var is unset returns `503 Service Unavailable` with a clear
  error message so operators can spot the misconfiguration in
  their dashboards.

## Endpoints

### `GET /admin/v1/health`

Liveness check; returns `{"status":"ok"}`. No auth required by
convention, but the bearer middleware still applies unless the
control plane is disabled.

### Providers

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/admin/v1/providers` | List providers (optional `?enabled=true|false`) |
| `POST` | `/admin/v1/providers` | Create or upsert a provider |
| `GET` | `/admin/v1/providers/:id` | Get a single provider |
| `PUT` | `/admin/v1/providers/:id` | Update a provider |
| `DELETE` | `/admin/v1/providers/:id` | Delete a provider |

Provider body:

```json
{
  "id": "openai",
  "name": "OpenAI",
  "vendor": "openai",
  "api_base": "https://api.openai.com/v1",
  "api_key": "sk-...",
  "auth_mode": "api_key",   // api_key | oauth | iam | none
  "oauth_meta": "...",      // optional JSON string
  "metadata": { "...": "..." },
  "enabled": true
}
```

The `api_key` is encrypted at rest with AES-256-GCM. The
`GET` response never returns the cleartext secret; the
`encrypted_api_key` field is replaced with a `[encrypted:…]`
marker.

### Routes

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/admin/v1/routes` | List routes |
| `POST` | `/admin/v1/routes` | Create or upsert a route |
| `GET` | `/admin/v1/routes/:id` | Get a single route |
| `PUT` | `/admin/v1/routes/:id` | Update a route |
| `DELETE` | `/admin/v1/routes/:id` | Delete a route |

Route body:

```json
{
  "virtual_model": "gpt-4o",
  "targets": [
    {
      "provider_id": "openai",
      "model_id": "gpt-4o",
      "weight": 1.0,
      "account_label": "team-a"
    }
  ],
  "enabled": true
}
```

### API keys

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/admin/v1/api-keys` | List keys (cleartext never returned) |
| `POST` | `/admin/v1/api-keys` | Create a key (returns the cleartext *once*) |
| `GET` | `/admin/v1/api-keys/:id` | Fetch one key + real-time usage (`usage` map) |
| `PUT` | `/admin/v1/api-keys/:id` | Disable a key (status → `disabled`) |
| `PATCH` | `/admin/v1/api-keys/:id` | Update the key's quota JSON only |
| `DELETE` | `/admin/v1/api-keys/:id` | Delete a key |

`POST` request body:

```json
{
  "name": "agent-1",
  "secret": "tg-...",      // optional; auto-generated when absent
  "quota": { "requests_per_minute": 60 }
}
```

`GET /admin/v1/api-keys/:id` returns the `ApiKeyView` fields plus a
flattened `usage` object. When a live quota counter is wired into the
control plane the `usage` map carries the current consumption per
bucket (`requests_per_minute`, `requests_per_day`, `tokens_per_minute`,
`tokens_per_day`); otherwise it is `{}`:

```json
{
  "id": "0190...",
  "name": "agent-1",
  "key_hash": "…",
  "quota": { "requests_per_minute": 60 },
  "status": "active",
  "usage": { "requests_per_minute": 12 }
}
```

`PATCH /admin/v1/api-keys/:id` replaces only `quota_json` (it never
touches `status`, so it is independent of the `PUT` disable verb):

```json
{ "quota": { "requests_per_minute": 100, "tokens_per_day": 1000000 } }
```

The `secret` is shown **once** in the response and is never
recoverable from the database — only the SHA-256 hash is stored.

### OAuth (provider authorization-code flow)

| Method | Path | Description |
| --- | --- | --- |
| `POST` | `/admin/v1/oauth/start` | Mint a `state` CSRF nonce + PKCE code-verifier; return the authorization URL the user-agent must be redirected to |
| `GET` | `/admin/v1/oauth/callback?code=…&state=…` | Provider redirect target; exchanges the auth code for an access token and persists the encrypted refresh-token metadata |
| `POST` | `/admin/v1/oauth/refresh` | Refresh an existing provider's access token without user interaction |

The `callback` and `refresh` routes both look up the provider
row in `providers.metadata_json.oauth` for the OAuth config;
the `callback` additionally writes the refresh-token metadata
back to `providers.encrypted_oauth_meta`. See
`docs/admin-api.md` §OAuth for the full flow diagram.

### Stats

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/admin/v1/stats/by-model` | Aggregate by `virtual_model` |
| `GET` | `/admin/v1/stats/by-provider` | Aggregate by `resolved_provider` |
| `GET` | `/admin/v1/stats/by-api-key` | Aggregate by `api_key_id` |

All stats endpoints accept `?since=<rfc3339>&until=<rfc3339>`
query parameters. When omitted, `since` defaults to 24h ago and
`until` defaults to "now".

Response shape:

```json
{
  "since": "2026-06-10T00:00:00Z",
  "until": "2026-06-11T00:00:00Z",
  "buckets": [
    { "bucket": "gpt-4o", "count": 12, "error_count": 1, "prompt_tokens": 400, "completion_tokens": 800, "total_tokens": 1200 }
  ]
}
```

### Audit log

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/admin/v1/audit?limit=N` | Most recent audit entries (default 50, max 500) |

Every successful write operation (create / update / delete on
any of providers, routes, api_keys) writes one row. The
`details` field is the JSON payload that was sent.

## Error responses

All errors follow the design doc §3.5 envelope:

```json
{ "error": { "message": "...", "type": "not_found", "source": "gateway" } }
```

* `error.source` is `gateway` for gateway-produced errors and
  `upstream` for errors forwarded from an upstream service.
* The HTTP status is the source of truth (404 for not found,
  400 for bad request, 503 for admin not configured, etc.).
