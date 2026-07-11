# Vendored source record: `axsched`

## Immutable published baseline

- Registry package: `axsched` `0.3.1`
- crates.io archive: `axsched-0.3.1.crate`
- crates.io archive SHA-256: `cad6b7b0b8d9ad1d52a834d8b7721114413da8cf3430af928b1c8651f911287a`
- Archive URL: <https://static.crates.io/crates/axsched/axsched-0.3.1.crate>
- Repository declared by the package: <https://github.com/arceos-org/axsched>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `4d86c55dce4c87dde52792515ce188081323ac07` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `374d6e997e4cf9db00d57c89af6c9b8b6cd3c8f31af27b5d3b95f23ad9a0ca89`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `07de702` introduced runtime CFS class/state configuration.
- `e5ee2f9` added FIFO/RR policy mechanics and priority handling.
- `96df7d9` and `d38fb1b` hardened cross-runqueue identity, enqueue reasons, and lifecycle behavior.
- `909591e` removed the false SCHED_DEADLINE capability from the generic scheduler surface.
- Maintained delta: fair/FIFO/RR scheduling mechanics and tests; unsupported deadline scheduling is rejected honestly.

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
