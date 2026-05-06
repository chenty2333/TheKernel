# Vendored: axtask

## Upstream

- Source: `axtask` v0.3.0-preview.2
- Repository: <https://github.com/arceos-org/arceos/tree/main/modules/axtask>
- Original manifest: not present in the current tree

## History

| Commit | Description |
|--------|-------------|
| `89473138` | perf(task): cache reusable TaskInner shells |
| `8e237d39` | fix: address review findings round 3 (build, evict, tmpfs, COW, cache) |
| `65d25e6f` | perf(task): add Thread object cache for pthread-create-heavy workloads |
| `3cb367bc` | perf(task): use thresholded reclaim in clone to avoid ping-pong |
| `df817d80` | perf(task): encapsulate exited-task ops with counter for fast empty check |
| `c098032f` | fix: stabilize oscomp la evaluation flow |
| `8eeab8d8` | fix: batch kernel and support compatibility updates |
| `5eb799ad` | fix: harden oscomp kernel semantics and compatibility |
| `74ae4ade` | perf(kernel): reduce low-memory task pressure |
| `b8cf2eb7` | fix kernel affinity, futex, and runner syncs |
| `73f2f01a` | perf(task): safely reintroduce per-cpu stack cache |
| `354e27c3` | chore(task): refresh gc scheduler comment |
| `fb7896df` | fix(task): remove unsafe kernel stack cache |
| `bcd005b6` | feat(mm): scale caches with physical memory |
| `66b54fd4` | perf(task): stop prioritizing gc over joiners |
| `08dfcc19` | perf(task): remove per-layout stack cache cap |
| `6b6ed067` | fix(task): defer stack reuse until final task drop |
| `e65aa4af` | fix(task): reclaim exited thread stacks early |
| `5f433f06` | fix(timer): program early monotonic wakeups |
| `de762e18` | fix(task): prioritize exited-task reclamation |
| `a6f1fb23` | feat(sched): add RT policy support for scheduler syscalls |
| `f809f7a4` | fix(sched): harden runtime scheduler state updates |
| `104d9884` | feat(sched): add runtime CFS classes and scheduler state |

## Changes

This fork is heavily integrated with the kernel task lifecycle. Local changes
cover scheduler policy plumbing, timer wakeups, exited-task reclamation, stack
reuse, low-memory pressure, affinity and futex synchronization, a fast empty
reclaim check, thresholded reclaim for clone-heavy workloads, and reusable
`TaskInner` shell caching.

Syncing with upstream requires a joint audit of `src/task.rs`,
`src/run_queue.rs`, `src/api.rs`, wait queues, and the kernel's `task` module.
Do not treat this as a drop-in ArceOS update without rerunning pthread, futex,
clone, and scheduler tests.
