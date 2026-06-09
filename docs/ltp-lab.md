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
- `scripts/oscomp.sh lab ...`
- `make lab-check`
- `make lab-inventory`
- `make lab-campaign`
- `make lab-list`
- `make lab-plan`
- `make lab-clean`

Generated local state:

- `.state/ltp-lab/images/` caches decompressed official images
- `.state/ltp-lab/inventory.json` records image, runtest, and current-list facts
- `.state/ltp-lab/lists/` stores generated `ltp_test.txt` variants
- `.state/ltp-lab/plans/` stores focused evaluator plans
- `.state/ltp-lab/runs/<run-id>/` stores support images, replay logs, parsed cases, and summaries
- `.state/ltp-lab/campaigns/<name>/` stores fixed candidate batches, semantic prompts, implementation ledgers, analysis, taxonomy, and promotion outputs
- `.state/ltp-lab/refs/` is available for optional reference source trees

All of `.state` is ignored by git. Recreate it on a new machine with the commands
below.

## Bootstrap

Check local tools and official image discovery:

```bash
make lab-check
```

Audit repository lab state and stale local artifacts:

```bash
./scripts/oscomp.sh lab audit
```

The host may not have both QEMU binaries. The normal development path is to run
the same commands inside `make dev-shell`, where the Docker image contract should
provide the evaluator toolchain.

Optional references:

```bash
./scripts/oscomp.sh lab bootstrap \
  --linux-ref .state/ltp-lab/refs/linux \
  --testsuits-ref .state/ltp-lab/refs/testsuits-for-oskernel \
  --fetch
```

`--fetch` clones missing references shallowly. A no-history Linux source tree
from a release tarball is also fine under `.state/ltp-lab/refs/linux`. Use Linux
as a behavior reference only; do not copy code into this kernel.

When running inside Docker and the official images are mounted at
`/opt/oskernel/testsuites`, keep the testsuite source metadata inside the
repository state so both image and runtest data are visible to the container:

```bash
./scripts/oscomp.sh lab bootstrap \
  --testsuits-ref .state/ltp-lab/refs/testsuits-for-oskernel \
  --fetch
./scripts/oscomp.sh lab inventory \
  --image-root /opt/oskernel/testsuites \
  --testsuite-source .state/ltp-lab/refs/testsuits-for-oskernel
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
./scripts/oscomp.sh lab summary
```

If `ltp_test.txt`, official images, runtest metadata, or support-disk plumbing
changed, run `make lab-inventory` before using the summary as current evidence.

## Campaign Workflow

Campaigns are the preferred workflow for real LTP expansion. They keep each
large batch fixed, connect candidates to testcase sources and Linux reference
paths, and leave a compact audit trail for later agents.

Create a broad 100-150 case batch for a subsystem:

```bash
./scripts/oscomp.sh lab campaign create goal3-fs-vfs-0001 \
  --mode unopened-runtest \
  --runtest fs \
  --runtest syscalls \
  --limit 120 \
  --goal 'FS/VFS/file-IO semantic expansion'
```

Use `--include` filters for narrower debug campaigns. Before treating a filtered
campaign as a large batch, check the generated candidate count with
`campaign status`; heavily filtered FS/VFS patterns may produce only a handful of
currently unopened cases.

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

Read `semantics/*.md` before editing kernel code. Each card lists selected cases,
test source pointers, Linux reference paths, expected semantics, and local kernel
paths to inspect. Use `implementation.md` to record the semantic behavior you
implemented and the testcase/Linux cross-checks. Do not add or remove candidates
inside a campaign after implementation starts; create another campaign for the
next batch.

Run the campaign through Docker after building current kernels:

```bash
make kernels
make dev-shell DEV_CMD='./scripts/oscomp.sh lab campaign run goal3-fs-vfs-0001 --arch both --libc both --skip-kernel-build --case-timeout 90'
```

Analyze results and inspect promotion eligibility:

```bash
./scripts/oscomp.sh lab campaign analyze goal3-fs-vfs-0001
./scripts/oscomp.sh lab campaign promote goal3-fs-vfs-0001 --dry-run --explain
```

`campaign analyze` writes:

- `analysis.json`: per-case status matrix, semantic bucket, and promotion flag
- `taxonomy.md`: grouped failure taxonomy
- `promotable.txt`: candidate lines with all required pass evidence

Apply promotions only after reviewing the generated list:

```bash
./scripts/oscomp.sh lab campaign promote goal3-fs-vfs-0001 \
  --output .state/ltp-lab/campaigns/goal3-fs-vfs-0001/promoted-ltp_test.txt
```

