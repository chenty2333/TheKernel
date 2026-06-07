# Repository Map

This repository is the complete working tree for the OSComp scoring workflow:
kernel implementation, evaluator-compatible artifacts, Docker development
environment, local replay, and LTP experiment tooling live here.

Large external assets are intentionally kept out of git and are discovered by
scripts at runtime.

## Top-Level Contract

Remote evaluator entrypoint:

```bash
make all
```

Evaluator-facing artifacts:

- `kernel-rv`
- `kernel-la`
- `disk.img`
- `disk-la.img`

`kernel-rv` and `kernel-la` are ELF kernels. `disk.img` and `disk-la.img` are
optional support disks mounted as the second QEMU block device. They carry
runtime helpers, LTP subset metadata, plan overrides, and environment overrides.

## Kernel Code

- `kernel/`: Rust application crate used as the kernel app.
- `src/main.rs`: top-level application entry used by the build.
- `src/init.sh`: guest-side OSComp runner. It mounts the official image,
  applies support-disk payloads, runs the evaluation plan, normalizes LTP output,
  and shuts down.
- `kernel/src/syscall/`: Linux ABI syscall implementations.
- `kernel/src/file/`: VFS, file descriptors, special files, locks, pipes, and
  related file behavior.
- `kernel/src/mm/`: user address-space and memory-management implementation.
- `kernel/src/task/`: process, thread, signal, timer, credential, and accounting
  behavior.
- `kernel/src/pseudofs/`: `/proc`, `/dev`, and other pseudo filesystem support.
- `kernel/src/bpf/`: BPF compatibility used by some LTP cases.

Most LTP expansion work should map failures back into one of these subsystem
directories instead of adding case-specific behavior in the runner.

## Build System

- `Makefile`: evaluator-facing top-level build, dev-shell entrypoints, and LTP
  lab convenience targets.
- `make/`: lower-level ArceOS build and QEMU helpers.
- `make/platforms/`: platform configuration for RISC-V and LoongArch QEMU.
- `third_party/rust-patches/`: local patched Rust crates used by the kernel.
- `Cargo.toml` and `Cargo.lock`: workspace dependencies.

Build command roles:

- `make all`: clean evaluator build and remote-submission entrypoint.
- `make artifacts`: refresh all evaluator artifacts without `clean-eval`; it
  keeps arch Cargo target caches but rebuilds the support disk.
- `make kernels`: high-frequency build of both evaluator kernels only.
- `make kernel-rv` / `make kernel-la`: high-frequency single-kernel artifacts.
- `make disk.img`: support-disk refresh only.
- `make clean-eval`: remove root evaluator artifacts and build/replay state while
  preserving `.state/ltp-lab`.
- `make clean`: full local cleanup, including `.state`.

Remote submissions should rely on top-level `make all`. Day-to-day test-case
work should use the high-frequency commands unless a clean evaluator build is
needed to rule out stale build state.

## Development Environment

- `dev-env/Dockerfile`: pinned Docker image for local development.
- `dev-env/compose.yaml`: mounts this repository at `/workspace` and official
  testsuite data at `/opt/oskernel/testsuites`.
- `dev-env/check-image.sh`: validates Rust, QEMU, cross compilers, and disk
  tools.
- `scripts/dev-shell.sh`: opens or executes commands inside the Docker dev
  environment.

Use either:

```bash
make dev-shell
```

or non-interactively:

```bash
make dev-shell DEV_CMD="./scripts/oscomp.sh lab audit"
```

## OSComp Scripts

- `scripts/oscomp.sh`: user-facing dispatcher for `list`, `run`, `verify`, and
  `lab`.
- `scripts/replay-oscomp-eval.sh`: boots official images under the contest QEMU
  shape.
- `scripts/verify-pre2025-layout.sh`: checks the official image layout.
- `scripts/build-oscomp-support-disk.sh`: builds the support disk consumed by
  the guest runner.
- `scripts/ltp-lab.py`: LTP inventory, focused list generation, focused replay,
  parsing, promotion, cleanup, and audit tooling.
- `scripts/support-tools/`: small helper binaries built into the support disk.
- `scripts/support-overlay/`: runtime overlays and group overrides copied into
  the support disk.

## LTP State

- `ltp_test.txt`: repository default LTP subset shipped in evaluator support
  disks.
- `.state/ltp-lab/inventory.json`: generated inventory of official image
  contents, source runtest entries, and current subset resolution.
- `.state/ltp-lab/lists/`: generated candidate LTP lists.
- `.state/ltp-lab/plans/`: generated focused evaluation plans.
- `.state/ltp-lab/runs/`: per-run logs, parsed case records, summaries, support
  images, and QEMU workdirs.

`.state/ltp-lab` is local and ignored by git. Recreate it with:

```bash
make lab-inventory
```

## External Assets

Official pre-2025 images are required for replay:

- `sdcard-rv.img` or `sdcard-rv.img.xz`
- `sdcard-la.img` or `sdcard-la.img.xz`

Search order is encoded in `scripts/replay-oscomp-eval.sh` and
`scripts/ltp-lab.py`:

- `$OSCOMP_TESTSUITE_DIR`
- `/home/dia/kernel-image`
- `$HOME/kernel-image`
- `$HOME/testsuits-for-oskernel`
- `/coursegrader/testdata`

Reference source checkouts are optional and should stay under `.state/ltp-lab/refs`
or another ignored path:

- Linux source for behavior reference
- `testsuits-for-oskernel` source for test source and runtest metadata

## Current Documentation

- `README.md`: quick start.
- `docs/oscomp-local.md`: local Docker, build, replay, and support-disk workflow.
- `docs/ltp-lab.md`: LTP experiment framework.
- `docs/score-tracking.md`: score evidence snapshots and update rules.
- `docs/repo-map.md`: this repository map.

Removed stale documentation:

- `docs/x11.md` was removed because it described an old StarryOS GUI workflow
  using obsolete `make img` / `GRAPHIC=y` commands.

## Cleanup

Clean generated lab state and legacy root score artifacts:

```bash
make lab-clean
```

Preview cleanup targets before removing them:

```bash
./scripts/oscomp.sh lab clean --generated --cache --dry-run
```

Audit stale state:

```bash
./scripts/oscomp.sh lab audit
```

Root-level `rv_.out`, `la_.out`, and `score.txt` are treated as stale legacy
score artifacts. New experiment logs belong under `.state/ltp-lab/runs/`.
