# Local crate provenance policy

Every local path in the root `[patch.crates-io]` table is a maintained fork,
not anonymous copied source. `PROVENANCE.toml` is the machine-readable
inventory; each package directory also retains a human `VENDOR.md`, the
published `Cargo.toml.orig`, and Cargo's `.cargo_vcs_info.json`.

The crates.io archive SHA-256 is the immutable source identity. Cargo's VCS
commit is exact only when the archive records `dirty=false`. When
`dirty=true`, the commit is repository context and cannot reproduce the
published tree. crates.io archives do not record a release tag, so the registry
uses the explicit value `not-recorded-in-published-archive` rather than
guessing a tag from a version.

License status is one of:

- `archive-files`: the published archive contains license text and the fork
  retains it;
- `declared-only`: the manifest has an SPDX expression but the archive
  contains no license file; this anomaly is recorded without synthesizing text;
- `recovered-after-release`: authoritative text was recovered separately and
  `VENDOR.md` records its source without pretending it was in the archive.

Upstream tests are inventoried as absent, restored byte-for-byte, or restored
and adapted. Adapted tests retain every published path; their original bytes
remain recoverable from the verified archive.

## Validation

The offline metadata gate needs no archive cache:

```sh
python3 scripts/ci/validate_vendor_provenance.py --archive-policy skip
```

When registry archives are available, require and verify every exact baseline:

```sh
python3 scripts/ci/validate_vendor_provenance.py \
  --archive-policy require \
  --archive-dir /path/to/additional/archive/cache
```

The validator also searches `$CARGO_HOME/registry/cache/*`, the repository
`.state/` directory, and every directory in
`$THEKERNEL_VENDOR_ARCHIVE_DIR`. The per-commit gate verifies every archive
that is present and always enforces the patch/record bijection, original
manifest hashes, VCS identity, license status, upstream-test inventory, and
human patch ledger.

## Maintaining a fork

For every code or manifest change under a patched package:

1. update the package's `VENDOR.md` semantic patch ledger;
2. keep `Cargo.toml.orig` byte-identical to the published archive;
3. change only normalized `Cargo.toml` for maintained dependency/features;
4. retain or adapt every published test path and add focused tests for new
   contracts;
5. run the metadata validator, strict archive validator, and relevant crate
   tests.

A history commit ID is only a navigation hint because project history can be
rewritten. To reconstruct the exact patch, extract the recorded archive and
diff it against the maintained directory, excluding provenance assets. Never
replace the archive checksum with a branch name or a same-named later crate.
