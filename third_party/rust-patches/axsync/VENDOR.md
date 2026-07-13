# Vendored source record: `axsync`

## Immutable published baseline

- Registry package: `axsync` `0.3.0-preview.2`
- crates.io archive: `axsync-0.3.0-preview.2.crate`
- crates.io archive SHA-256: `4fb8e90184332ae787f483a256561c6c4eecc4b5b41e06d3850fbd524a8c6a98`
- Archive URL: <https://static.crates.io/crates/axsync/axsync-0.3.0-preview.2.crate>
- Repository declared by the package: <https://github.com/arceos-org/arceos/tree/main/modules/axsync>.
- Upstream tag: `not-recorded-in-published-archive`; the registry archive does not prove a tag name.
- Cargo records exact source commit `6c6765c05df0550e31edb0ca82d468199f108b3f` with `dirty=false`.
- Original published manifest: `Cargo.toml.orig` (SHA-256 `4c2cdeec36fa0f3fedfdb08b4304d3f031eae66f05d55ba434a66284d479da1b`)
- Cargo source record: `.cargo_vcs_info.json`

The archive checksum is the exact source baseline. A same-version source tree
or a later branch tip must not be substituted for it.

## License

The manifest declares `GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`, but the
published archive contains no license text. This is recorded as a distribution
anomaly; no license file has been synthesized.

## Upstream tests

The published archive has no standalone `tests/` paths. Its embedded mutex unit
test is retained and adapted for TheKernel's fallible task-runtime API.

## TheKernel patch ledger

- Maintained delta: publish waiter interest only after `Event` initialization,
  skip `Event::notify` when no waiter is interested, and use a sequentially
  consistent slow-path handshake so unlock and waiter registration cannot miss
  each other on SMP.
- Added focused tests that observe zero allocation and zero notification on an
  uncontended unlock, exercise a real blocking handoff, and stress concurrent
  lock/unlock behavior.

The exact rebase baseline is the archive checksum above; the live patch is the
diff between that archive and this directory. `PROVENANCE.toml` plus
`scripts/ci/validate_vendor_provenance.py` validates the immutable assets and
prevents an unrecorded local `[patch.crates-io]` entry.

## Rebase rule

Start from the verified registry archive, retain the original manifest, Cargo
VCS record, license status, and upstream test inventory, then reapply and test
each maintained ledger item. Preserve the waiter-before-notify ordering proof;
an optimization that reintroduces an unadvertised sleep window is not valid.
