# Vendored source record: `virtio-drivers`

## Immutable published baseline

- Registry package: `virtio-drivers` `0.7.5`
- crates.io archive: `virtio-drivers-0.7.5.crate`
- crates.io archive SHA-256: `d6a39747311dabb3d37807037ed1c3c38d39f99198d091b5b79ecd5c8d82f799`
- Archive URL: <https://static.crates.io/crates/virtio-drivers/virtio-drivers-0.7.5.crate>
- Repository declared by the package: <https://github.com/rcore-os/virtio-drivers>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `a9487f2c69826b4caf9830e6d5588f28c27dc24d` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `a3fa5b3dedc192d52db972e36508d86f1db871ee48e3037a6921764d7e998f9c`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `MIT`; the archive license files are retained as `LICENSE`.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `2aee666` repaired LoongArch transport, queue, and device integration.
- `c52dc6f` added async block ownership/completion and queue instrumentation.
- `96df7d9` hardened descriptor admission, fallible allocation, interrupt cleanup, and device teardown.
- Maintained delta: generic VirtIO queue/device mechanics, entropy/sound/vsock additions, bounded diagnostics, and owned request lifecycles.

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
