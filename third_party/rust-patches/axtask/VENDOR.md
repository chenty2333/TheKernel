# Vendored source record: `axtask`

## Immutable published baseline

- Registry package: `axtask` `0.3.0-preview.2`
- crates.io archive: `axtask-0.3.0-preview.2.crate`
- crates.io archive SHA-256: `bc45120776afddf28b19bb7aba87e379c5779cf28a8f7884943a4821caeec774`
- Archive URL: <https://static.crates.io/crates/axtask/axtask-0.3.0-preview.2.crate>
- Repository declared by the package: <https://github.com/arceos-org/arceos/tree/main/modules/axtask>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `6c6765c05df0550e31edb0ca82d468199f108b3f` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `6c89e66d9e8755e6a7d9dcbba97db45612f62f7df398e80759ec930bb6a744b9`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

The published archive contains no `tests/` files, so there are no upstream test assets to restore. Local/unit coverage is tracked as part of the maintained patch.

## TheKernel patch ledger

- `07de702` and `4338186` introduced runtime scheduler-class/state plumbing.
- `24e5768` added timer-event deadline integration.
- `1508375`, `6fd07f7`, and `4837034` repaired stack reuse ownership and replaced unsafe caching with bounded per-CPU reuse.
- `c2db061`, `96df7d9`, and `d38fb1b` bounded exited-task reclamation, wait queues, task caches, and lifecycle cleanup.
- `909591e` removed deadline-policy pretense from the task interface.
- Maintained delta: generic task/runqueue/wait/timer/reclamation mechanisms; Linux process and scheduling ABI decisions remain above this crate.

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
