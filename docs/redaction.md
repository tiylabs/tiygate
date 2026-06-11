# TiyGate Redaction

TiyGate applies a **redaction pass** to every request envelope
and every audit detail before it lands in storage. The goal is
to make sure that *no* secret is persisted to disk, regardless of
the surrounding code path.

## What is redacted

The default redaction set (in
[`crates/core/src/redaction.rs`](../../crates/core/src/redaction.rs))
covers:

### Header names (exact match, case-insensitive)

* `authorization`
* `proxy-authorization`
* `cookie`, `set-cookie`
* `x-api-key`
* `anthropic-api-key`
* `openai-organization`, `openai-project`
* `tiygate-admin-token`

### Header name substrings (case-insensitive)

Any header name containing one of:

* `token`
* `secret`
* `password`
* `credential`

is redacted.

### JSON body keys (case-insensitive)

When the body is a JSON value, these keys are replaced with
`[REDACTED]` recursively:

* `api_key`, `apikey`
* `token`
* `access_token`, `refresh_token`
* `client_secret`
* `password`

## What is not redacted (by design)

* The HTTP body itself — only the *known credential keys* are
  scrubbed. Arbitrary fields like `messages[].content` are kept
  verbatim so the audit log and the embedding cache can be
  reproduced from the raw envelope.
* The `redacted_headers_json` column on `request_logs` is the
  *post-redaction* header set; the `raw_envelope_json` column
  carries the *pre-redaction* body. Reading these two columns
  together tells you "what the client sent, minus the secrets".

## When redaction runs

* **On the ingress hot path** — every handler builds a
  `RawEnvelope` and runs the redactor on its headers before
  adding it to the request's `PipelineContext`.
* **On the OLTP log sink** — the `OltpSink` reads the redacted
  headers from the `RequestEvent`; the `raw_envelope_json` blob
  is *not* re-redacted (it was already scrubbed at ingress).
* **On the audit log** — admin `details_json` payloads are
  written verbatim. The redaction contract is: callers must
  not pass secrets in admin write bodies. (Provider creation
  accepts an `api_key` field — the redaction pass on the
  audit-log `details_json` replaces it with `[REDACTED]` if it
  was not already encrypted-and-replaced by the handler.)

## Customising the rules

```rust
use tiygate_core::redaction::Redactor;

let r = Redactor::empty()
    .with_header_name("X-Our-Secret")
    .with_body_key("client_cert")
    .with_header_substring("pin");
```

The redactor is small and side-effect-free; tests cover the
default rules in `crates/core/src/redaction.rs` and
`crates/server/tests/phase4_smoke.rs`.

## Threat model

* An operator reading the database *cannot* recover secrets from
  `request_logs.redacted_headers_json` or
  `request_logs.raw_envelope_json` (header redaction is
  complete; body redaction covers the known credential keys).
* An operator with process memory access (or with the
  `TIYGATE_MASTER_KEY` env var) can still decrypt provider
  secrets — see [`encryption.md`](encryption.md) for the
  threat model and the AES-GCM guarantees.
* An operator with both the database *and* the master key can
  recover the cleartext provider keys. This is the same trust
  boundary as every other API gateway.
