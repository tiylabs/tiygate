# OAuth token lifecycle

TiyGate keeps OAuth credentials alive without a leader election service,
heartbeat rows, or durable leases. Every server instance runs the same
stateless scanner. Coordination is scoped to one provider credential and is
released automatically when the refresh transaction or process ends.

## Stored state

- Refresh tokens remain in `providers.encrypted_oauth_meta` for compatibility
  with existing installations and provider snapshots.
- Access tokens and refresh scheduling state live in `oauth_access_tokens`.
- Both token classes are encrypted with `TIYGATE_MASTER_KEY`, using separate
  HKDF purposes (`oauth-refresh-token` and `oauth-access-token`).
- A successful refresh writes the access token and the effective refresh token
  in one database transaction. If the authorization server omits a new refresh
  token, the previous refresh token is retained.
- `credential_version` increases on every successful token installation. It is
  diagnostic state; correctness does not depend on polling it in the request
  fast path.

Token values are never returned by the Admin API and must not be included in
logs or telemetry.

## Request path

1. A valid process-local access token is applied without database I/O.
2. On a cache miss or near-expiry, the instance reads the shared access-token
   row. A usable token written by another instance is copied into the local
   cache.
3. Only when the database also lacks a usable token does the instance acquire
   the provider refresh lock and check the row again.
4. The lock owner calls the authorization server and atomically persists the
   new access token plus any rotated refresh token.
5. Other instances reuse the committed access token instead of issuing another
   refresh grant.

Editing a provider's authentication mode, vendor, OAuth endpoint configuration,
or explicit OAuth metadata clears its shared and local access-token state while
holding the same provider lock. Explicitly submitted OAuth metadata is restored
inside that critical section so an already-running refresh cannot overwrite the
new credential.

An upstream `401 Unauthorized` is handled once at the common fallback layer for
all protocols. Before refreshing, TiyGate checks whether the local cache or
shared database already contains an access token different from the rejected
one. The original upstream request is retried once with the resulting token.

## Background keepalive

Every instance scans due OAuth providers. A scan does not elect a permanent
leader:

- PostgreSQL workers use `pg_try_advisory_xact_lock` and immediately skip a
  provider that another instance is refreshing.
- SQLite uses a provider-scoped process-local mutex shared by the request path,
  Admin operations, and the worker. SQLite is supported as a single-instance
  backend and never executes PostgreSQL-specific SQL.

Transient failures use bounded exponential retry backoff. Credential rejection
such as `invalid_grant` marks the provider invalid and suppresses automatic
retries until the operator reconnects or manually refreshes it.
Token-endpoint refresh requests have a 30-second total timeout so an
unresponsive authorization server cannot hold a provider lock or worker slot
indefinitely. Request-path refreshes honor the same persisted retry deadline;
the Admin manual-refresh action deliberately bypasses it.

The following settings are seeded into the `settings` table and hot-reloaded:

| Setting | Default | Meaning |
| --- | ---: | --- |
| `gateway.oauth.keepalive.enabled` | `true` | Enable background keepalive |
| `gateway.oauth.keepalive.scan_interval_secs` | `60` | Delay between scans |
| `gateway.oauth.keepalive.interval_secs` | `604800` | Refresh each healthy credential every seven days |
| `gateway.oauth.keepalive.concurrency` | `4` | Maximum refresh tasks per instance |

## PostgreSQL compatibility

Advisory locks are a standard PostgreSQL capability and the transaction-level
functions used here are available in every currently supported PostgreSQL
release. TiyGate derives a stable signed 64-bit lock key from the provider ID.
The lock is bound to the refresh transaction, so commit, rollback, connection
loss, process termination, restart, and autoscaling cannot leave an orphaned
lock row.

## Operational limits

OAuth refresh-token rotation cannot be made fully atomic with an external
authorization server: a process can receive a rotated refresh token and fail
before committing it to the database. Database coordination eliminates
concurrent consumption inside one TiyGate deployment, but it cannot close that
external side-effect window. Providers that revoke the old token immediately
may require an interactive reconnect after such a crash.

For PostgreSQL integration testing, set `TIYGATE_TEST_PG_URL` to an isolated
test database and run:

```bash
cargo test -p tiygate-server postgres_independent_instances_share_one_refresh_grant --all-features
```

Without that variable the conditional PostgreSQL test compiles and exits
without connecting. SQLite concurrency and token-rotation tests always run.
