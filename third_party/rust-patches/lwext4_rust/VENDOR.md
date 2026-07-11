# Vendored source record: `lwext4_rust`

## Immutable published baseline

- Registry package: `lwext4_rust` `0.2.0`
- crates.io archive: `lwext4_rust-0.2.0.crate`
- crates.io archive SHA-256: `b8518a02caf4803c6135450571d6af9bdb382880e5617adbd88c0e0eba237cbb`
- Archive URL: <https://static.crates.io/crates/lwext4_rust/lwext4_rust-0.2.0.crate>
- Repository declared by the package: <https://github.com/Starry-OS/lwext4_rust>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `0422ace7c91ca6639beec21c67819db8a14a324b` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `6fd097cbf0179803c0ff7e2a709ba31cefc0f847bdec09f874d0699169d39303`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-2.0`; the archive license files are retained as `LICENSE.GPLv2`.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `1b61d9c`, `4968cf8`, and `e80699a` repaired reproducible host/cross build integration.
- `05f30cc`, `3c8c09a`, and `24a36ad` hardened ext4 rename, dnotify, ioctl, and filesystem semantics.
- `c52dc6f` introduced mapped/async block integration with synchronous fallback.
- `96df7d9` and `d38fb1b` tightened cache freshness, fallible allocation, flush/error propagation, and resource bounds.
- Maintained delta: lwext4 C bindings, ext4 inode/file adapters, mapped-run/cache mechanisms, flush/error behavior, and build tooling.

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
