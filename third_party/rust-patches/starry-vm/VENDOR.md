# Vendored source record: `starry-vm`

## Immutable published baseline

- Registry package: `starry-vm` `0.3.0`
- crates.io archive SHA-256:
  `3596dd192ef0b8c6790c5d3d1c69746c3f94afef46907a5314f1a478917daf53`
- Repository declared by the package: <https://github.com/Starry-OS/starry-vm>
- Source commit recorded by Cargo:
  `13a9296f82ce2d0fd1143cbabca3598948bfffd9`
- Cargo VCS dirty flag: absent; the recorded commit is the release source
  identity.
- Authors: 朝倉水希 `<asakuramizu111@gmail.com>` and Mivik
  `<mivikq@gmail.com>`
- License: `Apache-2.0`; `LICENSE` matches the registry archive.
- Original manifest: `Cargo.toml.orig`
- Cargo source record: `.cargo_vcs_info.json`

The checksum above was verified against the downloaded crates.io archive.

## TheKernel patch lineage

- `9d4a3351c25dc92f0b03969b1f375d3f476bf47d` imported the published crate for
  the pinned toolchain.
- `aa98717bf6232df2ad584475d962e442f3ad2427` removed registry-only metadata and
  the upstream integration test from the local tree.
- `d38fb1b96d108942e8c52218a7d934db1a24fe72` added bounded, fallible owned
  user-memory snapshots and checked address arithmetic.

Against the extracted registry source, the maintained patch across
`src/alloc.rs` and `src/lib.rs`:

- uses `try_reserve` and reports `VmError::NoMemory`;
- rejects user-pointer arithmetic overflow;
- bounds NUL-terminated snapshots to 128 KiB and reports `VmError::TooLong`;
- lets callers select a smaller scan budget which includes the terminating
  zero element, without weakening the crate-wide ceiling;
- keeps user-memory reads behind the `VmIo` adapter.

The manifest-declared `tests/test.rs` and original manifest have been restored
from the verified archive. Two test-only `MaybeUninit` convenience calls were
rewritten into equivalent operations supported by TheKernel's pinned
nightly-2025-05-20 toolchain. Registry marker files and upstream repository CI
are not part of the maintained fork contract.

When rebasing, compare against the archive checksum and recorded source commit,
then reapply the bounded snapshot and error contracts rather than copying only
the current filenames.
