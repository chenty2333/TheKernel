# Vendored source record: `axfs-ng`

## Immutable published baseline

- Registry package: `axfs-ng` `0.3.0-preview.2`
- crates.io archive: `axfs-ng-0.3.0-preview.2.crate`
- crates.io archive SHA-256: `cda01a3d5334aef9764462e9e06639de220490f897a85749444bac4517d6edd4`
- Archive URL: <https://static.crates.io/crates/axfs-ng/axfs-ng-0.3.0-preview.2.crate>
- The published manifest declares no repository URL.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `6c6765c05df0550e31edb0ca82d468199f108b3f` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `9b07fbf8844c5029679a8c1aad36ece95aeb7782a57928acce6bdf2d155830d8`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `5f56231` normalized root/path behavior in the imported filesystem mechanism.
- `af59bd2`, `94d35dd`, and `5017136` added tracked page-cache writeback, sync, coherence, clean-page LRU, and bounded budget work.
- `887e9ce`, `a6899bb`, and `bbe3747` repaired truncate, mmap/COW, dirty-state, and reclamation interactions.
- `f66f1ff`, `ba0a4aa`, and `d38fb1b` hardened mount ownership, pathname admission hooks, fallible allocation, and lifecycle boundaries.
- The current MM integration binds deferrable cache-eviction acknowledgement to
  one stable address-space owner, rolls back on foreign or incomplete
  acknowledgement, and keeps address-space-owned page fills synchronous so no
  lower async request is published while the caller owns its MM transaction.
- The current ext4 adapter admits only complete single-request async scatters
  and waits for accepted handles after releasing the lwext4 filesystem lock;
  larger scatters fall back synchronously before any request is submitted.
- Maintained delta: generic filesystem/page-cache mechanisms, tmpfs/disk adapters, coherence/writeback, resource accounting, and VFS integration.

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
