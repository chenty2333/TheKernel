# TheKernel

TheKernel is a personal Rust operating-system project providing a
Linux-compatible userspace ABI on ArceOS components. x86_64 is the only
supported product architecture; the reference machine is QEMU `q35` with
UEFI/OVMF.

## Checkout and development environment

Clone the single repository; its root Cargo workspace includes the kernel,
mechanism crates (`crates/ax/`), and Linux ABI crates (`crates/linux/`).

```bash
git clone https://github.com/chenty2333/TheKernel.git
cd TheKernel
```

CI builds the checked-in `dev-env/Dockerfile` with a reusable BuildKit cache,
then runs the same `scripts/dev-shell.sh` commands used locally. No repository
image variable or registry publication is required. Locally the image is built
on first use; pass `--build` to rebuild after changing the Dockerfile:

```bash
export THEKERNEL_DEV_IMAGE=thekernel-dev:local
./scripts/dev-shell.sh -- bash
```

When Docker Compose connects to a rootless Podman API socket, set
`THEKERNEL_ROOTLESS_PODMAN=1` alongside `DOCKER_HOST`. This opt-in maps the host
caller to container root so mounted checkout writes retain host ownership. The
existing entrypoint adjusts ownership of its dedicated persistent home volume;
it does not recursively change checkout ownership. Leave this option unset for
Docker.

Boot the interactive TheKernel guest shell directly from the host with
`./scripts/dev-shell.sh --guest-shell`.

Provision the pinned Rust toolchain and `axconfig-gen` explicitly inside the
image, using the same script as CI:

```bash
./scripts/dev-shell.sh -- scripts/setup-toolchain.sh
```

The root `rust-toolchain.toml` selects Rust and its required components.
Verification checks the environment before work starts and never installs tools.

## Product entry point

`./tools/thekernel.py` is the only product build and boot entry point:

```bash
./tools/thekernel.py build
./tools/thekernel.py run --profile shell --interactive
./tools/thekernel.py test --suite guest --smp 4 --accel tcg
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

Kernel output is captured separately from the user terminal in `kernel.log`.
See [kernel diagnostics and request tracing](docs/debugging.md) for runtime log
filters, loss counters, and focused io_uring lifecycle capture.

Its commands write below `${THEKERNEL_STATE_DIR:-~/.cache/thekernel-targets}`.
With defaults, the system kernel and ESP are under
`~/.cache/thekernel-targets/out/x86_64/q35-uefi/system/mem1g/`, and the
root filesystem is `~/.cache/thekernel-targets/out/rootfs/x86/rootfs-x86.img`. On non-Debian x86_64 hosts the rootfs
build falls back to the native gcc and needs the static C library (Fedora:
`glibc-static`, Debian: `libc6-dev`).

## Verification

The same suite entry points serve local development and CI:

```bash
./tools/thekernel.py test --suite host
./tools/thekernel.py test --suite guest --smp 4 --accel tcg
./tools/thekernel.py test --suite abi --accel kvm --smp 4
./tools/thekernel.py test --suite graphics \
  --rootfs "$HOME/.cache/thekernel-targets/graphics/rootfs.ext2" \
  --screenshot "$HOME/.cache/thekernel-targets/graphics/screen.ppm"
./tools/thekernel.py test --suite cpu --accel kvm --smp 4
./tools/thekernel.py test --suite all --accel kvm --smp 4 \
  --rootfs "$HOME/.cache/thekernel-targets/graphics/rootfs.ext2" \
  --screenshot "$HOME/.cache/thekernel-targets/graphics/screen.ppm"
