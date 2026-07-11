# Vendored source record: `axnet-ng`

## Immutable published baseline

- Registry package: `axnet-ng` `0.3.0-preview.2`
- crates.io archive: `axnet-ng-0.3.0-preview.2.crate`
- crates.io archive SHA-256: `a3a33a22c3b07301d1cf096021a1f048c0d48b02d5fc034c237fb9968dc06da2`
- Archive URL: <https://static.crates.io/crates/axnet-ng/axnet-ng-0.3.0-preview.2.crate>
- Repository declared by the package: <https://github.com/arceos-org/arceos/tree/main/modules/axnet>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `6c6765c05df0550e31edb0ca82d468199f108b3f` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `7c21d3c1b4cbdf20ef2c4c947ff7933d2898de4d88bd59f14ad30c9f5be9c744`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `1154947` imported the original local network-module snapshot.
- `ca56928` replaced silently ignored socket-buffer options with retained state.
- `3612f70`, `44c07cb`, and `68ddc10` repaired TCP receive/close/drain behavior.
- `96df7d9` hardened fallible buffer replacement and subsystem boundaries.
- `d38fb1b` added bounded Unix/network lifecycle and resource admission work.
- Maintained delta: per-NetStack routing/namespace state, loopback and veth mechanics, bounded socket buffers, readiness/waker integration, and TCP/UDP/Unix transport fixes.

Commit IDs are navigation hints for the current rewritten history. The exact
rebase baseline is the archive checksum above; the live patch is the diff
between that archive and this directory. `PROVENANCE.toml` plus
`scripts/ci/validate_vendor_provenance.py` validates the immutable assets and
prevents an unrecorded local `[patch.crates-io]` entry.

## Rebase rule

Start from the verified registry archive, retain the original manifest, Cargo
VCS record, license status, and upstream test inventory, then reapply and test
each maintained ledger item. Do not infer API completeness from the package
name or silently drop a patch because a later upstream tree looks similar.
