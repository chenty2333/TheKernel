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

Run a command directly inside the dev container:

```bash
make dev-shell DEV_CMD="./scripts/oscomp.sh lab audit"
```

## Build Artifacts

Evaluator-facing artifacts are:

- `kernel-rv`
- `kernel-la`
- `disk.img`

Main commands:

```bash
make all
make artifacts
make kernels
make kernel-rv
make kernel-la
make disk.img
make disk-la.img
```

Command intent:

- `make all`: clean evaluator build. This is the remote-submission contract and
  final local confirmation path, not the normal edit/test loop.
- `make artifacts`: refresh all evaluator artifacts without `clean-eval`; it
  keeps arch Cargo target caches but rebuilds the support disk.
- `make kernels`: high-frequency build of both evaluator kernels only.
- `make kernel-rv` and `make kernel-la`: high-frequency single-kernel artifacts
  that preserve `.state/<arch>/target`.
- `make disk.img`: support-disk refresh only.
- `make clean-eval`: remove evaluator artifacts and build/replay state while
  keeping `.state/ltp-lab`.
- `make clean`: full local cleanup, including `.state`.

`make all` scrubs evaluator build/replay state before rebuilding:

- `.tmp`
- `.state/riscv64`
- `.state/loongarch64`
- `.state/oscomp-replay`

It also removes stale root-level evaluator artifacts before rebuilding. It keeps
`.state/ltp-lab` so that inventory, cached official images, generated lists, and
run records survive a clean evaluator build.

This means a fresh top-level build replaces stale replay workdirs, old arch build
state, and previous support-disk temp roots automatically without deleting lab
evidence. Use it for submission parity and clean confirmation; use the
high-frequency artifact commands for the inner edit/test loop.

The lower-level `make -C make disk_img` target also overwrites an existing disk image instead of warning and keeping the stale one.

## Local Replay

Main replay entrypoints:

- `./scripts/oscomp.sh list`
- `./scripts/oscomp.sh lab ...`
- `./scripts/oscomp.sh run --arch rv`
- `./scripts/oscomp.sh run --arch la`
- `./scripts/oscomp.sh verify --arch rv`
- `./scripts/oscomp.sh verify --arch la`
- `./scripts/replay-oscomp-eval.sh --arch rv`
- `./scripts/replay-oscomp-eval.sh --arch la`
- `make eval-rv`
- `make eval-la`
- `make replay-rv`
- `make replay-la`

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
- `/musl iperf`
- `/glibc iperf`
- `/glibc ltp`
- `/musl ltp`

The built-in full plan runs iperf before LTP so that long LTP coverage cannot starve the network throughput groups. LTP groups have default case budgets of 2400 seconds for glibc and 3000 seconds for musl (`OSCOMP_LTP_GROUP_BUDGET_SECS`, or the libc-specific `OSCOMP_LTP_GLIBC_GROUP_BUDGET_SECS` / `OSCOMP_LTP_MUSL_GROUP_BUDGET_SECS`), keeping the combined default LTP budget at the previous 5400-second ceiling while giving the later musl group more room; set the budget to `0` to disable that internal stop.

[`scripts/replay-oscomp-eval.sh`](/home/dia/TheKernel/scripts/replay-oscomp-eval.sh) replays the official pre-2025 flow with:

- official rootfs image as the first block device
- repository-built `disk.img` as the support disk
- contest QEMU shape for `rv` and `la`
- whole-QEMU timeout protection
- optional `--workdir` and `--keep-workdir`

Use `make eval-rv` or `make eval-la` when the selected kernel should be rebuilt
before replay. Use `make replay-rv` or `make replay-la` when the existing root
artifacts are known-current and should be reused.

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

The local framework supports focused replay instead of only full-matrix runs.
Use [`docs/ltp-lab.md`](./ltp-lab.md) for the current LTP experiment workflow.

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

The preferred high-level entrypoint is:

```bash
./scripts/oscomp.sh lab ...
```

It generates focused `ltp_test.txt` files, generates plan overrides, builds a
matching support disk, replays QEMU, parses logs, and stores results under
`.state/ltp-lab`.

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
- indexing official LTP payloads and source runtest entries
- storing focused LTP experiment results in `.state/ltp-lab`
- cleaning generated lab state without deleting evaluator artifacts
- keeping evaluator output format stable while still allowing explicit debug modes

What it does not try to do:

- emulate the old StarryOS showcase flow
- hide kernel failures behind timeout tuning
- make local replay independent from official image layout assumptions
