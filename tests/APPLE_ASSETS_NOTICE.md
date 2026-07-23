# Notice: Apple-derived test fixtures

The following directories contain assets extracted from Apple software (macOS system asset catalogs and application `Assets.car` files):

- `tests/fixtures/` — individual CSI rendition blobs
- `tests/re_fixtures/` — rendition payloads plus CoreUI reference render dumps (LZFSE-wrapped to keep the repo small)
- `tests/re_catalogs/` — complete system asset catalogs
- `tests/re_refs/` — CoreUI (`cuidump`) reference render dumps, trimmed to a minimal subset the oracle tests use and LZFSE-wrapped (regenerable with the `cuidump` example against the catalogs above)

These files are the property of Apple Inc. and are **not** covered by this project's AGPL-3.0 licence. They are included solely as test fixtures for verifying this decompiler's interoperability with the undocumented `Assets.car` format — byte-exact round-trips and pixel-exact decoding against what Apple's own frameworks produce. No ownership is claimed; if you are a rights holder and want any of these files removed, open an issue.

`tests/hand_synth.rs` additionally synthesizes the exotic rendition types from scratch at test time (no committed assets), validated against Apple's `assetutil`.
