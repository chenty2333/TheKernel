# Vendored source record: `axdriver_block`

## Immutable published baseline

- Registry package: `axdriver_block` `0.1.4-preview.3`
- crates.io archive: `axdriver_block-0.1.4-preview.3.crate`
- crates.io archive SHA-256: `7cccf23999a9dff620ef87c08c571509d2e90cc9dc80f932381b0fd949f020f9`
- Archive URL: <https://static.crates.io/crates/axdriver_block/axdriver_block-0.1.4-preview.3.crate>
- Repository declared by the package: <https://github.com/arceos-org/axdriver_crates>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records context commit `eea5576b64242a3d599600786632513ee847acd2` with `dirty=true`; that commit is not an exact tree identity.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `bce3ac5d6904c627fe15004b031b28637c4eab38e270741bbb9777310f1231b0`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `c52dc6f` added the generic async/batch block request, segment, capability, and completion contract.
- `96df7d9` hardened queue admission, fallback, fence, flush, and IRQ-facing behavior.
- The current I/O integration requires every accepted request prefix to carry
  unique completion handles, requires an error return to leave no device access
  to caller-owned buffers, and requires `wait_async_all` to reap every handle
  even after an earlier completion-status error.
- Maintained delta: bounded request/descriptor capability reporting with an honest synchronous fallback; no Linux ABI policy belongs here.

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
