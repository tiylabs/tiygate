# OAuth Keepalive Reliability Fixes

## Scope

This change fixes two defects in the coordinated OAuth token lifecycle:

1. PostgreSQL refresh coordination must not acquire a second pooled database
   connection while holding the provider advisory-lock transaction.
2. OAuth providers that cannot pass keepalive preflight must not remain
   permanently eligible and monopolize the bounded scanner batch.

Cross-replica invalidation of process-local access-token caches is explicitly
out of scope. The Admin credential-edit closure in
`mutate_provider_credentials` is also out of scope: this fix is limited to the
request/background token refresh coordinator identified by the review. Existing
refresh lock wait durations, token endpoint timeouts, keepalive settings,
encryption purposes, and Admin API behavior remain unchanged.

## Design

### Transaction-bound PostgreSQL refresh

`OAuthTokenStore` and `DbConfigStore` will expose a narrow transaction-bound
executor containing only the operations needed by the server's token refresh
coordinator. Once `pg_try_advisory_xact_lock` succeeds, provider credential
reads, shared token state reads, successful token installation, and refresh
failure scheduling will use that executor rather than `DbPool`. Passing the
executor explicitly keeps pool-backed store calls out of the lock-held portion
of the coordinator.

The server will load and decrypt the current provider row through the active
transaction before deciding whether a shared token can be reused or an
external refresh is necessary. This preserves the existing requirement that a
credential edit completed before lock acquisition is observed by the next
refresher.

The keepalive interval is not provider-scoped and does not participate in
credential coordination. It will be read before acquiring the PostgreSQL
transaction and passed into successful token persistence, eliminating the
post-refresh settings query while the transaction owns a pool connection.

On token endpoint failure, the coordinator will persist the correctness-critical
retry deadline through the active transaction before committing and releasing
the advisory lock. The operator-facing OAuth status is then updated outside the
transaction with a compare-and-set against the encrypted metadata read by the
failed attempt. If a concurrent manual refresh or credential edit has already
changed that metadata, the stale status update is skipped. A status update
failure is logged but cannot roll back the committed retry deadline. This
prevents both connection-pool re-entry and a window in which another automatic
refresher can acquire the lock before the retry deadline is visible. SQLite
will continue to use its provider-scoped process-local mutex, while sharing the
same failure-state construction and conditional status-update semantics.

Within request-path and keepalive token refresh, no external HTTP request may
be followed by a pool-backed store call before the PostgreSQL transaction is
closed. In-memory snapshot refresh and conditional operator-status updates
occur only after the transaction has committed or rolled back.

### Keepalive preflight suppression

`try_keepalive_provider` will ensure that every failed keepalive attempt leaves
a future `next_retry_at`, including failures that occur before the token
endpoint is called, such as missing OAuth metadata, missing token endpoint
configuration, or an empty refresh token.

Preflight validation and fallback retry persistence run while holding the same
provider coordination used for token refresh. PostgreSQL reloads the provider
row through the advisory-lock transaction immediately before writing the
fallback; SQLite reloads it while holding the process-local provider mutex. If
the credential has become usable since the failing attempt began, no stale
backoff is written. Otherwise the coordinator first preserves any active retry
state already written by the normal refresh failure path, then persists a
bounded preflight retry deadline. After the row is committed,
`list_due_provider_ids` excludes the provider until the deadline passes.

The fallback uses the existing failure-count state and bounded exponential
backoff rather than a permanent suppression. Once the deadline expires the
provider becomes due again, validation is repeated against its current row,
and a corrected credential proceeds normally. A provider corrected outside the
Admin API can therefore recover automatically, while normal credential edits
and OAuth callback installation continue to reset or replace the failure row
immediately.

The scanner query and its batch limit remain unchanged. Progress is restored
because every selected failure receives scheduling state before the next scan.

## Error Handling

- Failure-state persistence errors are returned or logged without exposing
  access tokens, refresh tokens, or raw token endpoint bodies.
- PostgreSQL transaction errors release the advisory lock through rollback or
  transaction drop.
- A failure to update operator-facing OAuth status is isolated after commit and
  cannot undo the retry deadline; a compare-and-set prevents a stale failure
  from overwriting newer credential metadata.
- Successful token installation continues to atomically store the access
  token and effective rotated refresh token.
- Manual refresh continues to bypass an existing retry deadline.

## Testing

The implementation will add regression coverage for:

- transaction-aware provider and token-state operations using the existing
  store test infrastructure;
- conditional PostgreSQL coordinator tests using a pool with exactly one
  connection for successful refresh, token-endpoint failure, and preflight
  failure, proving those paths do not re-enter the pool while holding the
  advisory-lock transaction;
- keepalive preflight failure creating an active retry deadline;
- a scanner batch containing more invalid providers than its limit eventually
  allowing a valid due provider to be selected;
- expiry of the preflight retry deadline causing the provider to become due
  again and successfully recover after its configuration is corrected;
- a concurrent credential correction preventing an older preflight attempt
  from installing a stale backoff;
- existing SQLite single-flight, token rotation, timeout backoff, and
  unauthorized recovery behavior;
- conditional PostgreSQL independent-instance coordination when
  `TIYGATE_TEST_PG_URL` is configured.

Verification will run Rust formatting, focused `tiygate-store` and
`tiygate-server` OAuth tests, workspace dependency-layer checks, and the
largest practical workspace check/test command supported by the environment.
