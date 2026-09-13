# The `exit-status` registry-walk race

Status: diagnosed and fixed on `feat/exit-status-race`.  The evidence is QEMU
(`q35-uefi`, `--accel tcg`) only; nothing here has been reproduced on the N305.

## The symptom

The guest acceptance case `exit-status` runs
`/opt/thekernel-tests/portable/exit-status`, whose second half
(`concurrent_clone_exit()`) forks four workers; each worker runs 32 rounds of

```c
child = clone(SIGCHLD [| CLONE_FS], 0, 0, 0, 0);
if (!child) syscall(round & 1 ? SYS_exit : SYS_exit_group, 37);
reaped = waitpid(child, &status, 0);
if (reaped != child || status != (37 << 8)) _exit(3);
```

and the case fails when a worker exits 3.  Measured at `--smp 4`, the case
failed in 8/100, 9/100, and 5/60 invocations, and 0/100 at `--smp 1`.

## The mechanism

The failure is `wait4` answering **`ECHILD` for a child that exists**.

`sys_waitpid` builds its candidate set from
`Process::try_children(registry)`, which snapshots the parent's children by
walking the whole process registry with the `Processes` iterator in
`crates/linux/process/src/process.rs`.  When that snapshot comes back empty,
`matching_wait_candidates` returns `ECHILD`, and the wait does not block — so a
worker whose child is alive and about to publish `(37 << 8)` gets `ECHILD`
instead, and reports the failure the acceptance case prints.

The snapshot came back empty because of how `Processes` bounded its walk.  It
captured a visit budget at construction:

```rust
remaining: self.membership_count(),
```

and stopped when that budget reached zero, treating exhaustion as proof that the
walk had finished:

```rust
while !self.finished && self.remaining != 0 { ... }
```

Exhaustion is not that proof.  The walk is a PID-ordered cursor: it advances
with `move_next()`, and when its cursor node has been unlinked it restarts from
`after` (the highest PID it has already returned).  The budget is charged per
node *visited*, including nodes it visits to get past removals, so the budget can
run out while entries still exist above the cursor.  The iterator then returns
`None` and every consumer sees a **silently truncated registry** — no error, no
partial-list signal.

`wait4` is the consumer where that is fatal: the parent's `try_children` returns
an incomplete list, the pid filter has no candidate to match, and the syscall
reports "you have no such child".

### What the probe measured

`/proc/sys/kernel/exit-status` (see "Diagnostics" below) made the difference
visible in one record.  The failing observation, from a 200-batch run:

```text
CLONE seq=4059 parent=841 child=838 signal=17
WAIT  seq=4063 pid=841 req=838 got=-1 status=0x0 event=5 err=1 ...
      kids=1 resolved=876 resolved_is_child=1 options=0x0
      kids=[876:vis=838:match=1:sig=17:acc=1:live=0:zom=0]
      candidates(seen=0 pid_ok=0 ok=0 tracee=0 invisible=0)
```

Read that as two independent observations taken inside the same syscall:

* after the failure, the registry still holds the child: global pid 876, visible
  pid 838, `is_zombie=0`, exit signal 17, parent = the _same_ waiter
  (`resolved_is_child=1`, `match=1`, `acc=1`);
* the candidate builder that produced the failure saw `seen=0` children.

`seen=0` is `Process::try_children` returning an empty list for a process that
has a live child in the registry.  Across seven 200-batch runs the failure
occurred once per run and **always** on the first or second check after a
successful `clone`, never after a block — consistent with a
`try_children`→`ECHILD` return rather than a lost wakeup.

### The single-factor experiment

The diagnosis was tested by changing only the budget, before writing the final
fix.  With the `remaining != 0` term relaxed to keep walking, 200 batches × 4
workers × 32 rounds — **25 600 cycles** — produced **0 failures**, where the same
kernel produced exactly one `ECHILD` per 200 batches in each of seven previous
runs.

## The fix

`Processes::next` now re-arms the budget instead of treating exhaustion as the
end of the tree:

```rust
if self.remaining == 0 {
    let live = self.registry.membership_count();
    if live == 0 { break; }
    self.remaining = live;
}
```

Insertion raises the membership count and removal lowers it, so a count above
zero means the tree can still hold an entry above the cursor; the walk is
strictly PID-increasing, so it terminates even while entries keep arriving.  The
observable change is that `try_children` no longer returns a truncated list.

