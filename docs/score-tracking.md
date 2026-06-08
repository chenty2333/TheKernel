# Score Tracking

This file tracks score evidence used for planning. It is not an automatic
leaderboard mirror. Update it only when a new local replay, official submission,
or refreshed leaderboard snapshot is available.

## Snapshot 2026-06-08

Sources:

- user-provided leaderboard rows for the top two teams
- user-provided local score table for this repository

Use this snapshot to prioritize work, not to make final submission claims.

## Local Replay Baseline 2026-06-08

Current Docker replay evidence is recorded in
`docs/baseline-2026-06-08.md`.

Summary:

- clean Docker `make all` succeeded and produced `kernel-rv`, `kernel-la`,
  `disk.img`, and `disk-la.img`
- RV completed all non-LTP groups, entered `ltp-glibc`, then panicked at
  `ltp_test.txt:1016` (`fallocate02`)
- RV parsed LTP cases: pass=960, fail=41, nonzero=13, timeout=1, panic=1
- LA completed musl groups through `cyclictest-musl` and `basic-glibc`, then
  timed out in `iozone-glibc` stride-read before reaching LTP
- Docker inventory resolved all 1089 current LTP entries on all four
  arch/libc combinations
- unopened available runtest frontier: 2734 unique lines

Use this replay as the current local baseline until a newer Docker replay or
official submission replaces it.

## Local LTP Harvest 2026-06-08

Harvest evidence is recorded in `docs/ltp-harvest-2026-06-08.md`.

Summary:

- promoted 10 official unopened DIO cases with all-four `pass` evidence
- promoted cases: `dio16`, `dio17`, `dio18`, `dio19`, `dio20`, `dio21`,
  `dio26`, `dio27`, `dio28`, `dio29`
- post-promotion `ltp_test.txt`: 1099 entries, all resolving on all four
  arch/libc combinations
- remaining unopened available runtest frontier: 2724 unique lines
- attempted non-promoted taxonomy: command/toolchain failures, missing syscalls,
  eventfd panic, libc-sensitive permission behavior, and one DIO long-run case

Use the harvest document for post-promotion LTP frontier planning; use the
baseline document for the pre-harvest full replay blockers.

## Protected Groups

These groups were already near or at the observed ceiling in the supplied local
score table:

| Group | Local shape | Observed target shape | Priority |
| --- | ---: | ---: | --- |
| basic | 102 per combo | 102 per combo | protect |
| busybox | 54 per combo | 54 per combo | protect |
| lua | 9 per combo | 9 per combo | protect |

## Performance Gaps

These groups already score, but still leave material points on the table:

| Group | Local shape | Observed target shape | Main kernel paths |
| --- | ---: | ---: | --- |
| cyclictest | 4.0-4.34 | about 8 | timers, scheduler wakeup, futex, signals |
| iozone | 20.29-25.76 | about 40 | VFS, page cache, read/write, pwrite/pread, block I/O |
| iperf | 6.0 | about 12 | loopback TCP, socket buffers, wakeups, copy paths |
| libcbench | 27.0-29.19 | about 54 | syscall hot paths, mmap, time, process/thread primitives |
| libctest | 217 on musl combos | 220 | remaining libc ABI correctness |
| lmbench | 42.12-42.70 | about 72 | fork/exec/wait, context switch, pipe/socket latency, mmap |
| netperf | 5.42-5.68 | about 10 | loopback TCP/UDP, poll/select, socket wakeups |

Passing a benchmark group is not enough. If the score is materially below the
observed target shape, keep it in the optimization backlog.

## LTP Gap

The largest known functionality gap is LTP:

| Group | Local shape | Observed target shape | Notes |
| --- | ---: | ---: | --- |
| LTP | 2545-6091 per combo | 72000 per combo | expand real kernel compatibility; do not fake output |

Current known payload layers:

- repository-open subset: `ltp_test.txt`
- official packaged glibc binaries: 2840 per arch image
- official packaged musl binaries: 2820 per arch image
- Docker recomputed current list: 1089 entries, all resolving on all four
  combinations as of `docs/baseline-2026-06-08.md`
- Docker recomputed unopened runtest frontier: 2734 unique available lines as
  of `docs/baseline-2026-06-08.md`
- after `docs/ltp-harvest-2026-06-08.md`: current list is 1099 entries and the
  unopened frontier is 2724 unique available lines

Recompute these from official images and support-disk plumbing when the images,
runner, or `ltp_test.txt` change.

## Update Rules

When updating this file:

- record the date and source of the score evidence
- separate local replay results from official submission results
- keep stale snapshots instead of overwriting them silently if they explain past
  prioritization
- update `AGENTS.md` only if the shape of the strategy changes
- never use stale root artifacts such as `rv_.out`, `la_.out`, or `score.txt` as
  current evidence
