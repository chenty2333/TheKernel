# TheKernel

TheKernel is a Rust operating-system kernel that provides a Linux-compatible
userspace ABI on top of ArceOS components. It targets RISC-V and LoongArch QEMU
platforms and is being evolved toward reusable, explicitly layered kernel
components rather than syscall-local implementations.

The repository is currently a preview. It has broad syscall and subsystem
coverage, but it is not yet a drop-in Linux replacement and its public crate
boundaries are still being stabilized.

## Architecture

The code follows five ownership layers:

1. architecture and HAL primitives;
2. generic task, driver, filesystem, and network mechanisms;
3. reusable Linux ABI support for process, VFS, fd, readiness, credentials,
   memory, and related semantics;
4. thin syscall argument handling and subsystem composition;
5. project build, boot, test, and diagnostic tooling.

Linux-visible policy should not leak into generic `ax-*` mechanisms, and
syscall bodies should not duplicate rules owned by the ABI-support layer.

## Development Environment

The supported build environment is the repository development container. Build
it once, verify the pinned toolchains, and then enter a shell:

```bash
make dev-image
make dev-check
make dev-shell
```

From the host, a one-shot command can be run with:

```bash
make dev-shell DEV_CMD='make all'
```

Inside an already-open development shell, run the inner `make` command
directly.

## Build

Build both release-mode kernel images:

```bash
make all
```

The materialized artifacts are:

- `kernel-rv`
- `kernel-la`

Build one architecture or the complete kernel artifact set explicitly:

```bash
make kernel-rv
make kernel-la
make artifacts
```

`make artifacts` produces only those two kernel images; it does not build or
publish test fixtures. Kernel and rootfs outputs use a content-addressed cache
under `.state/build-cache/`; Cargo target caches remain under
`.state/<arch>/target`.

## Root Filesystem

The project test rootfs is built from a checksum-pinned BusyBox release plus
TheKernel-owned init and guest test programs:

```bash
make test-fixtures
make rootfs-rv
make rootfs-la
```

The first build downloads BusyBox 1.36.1 into `.state/source-cache/`. The
resulting ext4 images contain the same semantic helpers used by local smoke and
nightly system tests. These repository-built images live under `.state/rootfs/`
and are local/CI fixtures, not published kernel release artifacts. No external
test image is required. The generated image contains its applicable project
and BusyBox notices; see
[PROVENANCE.md](PROVENANCE.md) before redistributing a generated image.

## Boot

Boot an interactive project rootfs on either architecture:

```bash
make shell-rv
make shell-la
```

Exit the guest shell to trigger a clean kernel shutdown. Additional generic
runner options can be passed through `SHELL_ARGS`.

The underlying runner accepts only explicit artifacts and keeps architecture
topology, image modes, timeouts, serial capture, and interaction separate from
test policy:

```bash
python3 -m tools.qemu_runner run \
  --arch rv \
  --kernel kernel-rv \
  --rootfs .state/rootfs/rootfs-rv.img \
  --timeout 300
```

## Verification

Run the project semantic init on both architectures:

```bash
make system-test
```

The system test covers PID 1 and child `execve` transitions, rootfs mutation,
tmpfs mount lifecycle, procfs reads, process creation, wait, pipes, and clean
shutdown. Targeted subsystem smokes exercise more specialized storage,
writeback, interrupt, mapped-I/O, pinning, and page-cache contracts:

```bash
make smoke-list
make smoke NAME=lwext4-io-boost ARCH=rv
```

Run host-side tool and contract tests:

```bash
make test-tools
scripts/ci/per-commit.sh
```

The per-commit gate runs focused, single-threaded test sets per subsystem, each
with a floor on the number of tests executed, and then runs the whole suite once
with the harness' default parallelism. The focused runs give precise
attribution; only the full run can observe tests that no filter names, or
interference between tests that share kernel globals.

## Lints

`cargo fmt` and `cargo clippy` both gate. Clippy runs per build profile,
because several lints answer a different question in each one: a symbol that is
unreachable in the `x86_64` host test build is often the live architecture
path, `GlobalGrace` only carries drop glue when `smp-tlb-shootdown` is enabled,
and a `c_char` cast that is redundant on RISC-V is required on the host.

```bash
scripts/ci/clippy-gate.sh --profile host
scripts/ci/clippy-gate.sh --profile rv
scripts/ci/clippy-gate.sh --profile la
```

The per-commit gate runs the `host` and `rv` profiles; the PR gate adds `la`.
Only TheKernel-owned packages are linted. Vendored sources under
`third_party/rust-patches/` keep their upstream lint posture; their diagnostics
are counted and printed but never fail the gate, so a clean owned report never
implies a clean tree.

The lint policy lives in `[workspace.lints]` in the root `Cargo.toml`, so
editors and a bare `cargo clippy` enforce exactly what CI does. Every allowance
there records the mechanism that makes the lint wrong for this codebase.

The PR gate builds both architectures and boots the project rootfs. Nightly
adapters add mixed pressure, deterministic allocation failure, ext4 power-cut
recovery, and non-loopback network coverage.

## Repository Layout

- `kernel/`: Linux-compatible kernel and syscall integration.
- `crates/`: maintained generic and reusable components.
- `third_party/rust-patches/`: pinned upstream sources with provenance records.
- `make/`: architecture build machinery.
- `tools/build.py`: content-addressed kernel and rootfs builder.
- `tools/qemu_runner/`: policy-neutral dual-architecture QEMU runner.
- `tests/guest/`: project init, guest helpers, and nightly programs.
- `scripts/ci/` and `scripts/smoke/`: repository verification workflows.

## Cleaning

`make clean` removes materialized kernels and run/build outputs while retaining
the content and Cargo caches. `make clean-all` removes all generated `.state`
data.

## License and Provenance

TheKernel source is distributed under Apache-2.0; see [LICENSE](LICENSE),
[NOTICE](NOTICE), and [PROVENANCE.md](PROVENANCE.md). Third-party and vendored
directories retain their upstream license terms, authorship, immutable source
records, and patch provenance.
