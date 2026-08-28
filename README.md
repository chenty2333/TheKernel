# TheKernel

TheKernel is a personal Rust operating-system project providing a
Linux-compatible userspace ABI on ArceOS components. x86_64 is the only
supported product architecture; the reference machine is QEMU `q35` with
UEFI/OVMF.

The bounded project intent and durable engineering decisions are maintained in
[`RSH.md`](RSH.md) and [`.rsh/records/`](.rsh/records/). Local working focus is
stored in `.rsh/focus.toml`; it is not a release-status or compatibility claim.
Unsafe review islands and their invariants are indexed in
[`docs/unsafe-boundaries.md`](docs/unsafe-boundaries.md).

## Checkout and development environment

Create the three-checkout workspace from a single TheKernel clone. The
bootstrap command reads the immutable repository/ref/path records in
[`config/source-combination.toml`](config/source-combination.toml), fetches
each listed commit, and checks out that exact commit detached:

```bash
mkdir thekernel-workspace
git clone https://github.com/chenty2333/TheKernel.git thekernel-workspace/TheKernel
cd thekernel-workspace/TheKernel
python3 scripts/ci/bootstrap_sources.py
```

The resulting workspace is `thekernel-workspace/{TheKernel,thekernel-ax,thekernel-linux-abi}`.
On later runs, the bootstrap tool only verifies existing sibling checkouts: it
never overwrites them, and it refuses a dirty checkout or a different commit.
Update or remove an existing sibling explicitly before rerunning it. CI uses
the same manifest and prints a stable combination ID that includes the checked-out
TheKernel commit.

Use the same immutable development image as CI:

```bash
export THEKERNEL_DEV_IMAGE=ghcr.io/chenty2333/thekernel-dev@sha256:279c82be5d0a98814293912e3c8f87ccbcc1471a1781690768c1771cefd78fe7
./scripts/dev-shell.sh -- bash
```

Alternatively, build the local image from the checked-in `dev-env/Dockerfile`
and enter it (the script rebuilds when its Dockerfile inputs change):

```bash
export THEKERNEL_DEV_IMAGE=thekernel-dev:local
./scripts/dev-shell.sh -- bash
```

Boot the interactive TheKernel guest shell directly from the host with
`./scripts/dev-shell.sh --guest-shell`.

The image deliberately contains no Rust runtime. Install rustup there, then
run `rustup show` from this checkout to install the toolchain declared by the
root [`rust-toolchain.toml`](rust-toolchain.toml): `nightly-2026-08-23`
(`rustc 1.100.0-nightly`, `c54751567`, commit date 2026-08-22). Product builds
also require the configuration generator; LLVM object tools come from the
toolchain's declared `llvm-tools` component:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain none --no-modify-path
export PATH="$HOME/.cargo/bin:$PATH"
rustup show
cargo install --locked --version 0.2.1 axconfig-gen
```

## Product entry point

`./tools/thekernel.py` is the only product build and boot entry point:

```bash
./tools/thekernel.py build
./tools/thekernel.py rootfs
./tools/thekernel.py run --profile shell --interactive
./tools/thekernel.py system-test --smp 4 --accel tcg
./tools/thekernel.py lint --smp 4
```

Its commands write below `${THEKERNEL_STATE_DIR:-.state}`. With defaults, the
system kernel and ESP are under
`.state/out/x86_64/q35-uefi/system/smp4-mem1g/`, and the root filesystem is
`.state/out/rootfs/x86/rootfs-x86.img`.

## Verification

Formatting and host tests are direct Cargo commands. The full host kernel test
uses the same linker settings as CI:

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
env \
  CC=gcc CXX=g++ AR=ar AS=as OBJCOPY=objcopy OBJDUMP=objdump SIZE=size \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-T$PWD/third_party/rust-patches/scope-local/percpu.x" \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$PWD/scripts/ci/host-test-linker.sh" \
  cargo test --locked --manifest-path kernel/Cargo.toml --tests \
    --features bpf,axtask/test,test-io-control \
    --target x86_64-unknown-linux-gnu -- --test-threads=1
```

Run the QEMU system suite with `./tools/thekernel.py system-test`; it reports
the guest KTAP suite. The same portable C assertions can be run against Linux
without a per-case wrapper:

```bash
./scripts/host-differential.sh
```

For the source-bound product gate, run all four product checks through one
manifest writer. It refuses a dirty checkout or mismatched sibling commit and
hashes the command logs, final kernel/ESP/rootfs, and complete guest console:

```bash
python3 -m tools.qemu_runner.gate_manifest \
  --output .state/gate/q35-preview-v0/manifest.json
```

Run a named semantic smoke with:

```bash
./scripts/smoke.sh list
./scripts/smoke.sh lwext4-io-boost
```

See [`docs/testing.md`](docs/testing.md) for the testing boundary, and
[`PROVENANCE.md`](PROVENANCE.md) for source and generated-rootfs provenance.

The current bounded product claim is `q35-preview-v0`; it is not a claim of
complete Linux ABI coverage, distribution/container compatibility, bare-metal
support, or general performance superiority. Its exact gate and fixed Linux
comparison baseline are defined in [`docs/testing.md`](docs/testing.md).

CI may override its checked-in development-image digest with the
`THEKERNEL_DEV_IMAGE` repository variable. When set, it must be an immutable
`ghcr.io/...@sha256:...` system-image reference; Rust remains controlled solely
by the root `rust-toolchain.toml` for product builds and CI.

## Repository layout

- `kernel/`: Linux-compatible kernel and syscall integration.
- `crates/`: maintained generic and reusable components.
- `config/`: x86_64 product configuration and GRUB configuration.
- `tools/thekernel.py`: product build, boot, system-test, and lint entry point.
- `tools/qemu_runner/`: x86_64 QEMU runner implementation.
- `tests/guest/`: system suite and semantic smoke command streams.

## License

TheKernel source is distributed under Apache-2.0; see [LICENSE](LICENSE),
[NOTICE](NOTICE), and [PROVENANCE.md](PROVENANCE.md). Third-party and vendored
directories retain their upstream license terms, authorship, immutable source
records, and patch provenance.
