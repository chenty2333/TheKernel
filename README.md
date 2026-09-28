# TheKernel

TheKernel is a personal Rust operating system that runs unmodified Linux
userspace. It implements the Linux syscall ABI on top of components derived
from ArceOS, and targets x86_64 only.

The reference machine is QEMU `q35` with UEFI/OVMF. Bring-up on real hardware
(an Intel Core i3-N305 mini PC, `--platform n305`) is in progress; see
[docs/design/n305-platform.md](docs/design/n305-platform.md).

What currently runs on it:

- an interactive shell image, optionally with a native C toolchain
  (`tcc`, glibc, or a distribution `gcc`) and a nested QEMU that boots Linux;
- a Weston desktop with a terminal, file manager, editor, image viewer,
  Python and the WebKitGTK MiniBrowser, using Virgl OpenGL and VirtIO sound;
- VirtIO block/net/input/GPU/sound devices, plus xHCI USB keyboards, mice
  and mass storage.

It is not a claim of complete Linux ABI coverage, distribution or container
compatibility, or general performance parity with Linux.

## Quick start

Everything builds inside the checked-in development container
(`dev-env/Dockerfile`), which CI uses as well:

```bash
git clone https://github.com/chenty2333/TheKernel.git
cd TheKernel
./scripts/dev-shell.sh -- scripts/setup-toolchain.sh   # first time only
./scripts/dev-shell.sh -- bash                         # enter the shell
```

The image is built on first use; pass `--build` to rebuild it after changing
the Dockerfile. With rootless Podman behind `DOCKER_HOST`, also set
`THEKERNEL_ROOTLESS_PODMAN=1`. The Rust toolchain is pinned by
`rust-toolchain.toml`.

Then boot something:

```bash
make run                          # interactive shell (KVM, 4 CPUs, 1 GiB)
make run RUN_ARGS="--toolchain gcc"   # shell image with a C compiler
make run-gui RUN_ARGS=--build     # build and open the Weston desktop
make run-gui                      # reopen the desktop without rebuilding
```

The first desktop build compiles Buildroot and WebKit and takes a while.
Shut the desktop down from the panel button; killing QEMU can lose pending
writes. The desktop home directory lives on a persistent disk under
`${XDG_DATA_HOME:-~/.local/share}/thekernel/desktop/` and survives rebuilds
and `make clean`. See [docs/graphics-rootfs.md](docs/graphics-rootfs.md) for
desktop build dependencies and options.

## Commands

`tools/thekernel.py` is the single build, boot and test entry point; the
Makefile wraps it with resource limits for everyday use.

| Command | Purpose |
|---|---|
| `thekernel.py build` | Build the kernel and UEFI ESP |
| `thekernel.py run` | Build and boot (`--profile shell --interactive` for a shell) |
| `thekernel.py run-gui` | Boot the Weston desktop (`--build` to update images) |
| `thekernel.py test --suite …` | `host`, `guest`, `abi`, `graphics`, `cpu`, `fbcon`, `all` |
| `thekernel.py bench --suite …` | `scheduler`, `io`, `graphics`, `all` |
| `thekernel.py lint` | Clippy for the product kernel configuration |
| `thekernel.py verify --tier …` | `daily`, `full`, `hardware` (see below) |
| `thekernel.py clean` | Remove generated run, output and cache directories |

Useful `run` options: `--gdb` exposes a GDB socket and pauses on
shutdown/panic; `--input-backend usb` and `--usb-disk IMAGE` switch input to
xHCI and attach a disk image; `--graphics-profile interactive` uses software
rendering. Kernel output goes to `kernel.log`, separate from the guest
terminal; see [docs/debugging.md](docs/debugging.md).

Build artifacts are written below
`${THEKERNEL_STATE_DIR:-~/.cache/thekernel-targets}`. On non-Debian hosts,
building the rootfs outside the container needs a static C library (Fedora:
`glibc-static`).

## Verification

```bash
./scripts/dev-shell.sh -- ./tools/thekernel.py verify --tier daily
./scripts/dev-shell.sh -- ./tools/thekernel.py verify --tier full
./tools/thekernel.py verify --tier hardware    # on a KVM host
```

| Tier | Contents | Runs on |
|---|---|---|
| `daily` | Static gates (dependency layers, module edges, ABI declarations and contracts, log surface), host tests, build, Clippy, the guest suite under TCG, framebuffer console | Pull requests and pushes to `main` |
| `full` | `daily` plus the pinned Buildroot graphics image and a Pixman rendering check | Nightly, when `main` has new commits |
| `hardware` | CPU correctness suite and the complete Linux ABI differential under KVM | Nightly and on demand, on a self-hosted KVM runner |

The guest suite must produce complete KTAP output with no failures or skips
and shut down cleanly. The ABI suite runs every registered contract on both
TheKernel and Linux 7.2.3 and compares the results; setting
`THEKERNEL_ABI_PROGRAMS` narrows a direct run, which is then reported as
`PARTIAL`. Benchmarks are described in [docs/benchmarks.md](docs/benchmarks.md).

## Repository layout

| Path | Contents |
|---|---|
| `kernel/` | Linux-compatible kernel and syscall integration |
| `crates/ax/` | Mechanism and platform crates (scheduler, memory, drivers, HAL, …) |
| `crates/linux/` | Reusable Linux ABI crates (signals, futex, io_uring, VFS, BPF, …) |
| `crates/*-adapter/` | Adapters between the layers |
| `config/` | Platform profiles, kernel configuration, ABI declarations |
| `tools/` | `thekernel.py`, the QEMU runner and helper tools |
| `scripts/` | Container, toolchain, rootfs and CI scripts |
| `tests/` | Host tests and the guest system suite |
| `docs/` | Debugging, licensing, provenance and design records |

Every workspace package declares its layer in
`package.metadata.thekernel.layer`, and CI enforces the allowed edges:
`mechanism` crates use only mechanisms, `platform` adds platform crates,
`linux_abi` adds Linux ABI crates, and `integration` (the kernel) may use all
of them. Keep Linux objects and product policy out of lower layers.
`scripts/ci/check_kernel_module_edges.py` tracks coupling between modules
inside the kernel crate; see
[docs/design/kernel-module-coupling.md](docs/design/kernel-module-coupling.md).

Component crates are named `tk-*` and are not published.

## License

TheKernel's own sources are Apache-2.0 ([LICENSE](LICENSE), [NOTICE](NOTICE)).
Vendored and third-party code keeps its upstream license. A distributed
binary built with the default features also links GPL-2.0-or-later C code;
[docs/licensing.md](docs/licensing.md) explains what a redistributor must
ship, and [docs/upstream-provenance.md](docs/upstream-provenance.md) indexes
the upstream sources this repository draws on.
