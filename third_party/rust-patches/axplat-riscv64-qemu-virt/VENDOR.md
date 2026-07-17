# Vendored source record: `axplat-riscv64-qemu-virt`

## Immutable published baseline

- Registry package: `axplat-riscv64-qemu-virt` `0.3.1-pre.6`
- crates.io archive: `axplat-riscv64-qemu-virt-0.3.1-pre.6.crate`
- crates.io archive SHA-256: `08f91aff22afadd24807e34fb94fe4d0d2c8c5b86fb89dd6ff87a8093f812518`
- Archive URL: <https://static.crates.io/crates/axplat-riscv64-qemu-virt/axplat-riscv64-qemu-virt-0.3.1-pre.6.crate>
- Repository declared by the package: <https://github.com/arceos-org/axplat_crates>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `811837d8c699941f43665510b6e30700faa0e633` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `888ea3900467fde34ec11096fd46a480f83cd31d5b7deb611f4f533ce4d430bc`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `e0a7cc1` aligned RISC-V runtime packaging and platform configuration.
- `5641ef3` retained console/configuration changes needed by the generic runtime path.
- TheKernel's SMP TLB integration acknowledges the current supervisor software
  interrupt before dispatch so a concurrent IPI reasserts `SSIP` instead of
  being cleared as part of the older delivery.
- Maintained delta: QEMU-virt platform configuration, console integration, and
  lossless concurrent software-interrupt dispatch.

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