./tools/thekernel.py bench --suite scheduler
./tools/thekernel.py bench --suite io
```

The guest gate requires complete KTAP output with no failures or skips and
normal guest shutdown. CPU and accelerated graphics validation require KVM;
native Intel graphics and bare-metal certification are deferred.

`python3 -m unittest discover -s tests -t .` runs the host Python framework
checks; its temporary files use the existing disk cache without requiring
`TMPDIR`. These checks exercise rejection paths and do not establish guest ABI
or performance results. The ABI declaration gate is likewise a static check;
the ABI suite runs its registered contracts on both TheKernel and Linux 7.2.3.

`--no-build` requires current guest test inputs and matching configuration,
rootfs, kernel and ESP. It can deliberately reuse a previously built kernel
after Rust source edits for baseline comparisons. A configuration or guest
test change requires a rebuild. The comparison runners verify the actual
kernel embedded in each ESP before boot; filesystem-sensitive differential
fixtures also verify their filesystem provider inside the guest.

For a focused guest failure, `tools/thekernel.py run --gdb` prints a Unix GDB
socket and keeps guest shutdown/reboot/panic paused for inspection. Attach GDB
with the matching unstripped Cargo binary. `run --rootfs-transport drive`
uses the same drive boot topology as the graphics and ABI runners.

The current bounded product claim is `q35-preview-v0`; it is not a claim of
complete Linux ABI coverage, distribution/container compatibility, bare-metal
support, or general performance superiority.

## Verification

```bash
./scripts/dev-shell.sh -- ./tools/thekernel.py verify --tier daily
./scripts/dev-shell.sh -- ./tools/thekernel.py verify --tier full
# On a provisioned KVM host:
./tools/thekernel.py verify --tier hardware
```

`daily` checks the environment, changed-line whitespace, dependency boundaries,
graphics configuration, host tests, the product build and Clippy, then runs the
existing system guest suite under TCG with a five-minute guest limit. The
build, lint and system guest share a 512 MiB configuration to keep the real
memory-pressure/reclaim workload within the TCG time budget. It does
not build a desktop rootfs. Clippy rejects correctness and suspicious findings;
style and performance suggestions remain visible advisories, while compiler
errors always fail. `full` adds the pinned Buildroot seatd image and
Pixman pixel smoke. Pull requests and main pushes run daily; scheduled runs use
full, and manual runs choose a tier. Stages print their name, result and failure
category. A timed-out stage terminates its process group; Linux child-subreaper
supervision also reaps descendants that created independent sessions.

`hardware` currently runs the existing CPU KVM correctness suite. Manual CI
first checks for an online idle runner labelled `self-hosted`, `linux`, `x64`,
and `thekernel-kvm`. Missing runners or inaccessible inventory fail explicitly
as **NOT RUN**, and no hardware job is queued. Runner inventory may require the
optional `THEKERNEL_RUNNER_READ_TOKEN` secret with repository Administration:read;
ordinary hosted verification does not need it. Availability is a point-in-time
check, not host capability attestation: the hardware job checks its tools and
KVM access after scheduling. A runner going offline afterward can still leave
the job queued; GitHub does not offer an atomic reserve-and-dispatch operation.

Performance comparisons, Linux ABI differential tests and accelerated graphics
remain explicit specialized suite commands, outside these default gates. Build
artifacts stay in the persistent container home under `.cache/thekernel-targets`.
The development shell rejects host `THEKERNEL_STATE_DIR` overrides because host
absolute paths are not automatically mounted there; unset the override before
using it. Direct host commands still accept that variable. Development containers
have an 8 GiB memory limit with no additional swap allowance.

## Repository layout

- `kernel/`: Linux-compatible kernel and syscall integration.
- `crates/ax/`: reusable mechanism crates.
- `crates/linux/`: reusable Linux ABI crates.
- Other `crates/` directories: maintained adapters and reusable components.
- `config/`: x86_64 product configuration and GRUB configuration.
- `tools/thekernel.py`: product build, boot, system-test, and lint entry point.
- `tools/qemu_runner/`: x86_64 QEMU runner implementation.
- `tests/guest/`: system suite and semantic smoke command streams.

Each workspace package declares `package.metadata.thekernel.layer`. CI checks
all declared local dependency edges, including optional and test dependencies:
`mechanism` uses mechanisms; `platform` uses platform and mechanism crates;
`linux_abi` uses Linux ABI and mechanism crates; `integration` may use all layers.
Standalone algorithms, ABI types, and driver interfaces remain mechanisms;
hardware access and the AX runtime belong to the platform layer.

## License

TheKernel source is distributed under Apache-2.0; see [LICENSE](LICENSE) and
[NOTICE](NOTICE). Third-party and vendored directories retain their upstream
license terms and authorship notices.
