# Vendored source record: `memory_set`

## Immutable published baseline

- Registry package: `memory_set` `0.4.1`
- crates.io archive: `memory_set-0.4.1.crate`
- crates.io archive SHA-256: `50a49ecd4114cf87f7e442ec5dd03bd590e7094541f987057310dbb32a6341ad`
- Archive URL: <https://static.crates.io/crates/memory_set/memory_set-0.4.1.crate>
- Repository declared by the package: <https://github.com/arceos-org/axmm_crates>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `8d46505d0167e28898de9fbcab3be617cec11083` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `feb3a7252558395be32ef9275ffddb7b77026447bfdb09ad6831c5eed0d7e581`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `80620f4` coalesced adjacent anonymous VM areas and reduced fault churn.
- `b6c0b4c` added append-biased kernel-area placement.
- `b4cdb5a` completed memory-backed file mapping behavior needed by memfd.
- Maintained delta: generic memory-area placement/coalescing and backend mechanics; Linux mmap/COW policy remains above this crate.

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
