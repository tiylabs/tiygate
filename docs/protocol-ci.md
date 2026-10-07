# Protocol CI

`.github/workflows/ci.yml` runs on every pull request, pushes to `master`, merge
queue events, and manual dispatch. It has no path filters, so core, codecs,
provider profiles, server lifecycle, specifications, fixtures, lockfiles, and
workflow changes all receive the same checks. PR updates cancel superseded runs.

The reusable `checks.yml` resolves the requested ref once, then checks out that
exact commit in both test jobs:

| Job | Checks | Build requirements |
| --- | --- | --- |
| Protocol and HTTP regressions | actionlint 1.7.12, CI script failure-injection tests, committed specification digests/revision, dependency layering, all core and codec tests, all default-feature server tests | Rust 1.98.1; local synthetic HTTP upstreams; no provider credentials |
| Full workspace tests and lint | WebUI type checking/build, Rust formatting, clippy with warnings denied, `make test` with all features and `--locked` | Node 22, Rust 1.98.1, Linux Tauri libraries, WebUI assets and a compile-only sidecar placeholder |
| CI gate | Requires revision resolution and both test jobs to succeed; failed, cancelled, or skipped dependencies fail the gate | Runs even when a dependency fails |

The server job includes all `protocol_review*` targets and `wiremock_providers`,
covering native-wire contracts, HTTP byte framing, malformed or truncated streams,
late usage, and fallback. Codec tests include the 16-direction basic text matrix
and independently authored JSON/SSE assertions. This is finite coverage of the
implemented contracts, not proof of every provider, account, model, or capability.
The test jobs use `INSTA_UPDATE=no` to prevent automatic snapshot acceptance.
GitHub retains step logs; any proptest counterexamples are uploaded on failure.
There are currently no executable proptest cases or fuzz harnesses, so the
artifact step does not imply property or fuzz coverage.

`create-prerelease.yml`, `build-and-push-image.yml`, and
`build-and-push-desktop.yml` each depend on the reusable checks. They validate
`refs/tags/<release tag>` and check out its returned SHA before creating a
prerelease, pushing images, or uploading desktop assets. Manual prerelease
creation uses the requested `release_version` tag. Checks do not inherit release
or provider secrets. Adding workflow files does not configure remote branch
rules: repository administrators must select the resulting **CI gate** check as
a required status check if merge enforcement is desired.

Run the fast checks locally from the repository root:

```sh
make test-protocols
```

Full workspace checks require a built `webui/dist` and the sidecar path declared
in `src-tauri/tauri.conf.json`. CI creates a placeholder only for compilation and
tests; release packaging builds the real sidecar separately. Once prepared, run:

```sh
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
make test CARGO="cargo --locked"
```

No schedule or paid provider smoke test is configured by this change. Manual
`make check-protocol-specs` checks current official snapshots; keep such network
drift monitoring separate from the offline merge gate. Automated scheduling,
expanded property/fuzz budgets, coverage trends, and fixed-version differential
tests need their own explicit scope and executable harnesses.
