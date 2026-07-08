# TheKernel

TheKernel is an OSComp 2026 operating-system design entry: a Rust-based
kernel for the RISC-V and LoongArch evaluator targets, with Linux ABI
compatibility for the official test suite.

## OSComp 2026 Documents

- Technical report: [docs/oscomp2026_report.pdf](docs/oscomp2026_report.pdf)
- Presentation slides: [docs/oscomp2026_slides.pdf](docs/oscomp2026_slides.pdf)

## Project

- Author: 陈天意 <hi@tychen.cc>
- Upstream baseline: StarryOS commit
  [`2e075accf4fb0aefdd1d252ebd9ccf29727d9923`](https://github.com/Starry-OS/StarryOS/tree/2e075accf4fb0aefdd1d252ebd9ccf29727d9923).
- Source code license: Apache License 2.0, see [LICENSE](LICENSE) and
  [NOTICE](NOTICE).
- Technical documents, presentation slides, and other defense materials:
  Creative Commons Attribution-ShareAlike 4.0 International
  ([CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/)).

Third-party code under `third_party/` and vendored patch directories keeps its
upstream copyright notices and license terms.

## Environment

Build, boot, replay, and smoke commands should run inside the repo development
container. The host may not have the RISC-V or LoongArch cross toolchains; a
host-side build failure such as `no usable cross toolchain found for
architecture riscv64` means the command was run outside the dev shell.

Build the development container image once:

```bash
make dev-image
```

Check the toolchain contract inside the image:

```bash
make dev-check
```

Open a development shell:

```bash
make dev-shell
```

Run a repository command inside the container:

```bash
make dev-shell DEV_CMD='make kernels'
```

Open the privileged builder service:

```bash
make dev-shell-root
```

## Build

Build evaluator artifacts from the host:

```bash
make dev-shell DEV_CMD='make all'
```

Build from inside an already-open `make dev-shell`:

```bash
make all
```

Both forms produce the evaluator artifacts at the repository root:

- `kernel-rv`
- `kernel-la`
- `disk.img` (RISC-V support disk)
- `disk-la.img`

RISC-V submissions use `disk.img` locally. Some remote logs show `disk-rv.img`;
that is the same support-disk role with a different filename on the grader side.

High-frequency kernel rebuilds keep Cargo target caches under
`.state/<arch>/target`. Kernel and support-disk outputs also use content keys in
`.state/kernel-cache/` and `.state/support-disk/`.

Build only the kernel artifacts:

```bash
make dev-shell DEV_CMD='make kernels'
make dev-shell DEV_CMD='make kernel-rv'
make dev-shell DEV_CMD='make kernel-la'
```

Inside `make dev-shell`, run the inner `make ...` commands directly.

## Local Evaluator

The local evaluator is centered on `tools/oscomp_eval/replay.py`.

| Entry | Purpose |
| --- | --- |
| `make replay-rv` / `make replay-la` | Build artifacts, run QEMU, judge, score |
| `python3 -m tools.oscomp_eval.replay replay` | Same pipeline with explicit CLI flags |
| `python3 -m tools.oscomp_eval.cli evaluate` | Compatibility alias for replay launch or offline log scoring |
| `scripts/oscomp.sh score-logs` | Offline scoring from existing console logs |
| `scripts/lab` | Focused replay for one group or LTP case |
| `scripts/ltp-lab.py` via `scripts/oscomp.sh ltp-lab` | LTP campaign inventory, replay, and cleanup |

Default replay timeouts live in `tools/oscomp_eval/config.py`:

- `REPLAY_TIMEOUT_FULL_SECS = 7000` for full `make replay-*`
- `REPLAY_TIMEOUT_FOCUSED_SECS = 3600` for `scripts/lab run`
- `REPLAY_TIMEOUT_SMOKE_SECS = 240` for boot-shell smoke scripts

## Replay

Build the matching artifacts, run one architecture in QEMU, judge the console
log, and write the local score report:

```bash
make dev-shell DEV_CMD='make replay-rv'
make dev-shell DEV_CMD='make replay-la'
```

Inside `make dev-shell`:

```bash
make replay-rv
make replay-la
```

Each replay writes `score.json` and per-arch judge artifacts under
`.state/oscomp-eval/runs/`. `score.json` includes a top-level `run` object with
replay provenance: run name, mode, status, arches, timeout, image paths, and per-
arch QEMU metadata. There is no `report.md`, `manifest.json`, or artifact index.

Raw `.img` test images are attached with QEMU snapshot mode. The support disk is
attached read-only. Compressed `.gz` or `.xz` test images are decompressed once
into `.state/oscomp-image-cache/` and reused by later replays. `make clean`
keeps the image cache; `make clean-all` removes all `.state` data.

Running both architectures through `python3 -m tools.oscomp_eval.cli evaluate
--arch both` launches RV and LA replays in parallel when `--fail-fast` is not
set.

Validate official image layout inside the dev shell:

```bash
./scripts/oscomp.sh verify --arch rv
./scripts/oscomp.sh verify --arch la
```

## Local Scoring

Replay and lab console logs can be parsed, judged, and scored without starting
QEMU:

```bash
./scripts/oscomp.sh validate-output --log .state/path/to/qemu.log --arch rv
./scripts/oscomp.sh judge-log --arch rv --log .state/path/to/qemu.log --out .state/oscomp-eval/runs/manual-rv/rv
./scripts/oscomp.sh score-logs \
  --rv-log .state/path/to/rv.log \
  --la-log .state/path/to/la.log \
  --name manual-score
./scripts/oscomp.sh inspect-run --json .state/oscomp-eval/runs/manual-score
```

For direct parser debugging, use `python3 -m tools.oscomp_eval markers`.

Support disks can be checked explicitly:

```bash
./scripts/oscomp.sh support-check --arch rv --image disk.img
./scripts/oscomp.sh support-check --arch la --image disk-la.img
```

Every scored run writes `score.json` and the per-arch marker/judge artifacts.
Use `inspect-run --json` to check those artifacts without mutating the run
directory.

Replay can build a support image from an explicit LTP list:

```bash
python3 -m tools.oscomp_eval.replay replay \
  --arch rv \
  --ltp-list .state/ltp-lab/candidates/ltp_test.txt \
  --name ltp-list-rv
```

Refresh the vendored official judge snapshot from an explicit local checkout:

```bash
./scripts/oscomp.sh official-refresh \
  --source /home/ava/Desktop/autotest-for-oskernel
```

Focused replays can pass an explicit group plan:

```bash
python3 -m tools.oscomp_eval.replay replay \
  --arch rv \
  --support-image .tmp/focused-rv-support.img \
  --plan .tmp/focused-plan.txt \
  --name focused-rv
```

## Focused Lab

Use `scripts/lab` for focused replay runs. It generates a guest plan, optional
case filter payload, and support image under `.state/oscomp-lab/`, then uses the
same replay, judge, and score path as `make replay-rv` and `make replay-la`.

```bash
make dev-shell DEV_CMD='./scripts/lab list'
make dev-shell DEV_CMD='./scripts/lab explain --arch rv --select ltp-glibc:openat01'
make dev-shell DEV_CMD='./scripts/lab run --arch rv --select ltp-glibc:openat01'
make dev-shell DEV_CMD='./scripts/lab run --arch rv --select basic-musl'
```

Selectors use `GROUP-LIBC[:EXPR]`. `ltp` supports exact case names,
`prefix=...`, and `regex=...`. Other groups run at group level.

Use `scripts/oscomp.sh ltp-lab` for broader LTP campaign workflows. That path is
separate from `scripts/lab` and keeps state under `.state/ltp-lab/`.

Exit codes:

- `0`: command completed without score-facing issues.
- `1`: command completed and wrote artifacts, but validation, judging, scoring,
  or replay status found issues.
- `2`: invalid command line, missing input, or unsupported configuration.
- `3`: infrastructure failure such as missing QEMU, image, runner, or toolchain.
- `4`: internal evaluator error with traceback written to stderr.
- `124`: replay timeout.
- `130`: interrupted by the user.

## Boot

Boot an interactive local shell:

```bash
make dev-shell DEV_CMD='make shell-rv'
make dev-shell DEV_CMD='make shell-la'
```

These targets build a `boot-shell` kernel (`kernel-*-shell`) that injects
`OSCOMP_BOOT_SHELL=1` at compile time. The shell uses the official test image
as the first disk so `/musl/busybox` and the usual userland layout are present.
The image is attached with QEMU snapshot mode, so the shell does not rewrite or
copy the test image.

Inside `make dev-shell`, run `make shell-rv` or `make shell-la` directly. Exit
the guest shell with `exit`; the kernel then powers off.

## Smoke

Targeted smoke checks are available through the smoke dispatcher:

```bash
make dev-shell DEV_CMD='./scripts/smoke.sh list'
make dev-shell DEV_CMD='./scripts/smoke.sh lwext4-io-boost --arch rv'
```

Boot-shell smokes share helpers in `scripts/smoke/lib.sh`. They build or reuse
`kernel-rv-shell` / `kernel-la-shell`, attach a support disk for guest tools,
and feed scripted commands into `python3 -m tools.oscomp_eval.replay qemu
--interactive`. They do not rely on `OSCOMP_BOOT_SHELL` env overrides baked
into the support disk.

`phase9-la-depth-gate` is different: it exercises the eval `kernel-la` path for
LoongArch async-depth gates.

## Tests

Run the local evaluator unit tests inside the dev shell or on the host:

```bash
PYTHONPATH=. python3 -m unittest discover -s tests/oscomp_eval -v
```

## Notes

- Kernel behavior lives mainly under `kernel/`.
- `src/init.sh` is guest-side runner logic embedded into the kernel image.
- Runtime, replay, lab, and smoke state live under `.state`.