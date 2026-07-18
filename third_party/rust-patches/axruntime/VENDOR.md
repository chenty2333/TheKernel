# Vendored source record: `axruntime`

## Immutable published baseline

- Registry package: `axruntime` `0.3.0-preview.2`
- crates.io archive: `axruntime-0.3.0-preview.2.crate`
- crates.io archive SHA-256: `ce5c658dc9a0e283dafb99317ade2bb191d3881b2133ac58da251d03f589f83f`
- Archive URL: <https://static.crates.io/crates/axruntime/axruntime-0.3.0-preview.2.crate>
- Repository declared by the package: <https://github.com/arceos-org/arceos/tree/main/modules/axruntime>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `6c6765c05df0550e31edb0ca82d468199f108b3f` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `b4f4815293d19414b7876ca74d9911dafb67d97c9a6df1d9a18b91e33c3d0376`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `24e5768` added early monotonic timer deadline programming.
- `2aee666` stabilized architecture/runtime boot integration.
- `96df7d9` hardened timer rearming and task-event ownership.
- Maintained delta: per-CPU periodic/early deadline selection, generic runtime
  initialization, and the consumer-side provider for the explicit outermost
  IRQ-exit scheduler boundary; Linux timer ABI policy remains above this crate.
- The optional `ipi` feature initializes the maintained typed `axhal` broker
  with the immutable runtime CPU topology and implies IRQ support. It no longer
  links the upstream allocation-backed `axipi` callback queue; a future
  call-function consumer must satisfy an explicit bounded-work contract.
- Timer IRQ registration is fail-fast rather than allowing boot to continue
  after a handler-slot conflict.

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
