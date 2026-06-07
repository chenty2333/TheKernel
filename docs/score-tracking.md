# Score Tracking

This file tracks score evidence used for planning. It is not an automatic
leaderboard mirror. Update it only when a new local replay, official submission,
or refreshed leaderboard snapshot is available.

## Snapshot 2026-06-08

Sources:

- user-provided leaderboard rows for the top two teams
- user-provided local score table for this repository

Use this snapshot to prioritize work, not to make final submission claims.

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

