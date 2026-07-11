# Vendored source record: `scope-local`

## Immutable published baseline

- Registry package: `scope-local` `0.1.2`
- crates.io archive: `scope-local-0.1.2.crate`
- crates.io archive SHA-256: `c80f3dd0611957c9384d8e5b076236a265e873b41dcae7ccc5d1ba4fe58e32ae`
- Archive URL: <https://static.crates.io/crates/scope-local/scope-local-0.1.2.crate>
- Repository declared by the package: <https://github.com/Starry-OS/scope-local>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `96d50f87014093de442b41a4dd7df1dd5fd637ce` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `81464d78a6d041ed597bba940b3c093406b8bfbdf75ff9424b3173c751e04c88`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `MIT OR Apache-2.0`; the archive license files are retained as `LICENSE-APACHE-2.0`, `LICENSE-MIT`.

## Upstream tests

All published test paths are present but adapted to the maintained fork: `tests/scope_local.rs`. The immutable originals remain recoverable from the verified archive.

## TheKernel patch ledger

- `d38fb1b` replaced infallible scope/item allocation paths with explicit `try_new` admission and rollback.
- Maintained delta: fallible scope-local object construction and allocation-safe registry publication.

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
