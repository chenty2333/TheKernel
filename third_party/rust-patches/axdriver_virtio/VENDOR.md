# Vendored source record: `axdriver_virtio`

## Immutable published baseline

- Registry package: `axdriver_virtio` `0.1.4-preview.3`
- crates.io archive: `axdriver_virtio-0.1.4-preview.3.crate`
- crates.io archive SHA-256: `da6c36cc900745f3bab9de0dd8d5a2d5ac720a253937b3c5f3ab81bbb9c9e139`
- Archive URL: <https://static.crates.io/crates/axdriver_virtio/axdriver_virtio-0.1.4-preview.3.crate>
- Repository declared by the package: <https://github.com/arceos-org/axdriver_crates>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records context commit `eea5576b64242a3d599600786632513ee847acd2` with `dirty=true`; that commit is not an exact tree identity.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `919f5de4458eca5c0da0a4c533812ce2ba870cf41dfb1742f4202e4131b1764d`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `2aee666` repaired VirtIO device construction and LoongArch integration.
- `c52dc6f` added async block submission/completion and interrupt-backed drain support.
- `96df7d9` tightened ownership, queue admission, fallible allocation, and cleanup.
- Maintained delta: generic VirtIO block/net/input/gpu/vsock adapters, explicit IRQ propagation, and bounded queue lifecycle.

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
