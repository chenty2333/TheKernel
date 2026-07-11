# Vendored source record: `starry-process`

## Immutable published baseline

- Registry package: `starry-process` `0.2.0`
- crates.io archive SHA-256:
  `88fa031a95c25b7bcfe8883f9f53238c9053a2a89f790bb1a7c35d080c6d3b65`
- Repository declared by the package:
  <https://github.com/Starry-OS/starry-process>
- Source commit recorded by Cargo:
  `ab4fd0e8f91587ca18d3d2ab3e79dcf88b4200a8`
- Cargo VCS dirty flag: absent; the recorded commit is the release source
  identity.
- Authors: 朝倉水希 `<asakuramizu111@gmail.com>`
- Published license expression: `MIT OR Apache-2.0`
- Original manifest: `Cargo.toml.orig`
- Cargo source record: `.cargo_vcs_info.json`

The checksum above was verified against the downloaded crates.io archive. Keep
the archive checksum as the immutable comparison baseline; do not infer a new
upstream identity from TheKernel's Git history.

## License anomaly

The `0.2.0` archive and its exact source commit contain no license text even
though the manifest declares `MIT OR Apache-2.0`. The `LICENSE` file in this
directory was recovered from upstream commit
`ad905ce0f555026609fd874c6ef58fca6d510162`, the immediate child of the release
commit whose purpose was to add Apache-2.0 licensing and change later upstream
metadata to `Apache-2.0`.

This records the available authoritative text without rewriting the historical
`0.2.0` license expression. No MIT license file was present in the release
archive or release commit, so none has been synthesized here.

## TheKernel patch lineage

- `3fe155bbdd3a68f778a76685f2ab3870cc82de90` introduced the local snapshot with
  durable child accounting and wait snapshots.
- `620612530e9989ad4a721f2f0d2ac9a8812e4f7e` added child-subreaper behavior.
- `f8b882702c158444dc4e02a1112ffd84b769b73d` and
  `73c25bf422fec71f2cae6ca85c908612103c8d5e` extended wait and lifecycle
  semantics.
- `d38fb1b96d108942e8c52218a7d934db1a24fe72` replaced weak-map/global scans
  with bounded intrusive registries and fallible admission.

Against the extracted registry source, the current `src/` patch is 844
insertions and 209 deletions across four files. Its main contracts are:

- fallible process and thread prepare/commit admission with rollback;
- a bounded intrusive process/TID registry and allocation-free iteration;
- fallible process, group, session, child, and thread snapshots;
- durable zombie/usage snapshots, subreaper reparenting, and explicit reap;
- explicit membership capacity and OOM errors.

The current normalized manifest replaces `weak-map` with
`intrusive-collections` plus `spin`. The published integration-test targets
have been restored and adapted to exercise the current fallible/admission API;
`Cargo.toml.orig` remains an unmodified record of the published manifest.

When rebasing, compare source against the verified crate archive first, then
reapply the contracts above. A same-named Starry-OS branch is not a substitute
for the archived release baseline.
