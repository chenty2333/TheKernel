# TheKernel

TheKernel is a personal Rust operating-system project that provides a
Linux-compatible userspace ABI on top of ArceOS components. Its target is a
high-performance, production-usable x86_64 kernel. It is being evolved toward
reusable, explicitly layered kernel components rather than syscall-local
implementations.

The repository is currently a preview. It has broad syscall and subsystem
coverage, but it is not yet a drop-in Linux replacement and its public crate
boundaries are still being stabilized.

## Architecture

x86_64 is the only product architecture. The reference virtual machine is QEMU
`q35` with UEFI/OVMF; builds, boot checks, and system tests use this platform.

The code follows five ownership layers:

1. architecture and HAL primitives;
2. generic task, driver, filesystem, and network mechanisms;
3. reusable Linux ABI support for process, VFS, fd, readiness, credentials,
   memory, and related semantics;
4. thin syscall argument handling and subsystem composition;
5. project build, boot, test, and diagnostic tooling.

Linux-visible policy should not leak into generic `ax-*` mechanisms, and
syscall bodies should not duplicate rules owned by the ABI-support layer.

The project's durable design, selected direction, current position, and known
pitfalls live respectively in [`maproom/terrain.md`](maproom/terrain.md),
[`maproom/route.md`](maproom/route.md),
[`maproom/basecamp.md`](maproom/basecamp.md), and
[`maproom/hazards.md`](maproom/hazards.md).

## Checkout layout

The root workspace consumes three maintained sibling repositories through
relative paths. Local development and CI use this layout:

```text
parent/
  TheKernel/
  vISA/
  thekernel-ax/
  thekernel-linux-abi/
```

Validate the layout with:

```bash
./scripts/ci.sh layout
```

GitHub Actions checks out exact sibling commits for each integration run. A
local developer may intentionally use different sibling revisions while
working on a cross-repository change.

## Development environment

The supported build environment is the repository development container. Build
it once, verify the installed tools, and enter a shell:

```bash
make dev-image
make dev-check
make dev-shell
```

From the host, run a one-shot command with:

```bash
make dev-shell DEV_CMD='make all'
```

Inside an already-open development shell, run the inner command directly.

GitHub Actions uses the same maintained image through the `THEKERNEL_DEV_IMAGE` repository variable, falling back to the repository `thekernel-dev:nightly` package. Publish the development image once before enabling the new test workflows.

## Build

Build the release-mode x86_64 kernel image:

```bash
make all
```

The materialized artifact is `kernel-x86_64`. Explicit build targets are also
available:

```bash
make kernel-x86_64
make artifacts
```

Kernel and rootfs outputs use a content-addressed cache under
`.state/build-cache/`; Cargo target caches remain under `.state/x86_64/target`.

## Root filesystem

The project test rootfs is built from a checksum-pinned BusyBox release plus
TheKernel-owned init and guest test programs:

```bash
make test-fixtures
make rootfs-x86
```

The first build downloads BusyBox 1.36.1 into `.state/source-cache/`. Generated
ext4 images live under `.state/rootfs/` and are local or CI fixtures, not
published kernel release artifacts. See [PROVENANCE.md](PROVENANCE.md) before
redistributing a generated image.

## Boot

Boot an interactive x86_64 project rootfs:

```bash
make shell-x86_64
```

Exit the guest shell to trigger a clean kernel shutdown. Additional runner
options can be passed through `SHELL_ARGS`.

The policy-neutral runner also accepts explicit artifacts:

```bash
python3 -m tools.qemu_runner run \
  --arch x86_64 \
  --kernel kernel-x86_64 \
  --rootfs .state/rootfs/rootfs-x86.img \
  --timeout 300
```

## Verification

The public verification front door is `scripts/ci.sh`.

Run the pull-request quality gate:

```bash
./scripts/ci.sh quick
```

It performs whitespace and formatting checks, validates vendored provenance,
runs build-tool, differential-tool, and local-adapter tests, checks and tests
the host kernel once, and runs host-profile Clippy.

Run the maintained local/fork contract tests that are outside the root Cargo
workspace:

```bash
./scripts/ci.sh patches
```

This covers the patched smoltcp, IPI, synchronization, memory-set, scope-local,
VFS/ext4, lwext4, and axnet/vsock profiles without rerunning the complete
kernel binary under dozens of filters.

Build and lint the actual x86_64 product configuration, including the
non-default diagnostic and test-control profiles:

```bash
./scripts/ci.sh kernel
```

Run the complete pull-request gate (`quick`, `patches`, and `kernel`):

```bash
./scripts/ci.sh all
```

The QEMU semantic system test remains an explicit heavier tier:

```bash
./scripts/ci.sh system
```

Targeted storage and page-cache smokes remain directly selectable:

```bash
./scripts/ci.sh smoke lwext4-io-boost --arch x86
```

Host Linux differential oracles are also explicit:

```bash
./scripts/ci.sh differential futex
./scripts/ci.sh differential epoll
```

See [`docs/testing.md`](docs/testing.md) for the tier policy and the boundary
between repository tests, sibling-crate tests, product boots, and research or
performance evidence.

## Lints

Clippy runs in two configurations because they answer different questions:

```bash
scripts/ci/clippy-gate.sh --profile host
scripts/ci/clippy-gate.sh --profile x86_64
```

The host profile covers tests and generic paths. The x86_64 profile reuses the
real kernel build machinery so platform configuration, features, and
`RUSTFLAGS` match the shipped image. The lint policy lives in
`[workspace.lints]` in the root `Cargo.toml`.

## Repository layout

- `kernel/`: Linux-compatible kernel and syscall integration.
- `crates/`: maintained generic and reusable components.
- `third_party/rust-patches/`: pinned upstream sources and provenance records.
- `make/`: architecture build machinery.
- `tools/build.py`: content-addressed kernel and rootfs builder.
- `tools/qemu_runner/`: policy-neutral x86_64 QEMU runner.
- `tests/guest/`: project init, guest helpers, and nightly programs.
- `scripts/ci.sh`: stable developer and CI front door.
- `scripts/ci/`: focused reusable helpers, differential oracles, and optional
  nightly or research checks.
- `scripts/smoke/`: named QEMU semantic smokes.

## Cleaning

`make clean` removes materialized kernels and run/build outputs while retaining
content and Cargo caches. `make clean-all` removes all generated `.state` data.

## License and provenance

TheKernel source is distributed under Apache-2.0; see [LICENSE](LICENSE),
[NOTICE](NOTICE), and [PROVENANCE.md](PROVENANCE.md). Third-party and vendored
directories retain their upstream license terms, authorship, immutable source
records, and patch provenance.
