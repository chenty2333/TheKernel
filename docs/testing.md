# Testing

TheKernel separates Rust host tests, portable Linux differentials, the guest
KTAP suite, semantic smokes, and performance measurements.

## `q35-preview-v0` gate

The current product candidate is limited to x86_64 QEMU `q35`, UEFI/OVMF,
4 vCPUs, 1 GiB RAM, and the configured virtio devices. A candidate must be
built from a clean worktree whose repositories match one
`config/source-combination.toml` identity, and must pass formatting/lint, the
host suites, portable Linux differential, and guest KTAP with zero FAIL and
zero SKIP. The guest must then shut down normally: a suite marker alone,
timeout, or runner-terminated QEMU is not a pass.

This preview covers untrusted local guest processes only for implemented ABI
surfaces. It does not claim unmodified distribution/systemd/container support,
strong multi-tenancy, long-running memory-pressure service, production
storage, bare metal, or complete Linux ABI coverage.

Linux semantic and performance comparisons use Linux stable `v6.12.103`
(commit `25c09b42358e73e1476e517b296edb6344f2e4bd`) with the compared kernel
configuration, OVMF, rootfs, helpers, virtual topology, and CPU placement
identified in the result. Syscall dispatcher branch count is not coverage
evidence.

The source-bound product portion of the gate is emitted as one machine-readable
manifest:

```bash
python3 -m tools.qemu_runner.gate_manifest \
  --output .state/gate/q35-preview-v0/manifest.json
```

It runs build, product lint, the portable Linux differentials, and the guest
system test in order. A pass binds the exact clean three-checkout combination,
per-command stdout/stderr, the final kernel/ESP/rootfs launched by the system
test, a complete numbered KTAP plan with zero FAIL/SKIP, the post-suite marker,
and normal guest shutdown. Formatting and host suites remain explicit checks
in the same CI workflow and are not silently folded into this product receipt.

## Host suite

Run formatting and the maintained adapter tests directly with Cargo:

```bash
cargo fmt \
  -p thekernel \
  -p thekernel-kernel \
  -p axnet-ng \
  -p thekernel-linux-process-adapter \
  -p thekernel-readiness-adapter \
  -- --check
cargo test --locked -p thekernel-readiness-adapter
cargo test --locked -p thekernel-linux-process-adapter
```

The host kernel suite needs the project linker settings:

```bash
env \
  CC=gcc CXX=g++ AR=ar AS=as OBJCOPY=objcopy OBJDUMP=objdump SIZE=size \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-T$PWD/third_party/rust-patches/scope-local/percpu.x" \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$PWD/scripts/ci/host-test-linker.sh" \
  cargo test --locked --manifest-path kernel/Cargo.toml --tests \
    --features bpf,axtask/test,test-io-control \
    --target x86_64-unknown-linux-gnu -- --test-threads=1
```

## Portable Linux differential

One runner discovers every source under `tests/guest/portable/`, compiles it
for Linux, and reports its direct `0` PASS, `1` FAIL, or `4` SKIP result as
KTAP:

```bash
./scripts/host-differential.sh
```

The rootfs builder discovers the same source directory for guest execution;
there is no second test manifest.

## Guest KTAP suite

The product system suite builds and boots the x86_64 Q35/UEFI image, then
reports the guest KTAP result:

```bash
./tools/thekernel.py system-test --smp 4 --accel tcg
```

## Semantic smokes

Smokes are named guest command streams run through the product CLI:

```bash
./scripts/smoke.sh list
./scripts/smoke.sh lwext4-io-boost
```

## Performance measurements

Performance runs are explicit. Pass `--receipt PATH` only when recording a
performance measurement that needs its run receipt; ordinary host, KTAP, and
smoke tests do not create one.

```bash
mkdir -p .state/performance
printf '%s\n' \
  '/opt/thekernel-tests/bin/thekernel-mm-performance --iterations 256 --vmas 512 --pin-iterations 64 --pin-workers 4 || exit 1' \
  'exit' > .state/performance/run.commands
./tools/thekernel.py run --profile mm-performance --smp 4 --accel kvm \
  --commands .state/performance/run.commands \
  --receipt .state/performance/run.json
```

A performance claim needs at least five fresh raw repeats on the same topology
and reports throughput, CPU cost, P50, P99, and P99.9. Zero raw samples,
unavailable PMU data, or missing `perf` produces only `not-measured` or degraded
evidence, never `formal`, `complete`, or an “exceeds Linux” claim. Keep an
optimization only when its predeclared primary metric improves by at least 5%
without correctness/resource regressions or more than 5% P99.9 regression;
otherwise revert it.
