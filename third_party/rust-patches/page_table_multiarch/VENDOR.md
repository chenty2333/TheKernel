# Vendored source record: `page_table_multiarch`

## Immutable published baseline

- Registry package: `page_table_multiarch` `0.6.1`
- crates.io archive: `page_table_multiarch-0.6.1.crate`
- crates.io archive SHA-256: `42c5b75d5d9bdbee44c827b0dd2766fa3d478a76b9c6735419228089d1b24536`
- Archive URL: <https://static.crates.io/crates/page_table_multiarch/page_table_multiarch-0.6.1.crate>
- Repository declared by the package: <https://github.com/arceos-org/page_table_multiarch>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `4f1fe0f9c62ec4a537af0a4acce0c863acc45242` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `a026f98bb2f3916edb316cec6a7018cbbc011a4cb560b8b63eceb22e2afc1f2b`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

All published test paths are present but adapted to the maintained fork: `tests/alloc_tests.rs`. The immutable originals remain recoverable from the verified archive.

## TheKernel patch ledger

- `aa610a0` skipped sparse COW leaf scans.
- `a749c80` batched COW unmaps through page-table drain.
- `b5f7a54` and `a4ec274` avoided invalid/redundant TLB work for inactive or freshly mapped tables.
- Maintained delta: multi-architecture page-table traversal, drain, and TLB primitives with focused allocation tests.

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
