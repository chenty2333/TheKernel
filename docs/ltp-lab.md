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
- `make lab-list`
- `make lab-plan`
- `make lab-clean`

Generated local state:

- `.state/ltp-lab/images/` caches decompressed official images
- `.state/ltp-lab/inventory.json` records image, runtest, and current-list facts
- `.state/ltp-lab/lists/` stores generated `ltp_test.txt` variants
- `.state/ltp-lab/plans/` stores focused evaluator plans
- `.state/ltp-lab/runs/<run-id>/` stores support images, replay logs, parsed cases, and summaries
- `.state/ltp-lab/refs/` is available for optional shallow reference checkouts

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

Optional shallow references:

```bash
./scripts/oscomp.sh lab bootstrap \
  --linux-ref .state/ltp-lab/refs/linux \
  --testsuits-ref .state/ltp-lab/refs/testsuits-for-oskernel \
  --fetch
```

Use Linux as a behavior reference only. Do not copy code into this kernel.

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

Print the current summary without rebuilding it:

```bash
./scripts/oscomp.sh lab summary
```

## Generate Lists

Generate a small unopened syscall batch:

```bash
./scripts/oscomp.sh lab generate \
  --mode unopened-runtest \
  --runtest syscalls \
  --limit 50 \
  --name syscalls-0001
```

Generate all currently unopened runtest entries available on all selected
arch/libc combinations:

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

## Focused Plans

Generate an LTP-only plan:

```bash
./scripts/oscomp.sh lab plan --libc both --name ltp-both
```

Single-libc focused plans:

```bash
./scripts/oscomp.sh lab plan --libc glibc --name ltp-glibc
./scripts/oscomp.sh lab plan --libc musl --name ltp-musl
```

The guest runner still owns the output protocol. Plans only choose which official
groups run.

## Run Experiments

Prepare evaluator artifacts with the high-frequency path during normal
iteration:

```bash
make kernels
make artifacts
```

Use `make all` only when a clean evaluator build is needed. It preserves
`.state/ltp-lab`, but it is slower than the high-frequency artifact targets.
Use `make kernels` for kernel-only work and `make artifacts` when the support
disk should also be refreshed.

Run a generated list on RV only, reusing existing `kernel-rv`:

```bash
make dev-shell DEV_CMD='./scripts/oscomp.sh lab run --name rv-syscalls-0001 --arch rv --libc both --test-list .state/ltp-lab/lists/syscalls-0001.txt --skip-kernel-build'
```

Run a newly generated batch on both arches:

```bash
make dev-shell DEV_CMD='./scripts/oscomp.sh lab run --name matrix-syscalls-0001 --arch both --libc both --mode unopened-runtest --runtest syscalls --limit 50 --skip-kernel-build'
```

The run command:

1. writes `ltp_test.txt` and `plan.txt` into the run directory,
2. builds a support image with those files,
3. replays `rv` and/or `la` through `scripts/replay-oscomp-eval.sh`,
4. stores console logs and QEMU workdirs,
5. parses LTP case results into `cases.jsonl` and `summary.json`.

If replay fails before LTP starts, the run still writes logs, exit codes, and
summaries, but `lab run` exits nonzero. Treat `cases=0` with a nonzero
`replay_exit[...]` as an infrastructure or kernel-artifact failure, not as an
empty LTP result.

Budget knobs are injected into the guest support disk:

```bash
--ltp-budget 0
--glibc-budget 2400
--musl-budget 3000
--env KEY=VALUE
```

## Parse And Summarize

Parse an existing replay log:

```bash
./scripts/oscomp.sh lab parse --arch rv --log .state/ltp-lab/runs/<run>/rv/console.log
```

Summarize a run:

```bash
./scripts/oscomp.sh lab summarize .state/ltp-lab/runs/<run>
```

The combined summary records per-arch replay exit codes and failed arches.

Group failed or incomplete cases:

```bash
./scripts/oscomp.sh lab failures .state/ltp-lab/runs/<run>
```

Case records include:

- case marker
- arch and libc
- return code
- TPASS/TFAIL/TBROK/TCONF/TWARN counts
- summary counts
- timeout and panic flags
- status: `pass`, `silent-pass`, `fail`, `nonzero`, `timeout`, `panic`, or `incomplete`

## Promote Passing Cases

Merge cases that passed the required combo set into a new list:

```bash
./scripts/oscomp.sh lab promote \
  .state/ltp-lab/runs/matrix-syscalls-0001 \
  --output .state/ltp-lab/lists/promoted.txt
```

By default promotion requires all four combinations:

- `rv/glibc`
- `rv/musl`
- `la/glibc`
- `la/musl`

Override with `--require rv/glibc --require la/glibc` for narrower experiments.

Promotion uses real parser `pass` status by default. It does not promote
`silent-pass` or TCONF-only cases unless `--allow-silent-pass` is passed
explicitly.

Review promoted lists before replacing the repository root `ltp_test.txt`.

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

`make legacy-clean` removes old root-level `rv_.out`, `la_.out`, and `score.txt`
files. It does not remove evaluator artifacts such as `kernel-rv`, `kernel-la`,
`disk.img`, or `disk-la.img`.

## Scoring Strategy

Use this framework to grow LTP in controlled batches:

1. choose a category, usually `syscalls`, `fs`, `ipc`, `mm`, `sched`, or `signal`,
2. generate a small unopened batch,
3. run RV and LA with glibc/musl,
4. classify failures by kernel subsystem,
5. fix real kernel behavior,
6. promote stable passing cases.

For non-LTP scores, keep the same evidence discipline: optimize kernel fast paths
for the benchmark workloads, but preserve observable Linux ABI behavior so that
new LTP cases do not regress.
