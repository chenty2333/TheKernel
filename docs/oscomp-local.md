# OSCOMP Local Development And Replay

## Development Environment

The repository uses a repo-local Docker environment. The main entrypoints are:

- `make dev-image`
- `make dev-check`
- `make dev-shell`
- `make dev-shell-root`

The image contract is enforced by [`dev-env/check-image.sh`](/home/dia/TheKernel/dev-env/check-image.sh):

- Rust toolchain: `nightly-2025-05-20`
- QEMU: `9.2.1`
- repo-local build helpers: `cargo-axplat`, `axconfig-gen`, `rust-objcopy`
- cross compilers for `riscv64` and `loongarch64`
- disk/image tools such as `mke2fs`, `debugfs`, `truncate`, `mkfs.vfat`, `mkimage`

[`scripts/dev-shell.sh`](/home/dia/TheKernel/scripts/dev-shell.sh) mounts:

- this repository at `/workspace`
- official testsuite data read-only at `/opt/oskernel/testsuites`

If official images are not under `~/kernel-image` or `~/testsuits-for-oskernel`, override the mount source with:

```bash
OSCOMP_TESTSUITE_HOST_DIR=/abs/path/to/kernel-image make dev-shell
```

## Build Artifacts

Evaluator-facing artifacts are:

- `kernel-rv`
- `kernel-la`
- `disk.img`

Main commands:

```bash
make kernel-rv
make kernel-la
make disk.img
make all
```

Top-level artifact builds now scrub repository-local build garbage before starting:

- `.tmp`
- `.state`

This means a fresh top-level build replaces stale replay workdirs, old state trees, and previous support-disk temp roots automatically.

The lower-level `make -C make disk_img` target also overwrites an existing disk image instead of warning and keeping the stale one.

## Local Replay

Main replay entrypoints:

- `./scripts/oscomp.sh list`
- `./scripts/oscomp.sh run --arch rv`
- `./scripts/oscomp.sh run --arch la`
- `./scripts/oscomp.sh verify --arch rv`
- `./scripts/oscomp.sh verify --arch la`
- `./scripts/replay-oscomp-eval.sh --arch rv`
- `./scripts/replay-oscomp-eval.sh --arch la`

[`scripts/oscomp.sh`](/home/dia/TheKernel/scripts/oscomp.sh) exposes the current fixed reference plan:

- `/musl basic`
- `/musl iozone`
- `/musl busybox`
- `/musl netperf`
- `/musl lua`
- `/musl libcbench`
- `/musl libctest`
- `/musl cyclictest`
- `/glibc basic`
- `/glibc iozone`
- `/glibc busybox`
- `/glibc netperf`
- `/glibc lua`
- `/glibc libcbench`
- `/glibc cyclictest`
- `/musl lmbench`
- `/glibc lmbench`
- `/musl ltp`
- `/glibc ltp`
- `/musl iperf`
- `/glibc iperf`

[`scripts/replay-oscomp-eval.sh`](/home/dia/TheKernel/scripts/replay-oscomp-eval.sh) replays the official pre-2025 flow with:

- official rootfs image as the first block device
- repository-built `disk.img` as the support disk
- contest QEMU shape for `rv` and `la`
- whole-QEMU timeout protection
- optional `--workdir` and `--keep-workdir`

Official image search order is:

- `$OSCOMP_TESTSUITE_DIR`
- `/home/dia/kernel-image`
- `$HOME/kernel-image`
- `$HOME/testsuits-for-oskernel`
- `/coursegrader/testdata`

Accepted suffixes:

- `.img`
- `.img.xz`
- `.img.gz`

## Support Disk

[`scripts/build-oscomp-support-disk.sh`](/home/dia/TheKernel/scripts/build-oscomp-support-disk.sh) builds the support disk used by replay and evaluator-facing flows.

Current payload includes:

- `/meta/ltp_test.txt`
- optional `/meta/oscomp_plan.txt`
- `/usr/lib/locale/C.UTF-8`
- `rv` and `la` glibc `libgcc_s.so.1`
- per-arch overlay tools under `/<arch>/overlay`

Overlay tools currently include:

- `ar`
- `date`
- `file`
- `readelf`
- `oscomp-default-signals`
- a minimal `make`
- `liboscomp-musl-compat.so`
- `rv` also gets `liboscomp-mmsg-compat.so`

At boot, [`src/init.sh`](/home/dia/TheKernel/src/init.sh) mounts the support disk read-only, copies what it needs into the rootfs, then unmounts `/support` early.

## Focused Replay And Overrides

The local framework already supports focused replay instead of only full-matrix runs.

Support-disk build options:

- `--test-list PATH`
- `--plan-override PATH`

Top-level support-disk build also accepts:

```bash
OSCOMP_PLAN_OVERRIDE=/abs/path/plan.txt make disk.img
```

This allows:

- focused LTP subsets
- custom evaluation plans
- targeted regression checks without editing the default guest runner

## Output And Debugging

The guest runner in [`src/init.sh`](/home/dia/TheKernel/src/init.sh) is evaluator-oriented:

- fixed group markers: `#### OS COMP TEST GROUP START ... ####`
- LTP emits `RUN LTP CASE ...`
- LTP keeps native `TINFO` / `TPASS` / `TFAIL` / `Summary`
- each case ends with `FAIL LTP CASE ... : ret`
- default mode is streaming output, not buffered success-only output

QEMU-side debugging is supported without polluting default evaluator output:

- `OSCOMP_QEMU_DEBUG`
- `OSCOMP_QEMU_DEBUG_FILE`
- `OSCOMP_REPLAY_VERBOSE`

These are handled by [`scripts/replay-oscomp-eval.sh`](/home/dia/TheKernel/scripts/replay-oscomp-eval.sh).

## Current Boundaries

What this framework is good at now:

- rebuilding evaluator-facing artifacts reproducibly
- replaying official `rv` and `la` images locally
- validating official image layout before replay
- injecting support payloads through `disk.img`
- running full or focused LTP subsets
- keeping evaluator output format stable while still allowing explicit debug modes

What it does not try to do:

- emulate the old StarryOS showcase flow
- hide kernel failures behind timeout tuning
- make local replay independent from official image layout assumptions
