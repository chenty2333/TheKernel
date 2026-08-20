# Build and test policy

TheKernel has three verification tiers. The tiers are intentionally separate so a pull request does not rebuild root filesystems, launch QEMU, upload receipts, or rerun the same Rust tests through several wrappers.

## Checkout layout

The root `Cargo.toml` consumes three maintained sibling repositories through relative paths. Local development and CI therefore use this layout:

```text
parent/
  TheKernel/
  vISA/
  thekernel-ax/
  thekernel-linux-abi/
```

Run `./scripts/ci.sh layout` to validate it. The GitHub workflow checks out exact sibling commits so a result is tied to one integration set rather than whichever `main` commits happen to exist when a runner starts.

GitHub jobs execute in the maintained development image selected by the repository variable `THEKERNEL_DEV_IMAGE`, or by default `ghcr.io/<owner>/thekernel-dev:nightly`. The existing `Publish Dev Image` workflow must have published that tag before the first test run; changing the image is an environment change, not a test-script change.

## Tier 1: pull-request quality

```bash
./scripts/ci.sh quick
```

This gate runs whitespace checks, `rustfmt`, vendor provenance checks, Python build-tool and differential-tool tests, the two local adapter crates, one host kernel check, one complete host kernel test run, and host-profile Clippy. It does not rerun the full kernel test binary under dozens of filters or enforce test-count floors.

The maintained sibling repositories own their unit, MSRV, and packaging tests. TheKernel builds their pinned revisions as dependencies and tests the integration boundary; it does not duplicate their complete suites.

## Tier 2: patched-source contracts and product profiles

```bash
./scripts/ci.sh patches
./scripts/ci.sh kernel
```

`patches` retains the real unit/contract tests for patched or local crates that are outside the root Cargo workspace. `kernel` checks the non-default diagnostic and test-control profiles, builds the actual x86_64 q35/UEFI kernel, and runs the architecture Clippy profile. `./scripts/ci.sh all` runs tiers 1 and 2 and is the GitHub pull-request gate.

## Tier 3: semantic and targeted tests

The system test and semantic smokes remain explicit because they build fixtures and launch QEMU:

```bash
./scripts/ci.sh system
./scripts/ci.sh smoke lwext4-io-boost --arch x86
```

Host Linux differential oracles remain available without being multiplied into one artifact-uploading job per case:

```bash
./scripts/ci.sh differential futex
./scripts/ci.sh differential epoll
```

The default CI gate reports command output directly. Checksums, copied source trees, sealed receipts, and tests of the gate's own evidence format are not acceptance criteria. Evidence-producing performance or research runs may keep their own receipts when the receipt is part of the experiment rather than a substitute for a test result.