The regression test is
`process::tests::registry_walk_completes_after_its_visit_budget_is_exhausted` in
`crates/linux/process/src/process.rs`.  It builds a registry of six committed
processes and walks it with the budget pre-set to `0` and to `1` — the
pathological forms of the race — and requires all six.  Before the fix the
`0` case returned nothing at all.

## Measured before and after

Both numbers come from the same harness: one guest boot at `--smp 4`, one
`thekernel-exit-status-count` run, which fork+execs the untouched acceptance
binary once per iteration, so every iteration is a fresh process image exactly
like a suite boot.  Both runs used the same kernel build apart from the fix
itself (identical instrumentation, identical rootfs, identical harness).

| build | iterations | failures | rate | 95% CI (Wilson) |
|---|---|---|---|---|
| without the fix | 200 | 27 | 13.5% | 9.4% – 19.0% |
| with the fix | 200 + 200 | **0** | 0% | 0% – 0.9% |

Two "with the fix" runs are reported because the first (200 invocations) was
taken before the trace recorder was trimmed, and the second (200 invocations)
on the exact committed build; both were clean.

Every "without" failure was the acceptance binary's own
`THEKERNEL_EXIT_STATUS_FAIL concurrent-clone-exit`, and every one was the child
process exiting 1 — never a signal, never a timeout.

Two independent pre-fix measurements agree with the baseline: the earlier
witness populations (8/100 and 9/100) and a shell loop (5/60), i.e. about 8.5%.
The 13.5% here is higher because `exit-status-count` removes the shell loop's
per-iteration `waitpid`/`echo` from the parent, which changes the churn shape the
race needs.

End to end, with the fix, the repository's own guest suite at `--smp 4` reported
`1..41` with zero `not ok`, `exit-status` included.

Not a timeout: the failures are `wait4` returning `ECHILD` immediately, so the
iteration ends early rather than hanging.

## Diagnostics

Landed with the fix:

* `tests/guest/tools/exit-status-trace.c` — repeats the acceptance case's exact
  clone/exit/wait worker, prints the failing round's requested pid, reaped pid,
  raw status and errno, then dumps the kernel trace.
* `tests/guest/tools/exit-status-count.c` — the measurement harness above;
  `count N 0` suppresses the trace dump so a measurement boot cannot overrun a
  serial console with it.
* `kernel/src/task/ops.rs`, `kernel/src/syscall/task/wait.rs`,
  `kernel/src/syscall/task/clone.rs`, `kernel/src/pseudofs/proc.rs` — the
  `/proc/sys/kernel/exit-status` ring: one record per pid-targeted `wait4`
  observation, per clone publication, and per exit publication.  A ring read
  never goes through the kernel log, because the log drops records under exactly
  the contention the race needs.

## What is not explained

* **Which walk loses its cursor.**  The budget accounting says truncation is
  possible; the probe says it happened, and relaxing only the budget removed it
  in 25 600 cycles.  What is *not* established is why the visit budget reached
  zero while entries remained above the cursor at that moment: the walk charges
  the budget per node visited, and the one place it can spend budget without
  advancing past an entry is the restart branch (its cursor node was unlinked,
  so it re-seeks from `after`).  That is a plausible account, not a captured
  trace: the probe recorded *that* the candidate list came back empty, not the
  internal cursor path that emptied it.  The fix does not depend on the answer —
  it removes the dependence on a budget being a proof of completion — but a
  reader who wants to know whether `membership_count()` itself can also
  under-count does not get that from this document.
* **The `0xab00`/`0xff00` exits.**  Kernel-log histograms during a stress loop
  show clone children exiting with codes the acceptance source does not contain
  (`171 << 8`, `255 << 8`), with exit counts far above what one loop can produce.
  Kernel-log records are dropped under load, so those histograms are not
  trustworthy and were not used for any conclusion here.
* **`pause-smoke`.**  It hangs in about 3.5% of `--smp 4` invocations.  It was
  not investigated: the brief reserves it until `exit-status` is understood.
  Nothing in this fix is specific to `wait4`, so a truncated registry walk could
  in principle explain a `pause-smoke` hang as well — for example a `SIGCONT`
  delivered to a process that a registry walk failed to find — but that is a
  hypothesis, not a measurement, and it was not tested.
* **The device.**  Every number here is QEMU TCG on the host.  The race is a
  concurrency bug in this kernel, so it should reproduce anywhere, but it has not
  been run on the N305.
