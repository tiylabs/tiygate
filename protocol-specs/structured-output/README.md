# Structured-output compatibility profiles

These profiles describe the JSON Schema dialect carried by a provider's output
format. They are intentionally independent of the API wire snapshots.

They are bootstrap inventories, not yet executable validators. A future
validator should use one shared recursive walker and these profiles as input;
unknown keywords must be rejected until explicitly classified. This prevents a
documentation refresh from silently widening cross-protocol conversions.

Each profile records only behavior verified against the linked official source.
`known_unsupported_keywords` therefore means "known unsupported", while an omitted
keyword remains unclassified rather than implicitly supported.

`cargo test -p tiygate-core --test structured_output_profiles` reads every
profile and verifies that its known supported/unsupported keywords agree with
the core target-dialect validator. Update a profile and its validator in the
same change.
