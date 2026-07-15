# Vendored source record: `axfs-ng-vfs`

## Immutable published baseline

- Registry package: `axfs-ng-vfs` `0.1.1`
- crates.io archive: `axfs-ng-vfs-0.1.1.crate`
- crates.io archive SHA-256: `75b3fc5c71051e9ae0b29700aa6eb676b7dadb91be3415d2b374cc8d2a2d37c6`
- Archive URL: <https://static.crates.io/crates/axfs-ng-vfs/axfs-ng-vfs-0.1.1.crate>
- Repository declared by the package: <https://github.com/Starry-OS/axfs-ng-vfs>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `0c1be8be0e4b43f2a8374e180517b2647065fd30` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `57879bed5f507a029155fa0b1b2928ef68ff35f046ecef8cb0b3d96b50225d7d`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `MIT OR Apache-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `3c8c09a` and `24a36ad` extended rename/dnotify/ioctl/stat metadata mechanisms.
- `4b2c772`, `5a3ce2f`, and `854fee0` repaired inode sharing, ctime, birth-time, and DIO metadata.
- `f66f1ff` removed mount ownership cycles and defined detach/flush lifecycle.
- `ba0a4aa` added generic pathname-walk admission rather than syscall-local prefix checks.
- `d38fb1b` hardened fallible allocation and object lifetime boundaries.
- The current path-snapshot slice makes component retention, capacity
  arithmetic, and final `PathBuf` construction fallible before Linux-ABI or
  procfs consumers render the result.
- The current file-contract slice preserves a completed vectored-I/O prefix
  across a later element error, exposes whether failed truncate is atomic, and
  separates stream, positioned-read, positioned-write, and seek capabilities.
- Maintained delta: generic dentries, mounts, pathwalk hooks, metadata, and node contracts; Linux DAC decisions are injected from the ABI layer.

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
