# Vendored source record: `starry-signal`

## Immutable published baseline

- Registry package: `starry-signal` `0.3.0`
- crates.io archive SHA-256:
  `f72adf2bff529986c36c6b3920332afbefd0f6f6178855347f1bac15f4304d37`
- Repository declared by the package:
  <https://github.com/Starry-OS/starry-signal>
- Source commit recorded by Cargo:
  `0a39846c582895555816145f47f82ceb0c89aa62`
- Cargo VCS dirty flag: absent; the recorded commit is the release source
  identity.
- Authors: Mivik `<mivikq@gmail.com>` and 朝倉水希
  `<asakuramizu111@gmail.com>`
- License: `Apache-2.0`; `LICENSE` matches the registry archive.
- Original manifest: `Cargo.toml.orig`
- Cargo source record: `.cargo_vcs_info.json`

The checksum above was verified against the downloaded crates.io archive.

## TheKernel patch lineage

- `53d3c0acc40fdd7cab7df3c0cec7662b7466047a` imported the crate and
  centralized syscall-restart enrollment.
- `abcba9ccef47cf4cd9b9083a4ee4c5ceedfbb6b1` added compatibility accessors.
- `f8b882702c158444dc4e02a1112ffd84b769b73d` and
  `becd8e37f77a6e9768f77b724ab1542f2a6d374c` refined blocked/ignored signal
  behavior and ABI fields.
- `d38fb1b96d108942e8c52218a7d934db1a24fe72` added fallible endpoint
  construction, publication rollback, and delivered-signal restart metadata.

Against the extracted registry source, the current source patch is 141
insertions and 20 deletions across six files; three integration-test files
carry 45 insertions and 14 deletions. The maintained differences include:

- restartability metadata returned with delivered signals;
- fallible thread-signal endpoint allocation and explicit registration tokens;
- rollback when an owning thread fails before publication;
- blocked/ignored and realtime-pending accounting helpers;
- ABI flag and `SignalInfo` compatibility updates.

All six manifest-declared integration tests, the original manifest, Cargo VCS
record, and Apache-2.0 license remain present. The test adapter avoids a
`MaybeUninit` convenience API unavailable on TheKernel's pinned nightly, and
the blocked-plus-ignored test reflects Linux behavior: generation is queued
while blocked even though it would be discarded while unblocked.

## Known boundary

`ThreadSignalManager::restore` still directly dereferences a user-controlled
signal-frame pointer. This source record does not certify that path as safe;
the planned sigreturn/user-copy slice must replace it with checked `VmIo`
copy-in before this crate can be treated as a hardened extraction candidate.

When rebasing, use the verified crate archive as the pristine baseline and
preserve the explicit registration/rollback contract. Do not infer safety or
Linux semantic completeness merely from the package name.
