# LTP Lab

`scripts/ltp-lab.py` is the local experiment harness for expanding OSComp LTP
coverage without changing the evaluator-facing `make all` contract.

The harness keeps kernel code, support-disk generation, focused replay, parsing,
and experiment records inside this repository. Large external artifacts are not
committed:

- official images: `sdcard-rv.img[.xz]`, `sdcard-la.img[.xz]`
- optional reference sources: Linux, `testsuits-for-oskernel`

## Layout

Repository-owned entrypoints:

- `scripts/ltp-lab.py`
- `scripts/lab`
- `scripts/oscomp.sh lab ...` for legacy forwarding
- `make lab-check`
- `make lab-bootstrap`
- `make lab-inventory`
- `make lab-new`
- `make lab-run`
- `make lab-review`
- `make lab-apply`
- `make lab-done`
- `make lab-trim`
- `make lab-clean`

Generated local state:

- `.state/ltp-lab/images/` caches decompressed official images
- `.state/ltp-lab/inventory.json` records image, runtest, and current-list facts
- `.state/ltp-lab/lists/` stores generated `ltp_test.txt` variants
- `.state/ltp-lab/plans/` stores focused evaluator plans
- `.state/ltp-lab/runs/<run-id>/` stores replay logs, parsed cases, summaries, and sometimes disposable support images/QEMU workdirs
- `.state/ltp-lab/campaigns/<name>/` stores fixed candidate batches, semantic prompts, implementation ledgers, static or observed taxonomy, and validation/promotion outputs when present
- `.state/ltp-lab/refs/linux` stores the optional Linux behavior and ABI reference tree
- `.state/ltp-lab/refs/testsuits-for-oskernel` stores optional testcase source and runtest metadata

All of `.state` is ignored by git. Recreate it on a new machine with the commands
below.

## Bootstrap

Check local tools and official image discovery:

```bash
make lab-check
```

Audit repository lab state and stale local artifacts:

```bash
./scripts/lab audit
```

The host may not have both QEMU binaries. The normal development path is to run
the same commands inside `make dev-shell`, where the Docker image contract should
provide the evaluator toolchain.

Optional references:

```bash
make lab-bootstrap
```

`--fetch` clones missing references shallowly. The standard Linux reference path
is `.state/ltp-lab/refs/linux`; a no-history Linux source tree from a release
tarball is also fine there. Use Linux as a behavior reference. The standard
testcase source path is `.state/ltp-lab/refs/testsuits-for-oskernel`.

When running inside Docker and the official images are mounted at
`/opt/oskernel/testsuites`, keep the testsuite source metadata inside the
repository state so both image and runtest data are visible to the container:

```bash
make lab-bootstrap
make lab-inventory
```

`dev-env/entrypoint.sh` does not recursively chown `.state` by default. This
keeps large cached images, baseline workdirs, and run logs from making every
`dev-shell` startup slow. If a historical root-owned `.state` subtree must be
repaired, run one explicit Docker shell with:

```bash
THEKERNEL_DEV_RECURSIVE_CHOWN_STATE=y ./scripts/dev-shell.sh -- true
```

`inventory` searches for `testsuits-for-oskernel` runtest metadata in this order:

- explicit `--testsuite-source`
- `~/testsuits-for-oskernel`
- `.state/ltp-lab/refs/testsuits-for-oskernel`
- `$OSCOMP_TESTSUITE_DIR`

## Inventory

Build or refresh the canonical experiment inventory:

```bash
make lab-inventory
```

The first run decompresses the official images into `.state/ltp-lab/images/`.
The inventory records:

- official image paths and cached plain images
- `/glibc` and `/musl` LTP packaged file counts per arch
- source `runtest` entries from `testsuits-for-oskernel`
- current `ltp_test.txt` entries and whether they resolve on all four combos

Print the cached summary without rebuilding it:

```bash
./scripts/lab summary
```

If `ltp_test.txt`, official images, runtest metadata, or support-disk plumbing
changed, run `make lab-inventory` before using the summary as current evidence.

## Campaign Workflow

Campaigns are the preferred workflow for real LTP expansion. They are fixed
case ledgers first and validation records second: create a broad batch, use it
to drive shared kernel implementation, then validate the batch after a
meaningful code pass.

Create a broad 100-150 case batch for a subsystem:

```bash
make lab-new NAME=goal3-fs-vfs-0001 SUITE="fs syscalls"
```

`SUITE` accepts runtest names or presets such as `fs`, `vfs`, `process`, `mm`,
`ipc`, `tty`, and `net`. The default batch size is 120 unopened cases. Use
`LIMIT=N` or `OFFSET=N` only when the batch shape needs to be adjusted.

The campaign directory contains:

```text
.state/ltp-lab/campaigns/<name>/
  manifest.json
  README.md
  candidates.txt
  cases.jsonl
  semantics/
    fs-open-permission.md
    fs-link-rename-unlink.md
    ...
  implementation.md
  taxonomy.md
```

### Code-First Phase

Read `cases.jsonl` and `semantics/*.md` before editing kernel code. Each card
lists selected cases, test source pointers, Linux reference paths, expected
semantics, and local kernel paths to inspect. Then inspect testcase sources,
Linux reference code, and local kernel code, and implement shared kernel
behavior before spending time on full replay.

Use `implementation.md` to record changed files, implemented semantics,
testcase/Linux cross-checks, expected candidate coverage, and deferred
validation groups. Before replay, use `taxonomy.md` as the static semantic map
for unresolved buckets. Do not add or remove candidates inside a campaign after
implementation starts; create another campaign for the next batch.

Cheap compiler checks and tiny crash probes are fine during this phase.
Repeated small pre-fix matrices should not be the main loop.

