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
make dev-shell DEV_CMD="./scripts/lab audit"
```

`make dev-shell-root` starts the privileged `builder` compose service for cases
that need extra container capabilities. The entrypoint still runs commands as
the local user id, not as a persistent root-owned workspace shell.

## Build Artifacts

Evaluator-facing artifacts are:

- `kernel-rv`
- `kernel-la`
- `disk.img`
- `disk-la.img`

Main commands:

```bash
make dev-shell DEV_CMD='make all'
make dev-shell DEV_CMD='make artifacts'
make dev-shell DEV_CMD='make kernels'
```

Inside an already-open `make dev-shell`, run `make all`, `make artifacts`,
`make kernels`, `make kernel-rv`, `make kernel-la`, `make disk.img`, or
`make disk-la.img` directly.

Command intent:

- `make all`: clean evaluator build. This is the remote-submission contract and
  final local confirmation path, not the normal edit/test loop.
- `make artifacts`: refresh all evaluator artifacts without `clean-eval`; it
  keeps arch Cargo target caches but rebuilds the support disk.
- `make kernels`: high-frequency build of both evaluator kernels only.
- `make kernel-rv` and `make kernel-la`: high-frequency single-kernel artifacts
  that preserve `.state/<arch>/target`.
- `make disk.img`: support-disk refresh only; `make disk-la.img` copies it for
  the LA artifact name.
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

The lower-level `make -C make disk_img` target also overwrites an existing disk
image instead of warning and keeping the stale one.

## Local Replay

Main replay entrypoints:

- `./scripts/oscomp.sh list`
- `./scripts/lab ...`
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

[`scripts/oscomp.sh`](/home/dia/TheKernel/scripts/oscomp.sh) exposes the fixed
reference plan with `./scripts/oscomp.sh list`. The built-in full plan runs
iperf before LTP so long LTP coverage cannot starve network throughput groups.
LTP groups default to bounded libc-specific budgets; set the budget to `0` to
disable the internal stop.

[`scripts/replay-oscomp-eval.sh`](/home/dia/TheKernel/scripts/replay-oscomp-eval.sh) replays the official pre-2025 flow with:

- official rootfs image as the first block device
- repository-built `disk.img` or `disk-la.img` as the support disk
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

Current payload includes `ltp_test.txt`, optional plan/env overrides, locale and
libgcc support, and per-arch overlay tools/libraries under `/<arch>/overlay`.

At boot, [`src/init.sh`](/home/dia/TheKernel/src/init.sh) mounts the support disk read-only, copies what it needs into the rootfs, then unmounts `/support` early.

## Focused Replay And Overrides

The local framework supports focused replay instead of only full-matrix runs.
Use [`docs/ltp-lab.md`](./ltp-lab.md) for the current LTP experiment workflow.
For LTP expansion work, prefer the short campaign targets:

```bash
make lab-new NAME=goal3-fs-vfs-0001 SUITE="fs syscalls"
make lab-run NAME=goal3-fs-vfs-0001
make lab-review NAME=goal3-fs-vfs-0001
make lab-apply NAME=goal3-fs-vfs-0001
make lab-done NAME=goal3-fs-vfs-0001
```

Campaigns keep fixed 50-150 case batches, semantic prompts, implementation
notes, deferred validation state, run evidence, promotion outputs, and
self-cleaning records under `.state/ltp-lab/campaigns`. For code-first goals,
use the campaign as a case ledger and semantic work queue before running replay.

For broad code-first work outside one campaign, use `docs/deferred-validation-*.md`
as a simple pending-validation list. Keep only syscall/surface names, short
caveats, and replay batches there; do not use it as pass evidence. Do not add
cheap-check sections or format/build-check logs.

Support-disk build options:

- `--test-list PATH`
- `--plan-override PATH`

Top-level support-disk build also accepts:

```bash
OSCOMP_PLAN_OVERRIDE=/abs/path/plan.txt make disk.img
```

This allows focused LTP subsets, custom evaluation plans, and targeted
regression checks without editing the default guest runner.

## Cleanup

Use the short cleanup targets by intent. `docs/ltp-lab.md` has the detailed
cleanup flag reference.

- `make lab-trim`: daily disposable-state cleanup after campaign evidence has
  been analyzed or finished. It keeps campaigns, cached official images, and
  reference checkouts.
- `make lab-clean`: stronger generated run/list/plan cleanup. It also keeps
  cached official images and reference checkouts.
- `make clean-eval`: evaluator-artifact cleanup before a clean local build.
- `make clean`: full local cleanup, including `.state`.

Preview before removing generated state:

```bash
./scripts/lab clean trim --dry-run
./scripts/lab audit
```

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
- injecting support payloads through `disk.img` / `disk-la.img`
- running full or focused LTP subsets
- indexing official LTP payloads and source runtest entries
- storing focused LTP experiment results in `.state/ltp-lab`
- cleaning generated lab state without deleting evaluator artifacts
- keeping evaluator output format stable while still allowing explicit debug modes

What it does not try to do:

- emulate the old StarryOS showcase flow
- hide kernel failures behind timeout tuning
- make local replay independent from official image layout assumptions
