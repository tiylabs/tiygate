# Protocol specification resources

This directory separates two kinds of versioned protocol resources. Neither is
loaded by the request hot path.

| Layer | Directory | Purpose |
| --- | --- | --- |
| API wire schemas | `api-wire/` | Authoritative request/response, header, and event-shape sources. Future consumers include fixture generation, compatibility checks, and offline validation. |
| Structured-output profiles | `structured-output/` | The JSON Schema dialect accepted inside each provider's output-format field. Future consumers include cross-protocol schema compatibility decisions. |

## Source policy

- Do not treat a wire API description as a structured-output compatibility
  profile. API descriptions express carrier fields; they do not fully describe
  each provider's supported JSON Schema subset or model-level limits.
- Snapshots are developer resources, not runtime dependencies. Updating them
  must not alter gateway behavior until the corresponding codec/profile change
  and tests have been reviewed.
- Only official vendor URLs are recorded here. Anthropic currently publishes an
  API reference and SDK types, but not an official OpenAPI document; its entry
  is deliberately reference-only.

## Refreshing wire snapshots

```bash
make sync-protocol-specs
make check-protocol-specs
```

`sync-protocol-specs` refreshes the OpenAI OpenAPI and Gemini Discovery
snapshots and rewrites `api-wire/lock.json` with their SHA-256 digests.
`check-protocol-specs` downloads to a temporary file and fails if either
snapshot differs from the committed lock. It never edits the working tree.

When a source changes, review the diff, update the relevant structured-output
profile if its documented semantics changed, and add or update contract tests.