### Validation Phase

Run the campaign through Docker only after a meaningful implementation pass and
a kernel build:

```bash
make kernels
make lab-run NAME=goal3-fs-vfs-0001
```

Run these inside `make dev-shell`; use `make dev-shell DEV_CMD='...'` only for
one-off host-side invocation.

Analyze results and inspect promotion eligibility:

```bash
make lab-review NAME=goal3-fs-vfs-0001
```

`make lab-review` writes:

- `analysis.json`: per-case status matrix, semantic bucket, and promotion flag
- `taxonomy.md`: observed pass/fail/panic/timeout taxonomy, replacing the static map
- `promotable.txt`: candidate lines with all required pass evidence

### Promotion And Cleanup

Apply promotions only after reviewing the generated list:

```bash
make lab-apply NAME=goal3-fs-vfs-0001
```

`make lab-review` only analyzes evidence and prints dry-run promotion status.
`make lab-apply` is the deliberate step that updates root `ltp_test.txt`.

Finish a campaign after the batch is handled:

```bash
make lab-done NAME=goal3-fs-vfs-0001
```

Finish keeps compact evidence (`console.log`, `cases.jsonl`, `summary.json`,
`combined-summary.json`, campaign metadata, semantic cards, taxonomy, and
promotion outputs) while removing heavy per-run `support.img` and QEMU `work/`
directories. Use `--no-clean` for forensic runs and `--dry-run` to preview.

Use direct `lab generate`, `lab replay`, `lab failures`, and `lab promote` for
small debugging runs, missing-combo repair, or one-off evidence checks. Use the
short campaign commands for the main 50-150 case expansion path.

## Lower-Level Lab Commands

Use these only for debugging, missing-combo repair, or one-off evidence checks.
Main LTP expansion should stay campaign-based.

Common entries:

```bash
make lab-list LAB_ARGS="--runtest syscalls --limit 50 --name syscalls-0001"
make lab-replay LAB_ARGS="--name rv-syscalls-0001 --arch rv --test-list .state/ltp-lab/lists/syscalls-0001.txt"
./scripts/lab summarize .state/ltp-lab/runs/<run>
```

`lab replay` writes the focused list, plan, support image, console logs,
`cases.jsonl`, summaries, and aggregate evidence under `.state/ltp-lab/runs/<run>/`.
If replay fails before LTP starts, treat `cases=0` with a nonzero
`replay_exit[...]` as infrastructure or kernel-artifact failure, not as an empty
LTP result.

For unusual filters, budgets, missing-combo repair, or single-arch replay, use
`./scripts/lab replay --help` or `make lab-replay LAB_ARGS="..."`.

Promotion requires real parser `pass` on the required combo set. The default
requirement is all four `rv/glibc`, `rv/musl`, `la/glibc`, and `la/musl` combos.
`silent-pass` and TCONF-only cases are not promoted unless `--allow-silent-pass`
is passed explicitly. Review promoted lists before replacing root
`ltp_test.txt`.

## Reorder For Evaluator Budget

Local parallelism speeds development, but the official evaluator still runs its
fixed flow under a two-hour wall clock. Use timing evidence to produce a reviewed
candidate order:

```bash
./scripts/lab reorder \
  --base ltp_test.txt \
  --evidence .state/ltp-lab/runs/matrix-syscalls-0001 \
  --output .state/ltp-lab/lists/reordered.txt
```

The command keeps stable fast passes earlier, pushes known timeout/panic cases
later, and writes a new list instead of editing root `ltp_test.txt`.

## Cleanup

Use short cleanup targets by default:

```bash
make lab-trim
make lab-clean
```

- `make lab-trim`: daily cleanup for disposable state after campaign evidence has
  been analyzed or finished. It removes failed or zero-case lab runs, per-run
  `support.img`, QEMU `work/`, smoke leftovers, baseline replay image copies,
  and old root-level score artifacts. It keeps campaigns, cached official
  images, and reference checkouts.
- `make lab-clean`: stronger generated-state cleanup for run/list/plan state and
  stale root score artifacts.

Both short targets keep cached official image decompressions and optional
reference checkouts. Baseline logs stay in `.state/baseline/`, but copied
baseline `sdcard-*.img` and `disk*.img` files are removable heavy artifacts.
Remove caches or refs explicitly only under disk pressure:

```bash
./scripts/lab clean cache --dry-run
./scripts/lab clean refs --dry-run
```

Useful cleanup modes:

```bash
# Preview daily cleanup.
./scripts/lab clean trim --dry-run

# Preview generated run/list/plan removals.
./scripts/lab clean generated --dry-run

# Preview finish analysis and heavy per-run artifact cleanup.
make lab-done NAME=goal3-fs-vfs-0001 LAB_ARGS="--dry-run"

# Keep only the newest 10 run directories.
./scripts/lab clean --runs --keep-runs 10

# Remove the whole lab tree, cache, refs, inventory, and legacy root outputs.
./scripts/lab clean all --dry-run
```

`./scripts/lab clean legacy-root` removes old root-level `rv_.out`,
`la_.out`, and `score.txt` files without removing evaluator artifacts such as
`kernel-rv`, `kernel-la`, `disk.img`, or `disk-la.img`. The Makefile
`legacy-clean` target is broader and is part of evaluator artifact cleanup.

## Deferred Validation Notes

`docs/deferred-validation-*.md` files are simple pending-validation lists for
code-first work. They are not generated lab state, are not removed by
`lab-trim` or `lab-clean`, and are not pass evidence.

Keep only syscall/surface names, short caveats, and replay batches. Do not add
`Cheap Checks Recorded`, `Cheap Verification Performed`, raw command logs, or
format/build-check records there.
