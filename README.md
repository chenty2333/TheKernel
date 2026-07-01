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
- AI-assisted commits are marked in Git history with
  `Co-Authored-By: Codex <noreply@openai.com>`.
- Source code license: Apache License 2.0, see [LICENSE](LICENSE) and
  [NOTICE](NOTICE).
- Technical documents, presentation slides, and other defense materials:
  Creative Commons Attribution-ShareAlike 4.0 International
  ([CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/)).

Third-party code under `third_party/` and vendored patch directories keeps its
upstream copyright notices and license terms.

## Development

Build, boot, and replay commands should run inside the repo development
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

Run one command inside the development shell without staying in it:

```bash
make dev-shell DEV_CMD='make kernels'
```

Open the privileged builder service when needed:

```bash
make dev-shell-root
```

## Build

Official evaluator artifact build, from the host:

```bash
make dev-shell DEV_CMD='make all'
```

The same build from inside an already-open `make dev-shell`:

```bash
make all
```

Both forms produce the evaluator artifacts at the repository root:

- `kernel-rv`
- `kernel-la`
- `disk.img`
- `disk-rv.img`
- `disk-la.img`

Local iteration can use narrower commands:

```bash
make dev-shell DEV_CMD='make kernels'
make dev-shell DEV_CMD='make artifacts'
make dev-shell DEV_CMD='make kernel-rv'
make dev-shell DEV_CMD='make kernel-la'
```

Inside an already-open `make dev-shell`, run those inner `make ...` commands
directly.

## Replay

Replay the evaluator flow from the host:

```bash
make dev-shell DEV_CMD='make replay-rv'
make dev-shell DEV_CMD='make replay-la'
```

Rebuild the matching kernel before replaying:

```bash
make dev-shell DEV_CMD='make eval-rv'
make dev-shell DEV_CMD='make eval-la'
```

Inside an already-open `make dev-shell`, run the inner commands directly:

```bash
make replay-rv
make replay-la
make eval-rv
make eval-la
```

Validate official image layout inside the dev shell:

```bash
./scripts/oscomp.sh verify --arch rv
./scripts/oscomp.sh verify --arch la
```

## Offline Local Scoring

Existing replay or lab console logs can be parsed, judged with the vendored
official-compatible judge scripts, scored, and reported without starting QEMU:

```bash
./scripts/oscomp.sh validate-output --log .state/path/to/qemu.log --arch rv
./scripts/oscomp.sh judge-log --arch rv --log .state/path/to/qemu.log --out .state/oscomp-eval/runs/manual-rv/rv
./scripts/oscomp.sh score-logs \
  --rv-log .state/path/to/rv.log \
  --la-log .state/path/to/la.log \
  --name manual-score
./scripts/oscomp.sh report-run .state/oscomp-eval/runs/manual-score
./scripts/oscomp.sh inspect-run --json .state/oscomp-eval/runs/manual-score
```

`scripts/validate-oscomp-output.py` remains as a compatibility shim for old
local scripts, but new usage should go through `scripts/oscomp.sh
validate-output`. For direct parser debugging, use `python3 -m
tools.oscomp_eval markers`.

`scripts/oscomp.sh evaluate --rv-log PATH --la-log PATH --name NAME` is the
same offline path under the new evaluation entrypoint. `scripts/oscomp.sh
evaluate --arch rv|la|both --name NAME` launches replay through the existing
`scripts/replay-oscomp-eval.sh` runner and then judges, scores, and reports the
captured logs. Replay mode accepts `--idle-timeout SECS`; when a QEMU run stops
writing console output for that long, the evaluator kills that replay, records a
structured timeout, and still writes the run artifacts that can be recovered.

Before a replay-backed run, `scripts/oscomp.sh support-check --arch rv --image
disk-rv.img` and the matching LA command validate that the support disk contains
the current `src/init.sh` and guest-side timeout helper used to keep LTP cases
bounded. `make check-eval-artifacts` runs those checks after confirming the
kernel and disk artifacts exist.

Every scored run writes `manifest.json`, `score.json`, a plain Markdown
`report.md`, and a machine-readable `artifact-index.json` that lists the run
artifacts that actually exist. The local evaluator does not generate an HTML
report.
Use `inspect-run --json` to check those artifacts without mutating the run
directory.
When `--replace` is used, stale evaluator artifacts in that run directory are
cleared before writing the new run; unrelated files are left alone.

Replay-backed evaluation can also build a run-local support image from an
explicit LTP list:

```bash
./scripts/oscomp.sh evaluate \
  --arch rv \
  --ltp-list .state/ltp-lab/candidates/ltp_test.txt \
  --name ltp-list-rv
```

`--ltp-list` is replay-only and is mutually exclusive with `--support-image`,
because an already-built support image already contains its own
`/meta/ltp_test.txt`.

Refresh the vendored official judge snapshot from an explicit local checkout:

```bash
./scripts/oscomp.sh official-refresh \
  --source /home/ava/Desktop/autotest-for-oskernel
```

This only imports `kernel/judge` scripts and provenance. It does not fetch from
the network or adopt the official Docker/QEMU controller.

Focused runs that intentionally use a reduced guest plan should pass the same
plan file to scoring so missing full-matrix groups are not treated as failures:

```bash
./scripts/oscomp.sh evaluate \
  --arch rv \
  --support-image .tmp/focused-rv-support.img \
  --plan .tmp/focused-plan.txt \
  --name focused-rv
```

Local evaluator exit codes are stable enough for scripts:

- `0`: command completed without score-facing issues.
- `1`: command completed and wrote artifacts, but validation, judging, scoring,
  or replay status found issues.
- `2`: invalid command line, missing input, or unsupported configuration.
- `3`: infrastructure failure such as missing QEMU, image, runner, or toolchain.
- `4`: internal evaluator error with traceback written to stderr.
- `124`: replay timeout.
- `130`: interrupted by the user.

## Boot

Boot an interactive local shell instead of the evaluator replay plan, from the
host:

```bash
make dev-shell DEV_CMD='make boot-rv'
make dev-shell DEV_CMD='make boot-la'
```

These targets build the matching kernel and a local support disk that sets
`OSCOMP_BOOT_SHELL=1`. That boot-only variable makes `src/init.sh` enter a
BusyBox shell; evaluator builds and replay runs do not set it.

Inside an already-open `make dev-shell`, run `make boot-rv` or `make boot-la`
directly. Exit the guest shell with `exit`; the kernel then powers off.

## Notes

- Kernel behavior lives mainly under `kernel/`.
- `src/init.sh` is guest-side runner logic.
- Runtime, replay, and lab state live under `.state`.
- OpenSpec changes should hold task-specific plans, investigations, and
  implementation notes.
