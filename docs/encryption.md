# TiyGate Encryption (Phase 4)

Provider API keys and OAuth refresh tokens are encrypted at rest
with **AES-256-GCM**. The master key is supplied via the
`TIYGATE_MASTER_KEY` env var and never leaves the process; the
ciphertext is stored in the `providers.encrypted_api_key` and
`providers.encrypted_oauth_meta` columns.

## Master key format

`TIYGATE_MASTER_KEY` accepts the following encodings:

* **Hex** — 64 lowercase characters (e.g.
  `4f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f708192a3b4c5d6e7f8`).
* **Base64** — 32 raw bytes encoded with the standard alphabet
  (padding optional, `+` and `/` allowed).

Both encodings are auto-detected. An invalid or wrong-length
value is logged at startup and the gateway runs in
"cleartext-fallback" mode (each secret is stored in cleartext and
a warning is emitted at every write).

## Key derivation

The master key is not used directly. Per-purpose subkeys are
derived via **HKDF-SHA256** with the info string
`tiygate/v1/<purpose>`. Two purposes are defined today:

| Purpose label | Used for |
| --- | --- |
| `provider-api-key` | `providers.encrypted_api_key` |
| `oauth-refresh-token` | `providers.encrypted_oauth_meta` |

The split ensures that a future rotation of one purpose does
not invalidate the other; it also means ciphertext from
`provider-api-key` cannot be decrypted as
`oauth-refresh-token` even if the master key is the same
(verified by the `keys::tests::purposes_are_isolated` test).

## Ciphertext format

`nonce (12) || ciphertext || tag (16)`, base64-encoded into a
single string. The 12-byte nonce is generated with the OS RNG
for every call; reusing a (key, nonce) pair catastrophically
breaks GCM confidentiality and integrity, so we never
re-use.

## Zeroisation

* The master key buffer is wrapped in `zeroize::Zeroizing<[u8; 32]>`
  so it is overwritten before the allocator reclaims it.
* The `KeyEncryption` `Debug` implementation emits
  `<redacted 32 bytes>` — the key never appears in log output.
* `KeyEncryption::redact(blob)` returns `[encrypted: <12 chars>…]`
  for safe display in admin GET responses.

## Threat model

* **Operator with DB access but no master key** sees only
  ciphertext. Decryption requires the master key, which is read
  once at startup and held in process memory.
* **Operator with both DB + master key** can decrypt every
  secret. This is the same trust boundary every API gateway
  has — there is no way to operate without holding the master
  key in memory.
* **Operator with process memory access** (e.g. `gdb` on a
  running binary) sees the master key. Mitigations: run the
  binary on a hardened host, use a secrets manager to inject
  the env var, and consider the
  `key_encryption::from_env` helper when you wire in a real
  KMS.

## What is *not* encrypted

* The `audit_log` table records the *action* and *target id*
  of every admin write; it does not record secrets, but the
  `details_json` may contain other operational metadata. Audit
  records are cleartext so they remain useful for forensic
  review.
* The `request_logs` table stores the request body, but only
  after the headers have been redacted (see
  [`redaction.md`](redaction.md) and the `Redactor` module).
* Caller-side API keys are not encrypted with AES-GCM; they are
  stored as **SHA-256 hashes**. The cleartext is shown to the
  admin exactly once on creation. See
  [`api-keys`](admin-api.md#api-keys) for the rationale.

## Implementation

The encryption primitives live in
[`crates/store/src/encryption.rs`](../../crates/store/src/encryption.rs).
The `keys` module in
[`crates/store/src/keys.rs`](../../crates/store/src/keys.rs)
exposes the per-purpose helpers used by the rest of the gateway.
