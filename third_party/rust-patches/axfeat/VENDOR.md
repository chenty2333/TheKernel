# Vendored source record: `axfeat`

## Immutable published baseline

- Registry package: `axfeat` `0.3.0-preview.2`
- crates.io archive: `axfeat-0.3.0-preview.2.crate`
- crates.io archive SHA-256: `0abc9f576faa89f8ffb6a56e521fcf4eccf47fc4321afa93a3c5b769d4fbaafa`
- Archive URL: <https://static.crates.io/crates/axfeat/axfeat-0.3.0-preview.2.crate>
- Repository declared by the package: <https://github.com/arceos-org/arceos/tree/main/api/axfeat>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `6c6765c05df0550e31edb0ca82d468199f108b3f` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `45f39b31ed0127d6490156cf0416a84085406d71b2ae8ad58a9280bb80e48d5c`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `2aee666` aligned architecture/device feature bundles.
- `96df7d9` wired the shared/async block mechanism without placing benchmark policy in the feature crate.
- Maintained delta: Cargo feature composition only; runtime and Linux-visible policy remain in their owning layers.

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
