# Vendored source record: `starry-fatfs`

## Immutable published baseline

- Registry package: `starry-fatfs` `0.4.1-preview.2`
- crates.io archive SHA-256:
  `3d994a3a25a5cc4a895b200bd00d8e757515f21f9c21647bd4e9a8e572a326f1`
- Repository declared by the package:
  <https://github.com/Starry-OS/rust-fatfs>
- Cargo VCS context commit:
  `2685439e679cc832a67fd21340258b7c018c0f33`
- Cargo VCS dirty flag: `true`. The commit identifies repository context, not
  the exact published tree; only the archive checksum is an exact baseline.
- Authors: Rafał Harabień `<rafalh92@outlook.com>` and Yu Chen
  `<yuchen@tsinghua.edu.cn>`
- License: `MIT`; `LICENSE.txt` matches the registry archive and retains
  Rafał Harabień's copyright notice.
- Original manifest: `Cargo.toml.orig`
- Cargo source record: `.cargo_vcs_info.json`

The checksum above was verified against the downloaded crates.io archive. This
package is itself a Starry-OS fork of Rafał Harabień's `rust-fatfs`; both the
original author and the fork lineage must remain visible.

## TheKernel patch lineage

- `96df7d9b5a2bb86e83d2f92b9d9a31b279407b03` introduced the local published
  snapshot plus explicit flush coverage and initial rename/error hardening.
- `d38fb1b96d108942e8c52218a7d934db1a24fe72` added bounded namespace
  transactions, rollback/poison handling, and fallible metadata allocation.

Against the verified archive, the current `src/` patch is 488 insertions and
148 deletions across five files. The maintained differences include:

- fixed-capacity directory-entry snapshots for rename transactions;
- replacement rename with rollback and a poisoned-filesystem fail-stop state;
- parent (`..`) update rollback for moved directories;
- fallible short-name/LFN metadata allocation and `NotEnoughMemory` mapping;
- explicit file/filesystem flush propagation and runtime poison checks.

The current normalized manifest adds the local `tests/flush.rs` target. The
format/rename tests add 318 lines relative to the published archive and cover
replacement, rollback, error injection, and capacity boundaries.

## Known boundary

These in-memory rollback rules are not journal-level power-loss atomicity, and
open-handle deferred unlink/rename semantics remain outside this fork's
current contract. Do not describe this patch as a crash-consistent journal.

When rebasing, compare against the verified archive checksum rather than the
dirty VCS context commit, retain the MIT notice and upstream authors, and rerun
format/rename/flush tests with injected I/O failures.
