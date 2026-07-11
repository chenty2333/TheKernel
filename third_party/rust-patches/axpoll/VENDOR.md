# Vendored source record: `axpoll`

## Immutable published baseline

- Registry package: `axpoll` `0.1.2`
- crates.io archive: `axpoll-0.1.2.crate`
- crates.io archive SHA-256: `36b92f85c6903350f5146216ccb7d7a7e7b4dbd6f5927a1279db03ba52a53ae7`
- Archive URL: <https://static.crates.io/crates/axpoll/axpoll-0.1.2.crate>
- Repository declared by the package: <https://github.com/Starry-OS/axpoll>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `86f20f6bc1b470fc21894721e72b721f49aa20b7` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `3debbfb6c8878ea36d06d7e026e04e2828c73e58969992e4a4422852fb21f019`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

All published test paths are present but adapted to the maintained fork: `tests/async.rs`, `tests/tests.rs`. The immutable originals remain recoverable from the verified archive.

## TheKernel patch ledger

- `d38fb1b` replaced an allocation-growing waker vector with a fixed 64-entry registry and moved clone/drop/wake work outside the IRQ-safe lock.
- Maintained delta: bounded readiness registration, duplicate suppression, deterministic replacement wakeup, and deferred waker destruction.

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
