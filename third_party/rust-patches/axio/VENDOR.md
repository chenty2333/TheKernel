# Vendored source record: `axio`

## Immutable published baseline

- Registry package: `axio` `0.3.0-pre.1`
- crates.io archive: `axio-0.3.0-pre.1.crate`
- crates.io archive SHA-256: `f6ce41624ae4e7ef942ebe3ac3aa3ce5d64340e8f23fb29bbd0007e9765544b4`
- Archive URL: <https://static.crates.io/crates/axio/axio-0.3.0-pre.1.crate>
- Repository declared by the package: <https://github.com/arceos-org/axio>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `9c7e15fdf9f0d7c26185c6a25044dd86811da688` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `b2a593ee89c01d23cc99aa3b901bccbc7036b7f14f56e0e47af4dd8640316e37`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

All published test paths are restored. The two inferred array lengths in
`tests/iofn.rs` remain written explicitly as 5, and the `core_io_borrowed_buf`
test annotations spell out their `u8` element type as required by the rolling
nightly API; behavior is unchanged.

## TheKernel patch ledger

- `9d4a335` replaced the unavailable integer checked-difference API while the
  source was first adapted to the nightly standard-library I/O traits.
- `aa98717` trimmed unrelated compatibility patches while retaining that equivalent checked arithmetic.
- Maintained delta: one checked signed-difference helper in `Take`; all eight
  published integration-test paths are retained with the explicit array-length
  and `BorrowedBuf`/`BorrowedCursor<u8>` test adapters described above.

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