If the list is correct, update root `ltp_test.txt` deliberately, usually with a
small reviewed patch or `campaign promote --apply-root` when the generated diff
has already been inspected.

Finish a campaign after the batch is handled:

```bash
./scripts/oscomp.sh lab campaign finish goal3-fs-vfs-0001
```

Finish keeps compact evidence (`console.log`, `cases.jsonl`, `summary.json`,
`combined-summary.json`, campaign metadata, semantic cards, taxonomy, and
promotion outputs) while removing heavy per-run `support.img` and QEMU `work/`
directories. Use `--no-clean` for forensic runs and `--dry-run` to preview.

Use direct `lab generate`, `lab run`, `lab failures`, and `lab promote` for
small debugging runs, missing-combo repair, or one-off evidence checks. Use
campaigns for the main 50-150 case expansion path.

## Direct List And Run Commands

Use these lower-level commands for debugging, missing-combo repair, or one-off
evidence checks. Main LTP expansion should use campaigns.

Generate an unopened batch:

```bash
./scripts/oscomp.sh lab generate \
  --mode unopened-runtest \
  --runtest syscalls \
  --limit 50 \
  --name syscalls-0001
```

Generate all unopened runtest entries available on the selected matrix:

```bash
./scripts/oscomp.sh lab generate --mode unopened-runtest --name unopened-all
```

Useful filters:

- `--arch rv`, `--arch la`, or `--arch both`
- `--libc glibc`, `--libc musl`, or `--libc both`
- `--runtest syscalls`
- `--include 'open*'`
- `--exclude '*stress*'`
- `--limit N`
- `--offset N`
- `--shuffle --seed N`

Generated lists are written under `.state/ltp-lab/lists/` unless `--output` is
provided.

Generate focused plans when needed:

```bash
./scripts/oscomp.sh lab plan --libc both --name ltp-both
```

Single-libc focused plans:

```bash
./scripts/oscomp.sh lab plan --libc glibc --name ltp-glibc
./scripts/oscomp.sh lab plan --libc musl --name ltp-musl
```

Prepare evaluator artifacts during normal iteration:

```bash
make kernels
make artifacts
```

Use `make all` only when a clean evaluator build is needed. It preserves
`.state/ltp-lab`, but it is slower than the high-frequency artifact targets.

Run a generated list, reusing existing kernels:

```bash
make dev-shell DEV_CMD='./scripts/oscomp.sh lab run --name rv-syscalls-0001 --arch rv --libc both --test-list .state/ltp-lab/lists/syscalls-0001.txt --skip-kernel-build'
```

`lab run` defaults to `--parallel arch --jobs auto`, so `--arch both` launches
RV and LA concurrently. For a stronger machine, use `--split-combos --jobs auto`
to split by `arch/libc` into up to four independent QEMU tasks.

Useful execution controls:

- `--parallel arch`: default; one task per selected arch.
- `--split-combos` or `--parallel combo`: one task per selected `arch/libc`.
- `--no-parallel`: serial replay for debugging.
- `--jobs auto`: use the selected task count.
- `--case-timeout SECS`: timeout one LTP case in the guest and continue.
- `--task-timeout SECS`: timeout one QEMU replay task.

The run command writes `ltp_test.txt`, `plan.txt`, support images, console logs,
`cases.jsonl`, `summary.json`, and `combined-summary.json` under
`.state/ltp-lab/runs/<run>/`. Split-combo runs keep per-combo logs and summaries
under `tasks/<arch-libc>/`, then aggregate evidence back to `rv/` and `la/`.
Evidence commands also fall back to `tasks/*/cases.jsonl` if an old or partial
split-combo run is missing the aggregate `rv/` or `la/` files.

If replay fails before LTP starts, the run still writes logs, exit codes, and
summaries, but `lab run` exits nonzero. Treat `cases=0` with a nonzero
`replay_exit[...]` as an infrastructure or kernel-artifact failure, not as an
empty LTP result.

Budget knobs are injected into the guest support disk:

```bash
--ltp-budget 0
--glibc-budget 2400
--musl-budget 3000
--case-timeout 60
--env KEY=VALUE
```

Parse, summarize, or inspect failures:

```bash
./scripts/oscomp.sh lab parse --arch rv --log .state/ltp-lab/runs/<run>/rv/console.log
```

Summarize a run:

```bash
./scripts/oscomp.sh lab summarize .state/ltp-lab/runs/<run>
```

Case records include:

- case marker
- arch and libc
- return code
- TPASS/TFAIL/TBROK/TCONF/TWARN counts
- summary counts
- duration in seconds when the guest emitted timing evidence
- timeout and panic flags
- status: `pass`, `silent-pass`, `fail`, `nonzero`, `timeout`, `panic`, or `incomplete`

