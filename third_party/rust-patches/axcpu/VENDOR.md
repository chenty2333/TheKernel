# Vendored source record: `axcpu`

## Immutable published baseline

- Registry package: `axcpu` `0.3.0-preview.8`
- crates.io archive: `axcpu-0.3.0-preview.8.crate`
- crates.io archive SHA-256: `361edfc761188b19fb3d906b0b155942a6290068ee88d42f3b1f0ce31dcd099e`
- Archive URL: <https://static.crates.io/crates/axcpu/axcpu-0.3.0-preview.8.crate>
- Repository declared by the package: <https://github.com/arceos-org/axcpu/tree/dev>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `0edeaa68c89b5410e17ccae1b03db9d56311d0a5` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `cfee4f86d0f15ab5f35f68a2624fae8a024efe7d5e48a169966e3198bb110554`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- The maintained source is narrowed to x86_64; the retained architecture
  implementation includes context switching, user-copy assembly, trap setup,
  and instruction-cache publication.
- x86_64 address-space switching adds bounded PCID/INVPCID capability probing,
  per-CPU bootstrap accounting, generation-aware identities, and defensive
  CR3 fallback behavior. Remote execution remains the responsibility of the
  higher-level maintenance broker.
- Adds a fixed-capacity `IrqBoundary` transport around the existing
  `handle_trap!(IRQ, ...)` dispatch. It reports enter/exit only; platform IRQ
  acknowledgement and scheduler policy remain in `axhal`/higher layers.
- `host-test-context` disables exception-table recovery in hosted unit tests,
  which cannot route CPU faults through the kernel trap entry or use the
  production linker script; the `target_os = "none"` kernel path is unchanged.
- Linux user-copy and MM policy remain above this crate.

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
