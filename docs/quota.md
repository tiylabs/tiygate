# TiyGate Quota Configuration

Quotas gate the request hot path by **API key id** (the
`api_keys.id` from the control plane) and are enforced *before*
the gateway forwards a request upstream. A denied request
returns `429 Too Many Requests` with a `Retry-After` header.

## Counter kinds

The `QuotaSpec` accepts four optional limits:

| Field | Meaning | Window |
| --- | --- | --- |
| `requests_per_minute` | Maximum requests per key per minute | 60 s |
| `requests_per_day` | Maximum requests per key per day | 86 400 s |
| `tokens_per_minute` | Maximum input + output tokens per minute | 60 s |
| `tokens_per_day` | Maximum input + output tokens per day | 86 400 s |

A key with an *empty* spec is unlimited.

## Where the spec lives

The quota spec is part of the `api_keys.quota_json` column. The
default shape is `{}` (unlimited). Operators set it through the
admin API:

```bash
curl -X PATCH http://localhost:3000/admin/v1/api-keys/<id> \
  -H "Authorization: Bearer $TIYGATE_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "quota": { "requests_per_minute": 60, "tokens_per_day": 1000000 } }'
```

> `PATCH` updates *only* the quota JSON. The `PUT` verb on the same
> path **disables** the key (sets `status = disabled`) and takes no
> body, so the two operations never collide.

## Backends

The Phase 4 default backend is the in-memory `InMemoryQuota`,
which is per-replica. Multi-replica deployments can swap in the
`RedisQuota` implementation by setting
`TIYGATE_REDIS_URL=redis://host:6379` (the feature flag is
reserved for Phase 5; the trait surface is stable and the
in-memory implementation is the wire-compatible fallback).

### Per-replica semantics

`InMemoryQuota` keeps counters in process memory. The counters
are *lost on restart*. This is acceptable for single-replica
deployments and is the same trade-off BitRouter's per-instance
circuit breaker makes (§3.4). For multi-replica production
deployments, prefer the Redis backend (Phase 5) so quotas are
globally consistent.

## What the request sees

1. The gateway extracts the API key from the request header.
2. The associated `QuotaSpec` is loaded from the `api_keys`
   table (cached for the duration of the request).
3. Before the upstream call, `check_and_consume` is called with
   the prompt + completion token estimate. If the request is
   denied, the gateway returns:
   * `429 Too Many Requests`
   * `Retry-After: <seconds>` (the `retry_after` value reported
     by the counter)
   * JSON body with `error.source = "gateway"` and
     `error.type = "rate_limited"`.

## Observation

`GET /admin/v1/api-keys/<id>` returns the current usage via the
underlying `QuotaCounter::current_usage` API. The
`/admin/v1/stats/by-api-key` aggregate endpoint reports
cumulative counts for the dashboard.

## Implementation

The trait + the two backend implementations live in
[`crates/core/src/quota.rs`](../../crates/core/src/quota.rs).
The `QuotaCounter` is an async trait with two methods:

* `check_and_consume(key_id, spec, tokens) -> QuotaDecision`
* `current_usage(key_id) -> HashMap<QuotaKind, u64>`

The `QuotaDecision` enum is `Allow { remaining }` or
`Deny { retry_after, limit, kind }`.
