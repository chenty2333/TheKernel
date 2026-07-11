# Vendored source record: `kernel-elf-parser`

## Immutable published baseline

- Registry package: `kernel-elf-parser` `0.3.4`
- crates.io archive: `kernel-elf-parser-0.3.4.crate`
- crates.io archive SHA-256: `3f1495ab3ea0a7cee31d14901a858a732e282c139b3d17d3f935aebeeefcc34a`
- Archive URL: <https://static.crates.io/crates/kernel-elf-parser/kernel-elf-parser-0.3.4.crate>
- Repository declared by the package: <https://github.com/Starry-OS/kernel-elf-parser>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records context commit `fdcce740d718031224bdb8a77ff593b38e03cea7` with `dirty=true`; that commit is not an exact tree identity.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `37a72b550da7f51ff46035d3d88b81667b469a0e98bb5de20e1bb5a967719840`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A Git commit marked as
context must never be substituted for the dirty published tree.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the published archive contains no license text. This is recorded as a distribution anomaly; no license file has been synthesized.

## Upstream tests

All published test paths are present but adapted to the maintained fork: `tests/elf_static`, `tests/ld-linux-x86-64.so.2`, `tests/test_dynamic.rs`, `tests/test_static.rs`. The immutable originals remain recoverable from the verified archive.

## TheKernel patch ledger

- `61becfd` aligned auxiliary-vector and user-stack layout for static glibc.
- `bafce29` repaired loader behavior exposed by RISC-V LTP.
- `96df7d9` added fallible/checked loader and stack construction boundaries.
- Maintained delta: ELF metadata parsing, checked user-stack construction, and static/dynamic loader test adaptations.

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
