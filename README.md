# TheKernel

TheKernel is a personal Rust operating-system project providing a
Linux-compatible userspace ABI on ArceOS components. x86_64 is the only
supported product architecture; the reference machine is QEMU `q35` with
UEFI/OVMF.

## Checkout and development environment

Create the three-checkout workspace from a single TheKernel clone. The
bootstrap command reads the repository/ref/path configuration in
[`config/source-combination.toml`](config/source-combination.toml) and checks
out each repository's `main` branch:

```bash
mkdir thekernel-workspace
git clone https://github.com/chenty2333/TheKernel.git thekernel-workspace/TheKernel
cd thekernel-workspace/TheKernel
python3 scripts/ci/bootstrap_sources.py
```

The resulting workspace is `thekernel-workspace/{TheKernel,thekernel-ax,thekernel-linux-abi}`.
On later runs, the bootstrap tool only verifies existing sibling checkouts: it
never overwrites them, and it refuses a dirty checkout or any branch other than
`main`. Update an existing sibling explicitly before rerunning it. CI uses the
same source configuration.

CI requires the `THEKERNEL_DEV_IMAGE` repository variable to name an immutable
image under the project's `ghcr.io` namespace. Container jobs reference the
variable directly, and `dev-env/check-image.sh` validates the expected tools
and versions inside that image. Publishing the image from the checked-in
`dev-env/Dockerfile` and updating the repository variable remain explicit
maintainer operations.

Alternatively, build the local image from the checked-in `dev-env/Dockerfile`
and enter it. The local image is built on first use when it is missing; pass
`--build` to force a rebuild:

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

For the ordinary interactive shell workflow, the root Makefile supplies a
small, resource-bounded wrapper around that same entry point:

```bash
make run
make run-existing  # reuse already-built kernel, ESP, and rootfs artifacts
make build         # build without booting
make lint          # run Clippy for the product kernel configuration
make test          # run the host verification suite
make clean         # remove generated run, output, and cache directories
make docker-clean  # remove the dev container volume and local image
```

Its commands write below `${THEKERNEL_STATE_DIR:-~/.cache/thekernel-targets}`.
With defaults, the system kernel and ESP are under
`~/.cache/thekernel-targets/out/x86_64/q35-uefi/system/mem1g/`, and the
root filesystem is `~/.cache/thekernel-targets/out/rootfs/x86/rootfs-x86.img`. On non-Debian x86_64 hosts the rootfs
build falls back to the native gcc and needs the static C library (Fedora:
`glibc-static`, Debian: `libc6-dev`).

## Verification

Formatting and host tests are direct Cargo commands. The full host kernel test
uses the same linker settings as CI:

```bash
cargo fmt \
  -p thekernel \
  -p thekernel-kernel \
  -p thekernel-linux-process-adapter \
  -p thekernel-readiness-adapter \
  -- --check
cargo test --locked -p thekernel-readiness-adapter
cargo test --locked -p thekernel-linux-process-adapter
env \
  CC=gcc CXX=g++ AR=ar AS=as OBJCOPY=objcopy OBJDUMP=objdump SIZE=size \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-T$PWD/../thekernel-ax/crates/thekernel-scope-local/percpu.x" \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$PWD/scripts/ci/host-test-linker.sh" \
  cargo test --locked --manifest-path kernel/Cargo.toml --tests \
    --features bpf,axtask/test \
    --target x86_64-unknown-linux-gnu -- --test-threads=1
```

Run the QEMU system suite with `./tools/thekernel.py system-test`; it reports
the guest KTAP suite.

For the Q35 product gate, build and run the guest KTAP suite. A pass requires
the complete suite to finish without failures or skips and the guest to shut
down normally:

```bash
./tools/thekernel.py system-test --smp 4 --accel tcg
```

The current bounded product claim is `q35-preview-v0`; it is not a claim of
complete Linux ABI coverage, distribution/container compatibility, bare-metal
support, or general performance superiority.

The `THEKERNEL_DEV_IMAGE` repository variable must be updated after publishing
a rebuilt development image. Rust remains controlled solely by the root
`rust-toolchain.toml` for product builds and CI.

## Repository layout

- `kernel/`: Linux-compatible kernel and syscall integration.
- `crates/`: maintained generic and reusable components.
- `config/`: x86_64 product configuration and GRUB configuration.
- `tools/thekernel.py`: product build, boot, system-test, and lint entry point.
- `tools/qemu_runner/`: x86_64 QEMU runner implementation.
- `tests/guest/`: system suite and semantic smoke command streams.

## License

TheKernel source is distributed under Apache-2.0; see [LICENSE](LICENSE) and
[NOTICE](NOTICE). Third-party and vendored directories retain their upstream
license terms and authorship notices.