Merge cases that passed the required combo set into a new list:

```bash
./scripts/oscomp.sh lab promote \
  .state/ltp-lab/runs/matrix-syscalls-0001 \
  --output .state/ltp-lab/lists/promoted.txt
```

By default promotion requires real parser `pass` on all four combos. Override
with `--require` for narrower experiments. Selectors accept exact combos such as
`rv/glibc`, equivalent spellings such as `rv-glibc`, whole arches such as `rv`,
whole libcs such as `glibc`, or `both` for the full matrix. `silent-pass` and
TCONF-only cases are not promoted unless `--allow-silent-pass` is passed
explicitly.

Promotion can merge evidence from multiple run directories. Use `--dry-run` and
`--explain` to see why a case is or is not promotable:

```bash
./scripts/oscomp.sh lab promote \
  .state/ltp-lab/runs/run-a \
  .state/ltp-lab/runs/run-b \
  --dry-run \
  --explain \
  --output .state/ltp-lab/lists/promoted.txt
```

Inspect a candidate list or generate rerun lists for missing/nonpassing combos:

```bash
./scripts/oscomp.sh lab matrix-status \
  .state/ltp-lab/runs/run-a \
  --test-list .state/ltp-lab/lists/candidates.txt

./scripts/oscomp.sh lab missing-combos \
  .state/ltp-lab/runs/run-a \
  .state/ltp-lab/runs/run-b \
  --test-list .state/ltp-lab/lists/candidates.txt \
  --output .state/ltp-lab/lists/missing
```

If a focused repair or missing-combo run was launched with `lab run` instead of
`lab campaign run`, attach it before campaign analysis so promotion and finish
use the same recorded evidence set:

```bash
./scripts/oscomp.sh lab campaign attach-run goal3-fs-vfs-0001 repair-run-0001 \
  --note "rv/musl missing-combo repair"
```

Review promoted lists before replacing the repository root `ltp_test.txt`.

## Reorder For Evaluator Budget

Local parallelism speeds development, but the official evaluator still runs its
fixed flow under a two-hour wall clock. Use timing evidence to produce a reviewed
candidate order:

```bash
./scripts/oscomp.sh lab reorder \
  --base ltp_test.txt \
  --evidence .state/ltp-lab/runs/matrix-syscalls-0001 \
  --output .state/ltp-lab/lists/reordered.txt
```

The command keeps stable fast passes earlier, pushes known timeout/panic cases
later, and writes a new list instead of editing root `ltp_test.txt`.

## Cleanup

Remove generated run/list/plan state and old root-level score artifacts:

```bash
make lab-clean
```

Pass extra cleanup flags through `LAB_CLEAN_ARGS`:

```bash
make lab-clean LAB_CLEAN_ARGS="--cache --dry-run"
```

Baseline replay directories under `.state/baseline/` are intentionally not part
of `make lab-clean`; remove old baseline runs manually after their evidence has
been superseded.

`make lab-clean` keeps cached official image decompressions and optional
reference checkouts. Remove those explicitly when needed:

```bash
./scripts/oscomp.sh lab clean --cache --dry-run
./scripts/oscomp.sh lab clean --refs --dry-run
```

Useful cleanup modes:

```bash
# Preview all generated run/list/plan removals.
./scripts/oscomp.sh lab clean --generated --dry-run

# Preview campaign removals.
./scripts/oscomp.sh lab clean --campaigns --dry-run

# Preview finish analysis and heavy per-run artifact cleanup.
./scripts/oscomp.sh lab campaign finish goal3-fs-vfs-0001 --dry-run

# Remove failed or zero-case runs after an experiment pass.
./scripts/oscomp.sh lab clean --failed-runs --empty-runs

# Keep only the newest 10 run directories.
./scripts/oscomp.sh lab clean --runs --keep-runs 10

# Remove large per-run QEMU workdirs while preserving console logs and parsed summaries.
./scripts/oscomp.sh lab clean --workdirs --dry-run

# Remove run support images older than one week while preserving logs.
./scripts/oscomp.sh lab clean --support-images --older-than 7d

# Remove stale smoke-named local experiments from older framework revisions.
./scripts/oscomp.sh lab clean --smoke

# Remove the whole lab tree, cache, refs, inventory, and legacy root outputs.
./scripts/oscomp.sh lab clean --all --dry-run
```

`./scripts/oscomp.sh lab clean --legacy-root` removes old root-level `rv_.out`,
`la_.out`, and `score.txt` files without removing evaluator artifacts such as
`kernel-rv`, `kernel-la`, `disk.img`, or `disk-la.img`. The Makefile
`legacy-clean` target is broader and is part of evaluator artifact cleanup.
