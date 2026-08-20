# Build and test policy

TheKernel keeps source checks, product builds, semantic boots, and research
evidence separate. GitHub Actions expresses the ordinary checks directly; no
shell command recursively invokes another acceptance gate.

## Checkout layout

The root `Cargo.toml` consumes three maintained sibling repositories through
relative paths:

```text
parent/
  TheKernel/
  vISA/
  thekernel-ax/
  thekernel-linux-abi/
```

GitHub Actions checks out exact sibling commits. A local cross-repository change
may intentionally substitute different sibling revisions, but the resulting
integration set should be recorded explicitly.

## Pull-request checks

The ordinary workflow has two visible jobs.

### Host checks and tests

The host job runs changed-line whitespace checks, `cargo fmt`, vendored
provenance validation, `make test-tools`, the two local adapter suites, one
complete host kernel test run, and direct host-profile Clippy. It does not
replay the complete kernel binary under a directory of test filters, enforce
test-count receipts, or test an acceptance script's evidence schema.

### x86_64 product configuration

The target job checks the diagnostic and I/O-control feature profiles, builds
`kernel-x86_64`, and runs Clippy through the same q35/UEFI build configuration.
The maintained sibling repositories own their complete unit, MSRV, packaging,
and release checks; TheKernel verifies their pinned integration boundary.

## Semantic and targeted tests

The full QEMU semantic boot remains an explicit heavier operation:

```bash
make system-test
```

Targeted smokes and host Linux differential oracles remain directly runnable:

```bash
make smoke NAME=lwext4-io-boost ARCH=x86
scripts/ci/futex-host-differential.sh
scripts/ci/epoll-host-differential.sh
```

These commands report their actual test output. Performance and research tools
may retain checksums, manifests, or receipts when those artifacts are part of
the experiment; such evidence is not a substitute for an ordinary source or
product test result.
