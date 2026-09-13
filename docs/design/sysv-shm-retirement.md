# SysV SHM exit retirement: the window, the fix, and what was measured

`tests/guest/system-init.c::test_sysv_shm` was flaky before commit `95c16a8c`
("fix(ipc): retire SysV SHM attachments at process exit, before reap"): the
same boot could report `ok 4 - sysv-shm` or `not ok 4 - sysv-shm` with no
change to the kernel. This note records what the window is, what the fix
does, and the measured failure rate of a direct attack on it inside TheKernel.

Everything under "measured" below was produced on this host with QEMU/KVM at
`--smp 4`, `--memory 512M`, shell profile, and is reproducible with one
command (§6). Numbers quoted from other workstreams are marked as such.

## 1. The window

A SysV segment marked `IPC_RMID` stops being attachable as soon as its last
attachment is gone. The guest case asserts exactly that:

1. `shmget(IPC_PRIVATE, 4096, IPC_CREAT|0600)`, `shmat` (read-write);
2. `shmctl(id, IPC_RMID, NULL)`;
3. `shmat` again (read-only) — still allowed, the parent holds an attachment;
4. `fork()`; the child inherits both attachments, attaches a third, touches a
   byte, detaches its own, and `_exit(0)`;
5. the parent `waitpid()`s the child, detaches its own two attachments;
6. **the parent `shmat(id, NULL, 0)` and requires `(void *)-1` with
   `errno == EINVAL`.**

Step 6 is only correct if the child's *inherited* attachment records died with
the child. Before `95c16a8c` they did not: the only authority that retired an
exiting process's attachments was the deferred VMA mapping finalizer, which
the policy worker runs after `AddrSpace` teardown, and `AddrSpace` teardown
completes *after* `publish_final_process_exit()`. So `wait()` could return
while the exited child still owned the segment, `shmat` found the removed
shmid, and the case failed at `shmat-removed-id` with `errno == 0`.

The window is therefore: **a parent's `wait()` has returned, and the child's
SysV SHM identity is still alive.**

## 2. The fix

`95c16a8c` restores a namespace-aware `clear_proc_shm_in_namespace(namespace,
pid)` call in `ProcessData::cleanup_touched_ipc_namespaces`, which the final
exit path runs *before* the zombie is published — Linux's `do_exit()`
ordering, where `exit_shm()` precedes `exit_notify()`.

In `kernel/src/task/ops.rs` the order inside the final-exit path is:

| step | line | what it does |
| --- | --- | --- |
| 1 | `cleanup_touched_ipc_namespaces(pid)` | retires the exiting process's SysV attachments (`clear_proc_shm_in_namespace`) |
| 2 | `release_process_fd_table` + drop | closes the exiting process's file descriptions |
| 3 | `publish_final_process_exit` | makes the zombie waitable, so `wait()` can return |

Two consequences are load-bearing for the tests below:

* after step 1, an `IPC_RMID` segment whose last attachment belonged to this
  process is unreachable: `shmat` must fail with `EINVAL`;
* step 2 happens after step 1, so *EOF on a pipe held only by the exiting
  process is proof that step 1 already ran*, even before step 3 and therefore
  before any `wait()`.

Page lifetime is deliberately not decided at step 1. A segment's frames are
owned by the `Arc<SharedPages>` that every mapping holds through its
`SharedBackend`, and a mapping releases that ownership only after its TLB
grace, so removing the attachment record cannot free memory a live or
retiring mapping still uses; the outstanding VMA lease's finalizer becomes an
exact-match no-op. Host tests in `kernel/src/syscall/ipc/shm.rs`
(`exit_retirement_destroys_rmid_identity_without_vma_finalizers`,
`exit_retirement_releases_rmid_identity_before_late_vma_finalizers`,
`late_vma_finalizer_after_exit_retirement_cannot_detach_a_successor`, and the
negative control
`deferred_only_retirement_leaves_an_exited_owners_rmid_identity_reachable`)
pin that ordering and the successor-reuse case in unit tests.

## 3. The instrument: five variants, five different attacks

`tests/guest/portable/sysv-shm-exit-stress.c` loops the window many times per
boot. Each round creates its own private segment, so rounds are independent.
`scripts/build-rootfs.sh` compiles every `tests/guest/portable/*.c` into
`/opt/thekernel-tests/portable/<stem>`, so the program is already in the guest
rootfs; it takes `<variant> <rounds> <alarm-seconds>`.

| variant | what it does | what it attacks |
| --- | --- | --- |
| `wait` | the acceptance topology exactly: one child, parent `waitpid`s, detaches, probes | the window itself, at the same point the guest case asserts it |
| `pipe` | no `wait()` at all: the parent blocks on `read()` until EOF on a pipe whose write end is held only by the child, then probes | the exit ordering *without* relying on the reap path: step 2 above EOF implies step 1 already ran, so the probe is a hard assertion that fails deterministically if retirement is later than fd teardown |
| `multi` | four children attach and exit before the parent waits for all of them | the same window four times per round; any single surviving record keeps the removed segment reachable |
| `nowait` | the parent probes with no synchronisation right after `fork()`, then waits and probes again | the second probe is the assertion; the first is descriptive (`alive_probe`) and is expected to fire while the child is still running |
| `shared` | a child exits while a *grandchild* still holds the segment; the parent probes after reaping the child (the segment **must** still be reachable), then releases the grandchild and probes again (it must now be gone) | over-retirement: a fix that clears more than the exiting process's own attachments, or that retires a namespace instead of a process. The final probe attacks the same window one generation deeper |

The program prints one summary line per invocation
(`SYSV-SHM-STRESS variant=... rounds=... completed=... setup_fail=...
child_fail=... value_fail=... attach_fail=... errno_fail=...
retire_early_fail=... grandchild_fail=... alive_probe=...`) and a verdict
line (`SYSV-SHM-STRESS-OK|FAIL <variant>`); the process exits non-zero when
any counter except `alive_probe` is non-zero. `attach_fail` and `errno_fail`
are the two ways step 6 can fail (`shmat` succeeded, or it failed with the
wrong errno); `retire_early_fail` is the over-retirement counter.

## 4. What was measured

### 4.1 Controls: the instrument can see the bug

Both controls run the *same* program, boot harness, round counts and `--smp 4`
parameters as the measurement; only the kernel differs. "Defective rounds" is
the program's own counter sum.

| variant | one-line revert of the fix (`5c4fdae9` minus `clear_proc_shm_in_namespace`) | pre-fix tree `95c16a8c^` |
| --- | --- | --- |
| `wait` | 675 / 2000 | 1627 / 6000 (3 boots) |
| `pipe` | 1846 / 2000 | 1889 / 2000 |
| `multi` | 507 / 2000 | 395 / 2000 |
| `nowait` | 1747 / 2000 | 1735 / 2000 |
| `shared` | 1795 / 2000 | 1779 / 2000 |

Every defect in both controls is `kind=removed-id-still-attachable errno=0` —
exactly the failure the guest case reports. The one-line revert isolates the
fix itself (same tree, same tool, only the restored call removed); the older
tree shows the same behaviour on the code the flake was reported against.

A detection power of 20–92% per round in the weakest/strongest variant means a
regression of the pre-fix kind cannot hide behind the sample sizes below; the
rate that matters post-fix is not "a few per boot" but "any at all".

### 4.2 Host Linux control

The same program on host Linux (kernel 7.2.4) says the harness agrees with the
ABI rather than with TheKernel: 19000 rounds across the five variants with
zero defects on the committed revision (`wait` 5000, `pipe` 5000, `nowait`
5000, `multi` 2000, `shared` 2000), and 10000 rounds on the instrumented
revision used for the follow-up measurement. This is a control on the tool,
not kernel evidence.

### 4.3 The window inside TheKernel, post-fix

Four post-fix runs, all at `--smp 4`, 512 MiB, KVM, shell profile. "Window
defects" counts `attach_fail + errno_fail + retire_early_fail`, the counters
that mean the removed segment was reachable when it should not have been (or
unreachable while a live attachment existed). "Other" counts every other
counter, i.e. rounds that did not complete for a reason outside the window.

| run | label | boots | variants | rounds | window defects | other | 95% UB, window defects |
| --- | --- | --- | --- | --- | --- | --- | --- |
| original revision, strict chain | `rate` | 11 | 5 | 550000 | **0** | 2 | 5.4e-6 per round |
| instrumented, fixed driver | `confirm` | 4 | 5 | 200000 | **0** | 0 | 1.5e-5 per round |
| instrumented, `shared` only | `shared2` | 7 | 1 | 140000 | **0** | 1 | 2.1e-5 per round |
| instrumented + reports, `shared` only | `shared3` | 10 | 1 | 200000 | **0** | 0 | 1.5e-5 per round |
| **all post-fix runs** | | **32** | | **1090000** | **0** | **3** | 2.75e-6 per round |

`confirm`, `shared2` and `shared3` stopped at fewer boots than they asked for
because the artifact stamp refused boots after a mid-run edit to the stress
program's C source (an intentional guard: `--no-build` will not boot a kernel
and rootfs that no longer match the tree). Every boot in the table is a boot
that actually ran its rounds; see §4.5.

Per variant over all post-fix runs (`shared` is the pooled `shared` half of the
five-variant runs plus the two `shared`-only runs):

| variant | boots | rounds | window defects | other | 95% upper bound, window rate |
| --- | --- | --- | --- | --- | --- |
| `wait` | 15 | 150000 | 0 | 0 | 2.0e-5 |
| `pipe` | 15 | 150000 | 0 | 0 | 2.0e-5 |
| `multi` | 15 | 150000 | 0 | 0 | 2.0e-5 |
| `nowait` | 15 | 150000 | 0 | 0 | 2.0e-5 |
| `shared` | 32 | 490000 | 0 | 3 | 6.11e-6 |
| **total** | **32** | **1090000** | **0** | **3** | **2.75e-6** |

The zero is not a summary-line artefact: every console log of every run was
grepped for a non-zero `attach_fail`, `errno_fail` or `retire_early_fail`, and
the only non-zero counters anywhere in the 32 stored console logs are `rounds`,
`completed`, three `child_fail=1` (§4.4) and `nowait`'s descriptive
`alive_probe`.

**The fix holds at this sample size.** 0 window defects in 1090000 rounds
gives a one-sided 95% upper bound of 2.75e-6 per round (rule of three:
3/1090000), and 6.1e-6 per round for `shared` alone. For comparison, the
weakest pre-fix variant (`multi`) failed 19.8% of rounds and the acceptance
topology's own variant (`wait`) failed 27% — four to five orders of magnitude
above this bound (72000x and 98000x respectively). A per-round bound is the honest headline because each round is an
independent re-run of the window.

Boot level: 0 of 32 boots failed a window assertion, whose 95% upper bound is
8.9% per boot. That number carries almost no information about a rare window,
which is the whole reason the stress program exists.

### 4.4 The `shared` non-window anomaly

Three times across the runs above — `rate-8` round 1603, `rate-11` round 4383,
`shared2-6` round 4231 — a `shared` round ended with exactly this shape:

```
SYSV-SHM-STRESS-FIRST-FAIL variant=shared round=1603 kind=child-status errno=1280 data=0
SYSV-SHM-STRESS variant=shared rounds=10000 completed=10000 setup_fail=0 child_fail=1 \
  value_fail=0 attach_fail=0 errno_fail=0 retire_early_fail=0 alive_probe=0
```

`errno=1280` is the raw `wait` status: normal exit, code 5, which in
`round_shared` means "the child's read of the grandchild's attached byte did
not return exactly one byte". Nothing else moved: `completed` equals `rounds`,
every other counter is zero, and the other variants in the same boots were
clean. Rate: 3 events in 490000 `shared` rounds (6.1e-6 per round; all three
fall in the 290000 rounds that ran before the final instrumentation, 1.0e-5
there).

The kernel log of `rate-8` (COM2, full timestamp range 0.002–145.4 s) contains
no `SIGSEGV`, `SIGBUS` or `SIGPIPE`, no SysV SHM error message, and no
non-zero task exit code; task-exit lines are sampled (5824 of roughly 80000
process exits in that boot appear), so this bounds the search rather than
proving absence.

`retire_early_fail=0` in these rounds is informative. That counter is the
parent's mid-round probe, which runs after reaping the child and before
releasing the grandchild, and expects the removed segment to still be
reachable because the grandchild holds it. It succeeded, so something still
held the segment at that point even though the child was gone and the
grandchild had stopped signalling. That is consistent with a grandchild that
attached and was still alive, and with a grandchild that had just died and
whose own exit retirement had not yet overtaken the probe — the grandchild is
not the parent's child, so nothing in `round_shared` orders the parent's probe
after the grandchild's teardown.

Candidate mechanisms, in order of how much they would matter:

1. the grandchild's `shmat` failed with `EINVAL` — the removed segment was
   destroyed while the child still held inherited attachments. This would be a
   defect in the retirement ordering's reverse direction and would matter;
2. the grandchild attached but read a value other than the parent's 17 —
   shared-page aliasing, or frame reuse without clearing;
3. the grandchild was killed by a signal, or exited without reaching any of
   its own failure paths, before it could write the attached byte.

Commits `b9fcb83a` and `e4467c06` make these separable in the console log:

* the grandchild writes `{code, error}` to the `finished` pipe the parent
  already reads; code 1 `shmat` (with its errno), 2 wrong first byte (with the
  byte), 3 write of the attached byte, 4 `shmdt`, 5 unexpected token, 6 its
  own `fork`;
* the child writes code 7 when its read does not return one byte, with
  `data=0` meaning EOF (the grandchild died without reporting) and `data` the
  errno otherwise;
* the child and grandchild install a fatal-signal handler that reports code 8
  with the signal number, so a fault in the shared mapping can no longer look
  like a silent exit;
* both are counted by the new `grandchild_fail` counter, and a recurrence
  prints a `FIRST-FAIL` line naming the code, the errno and the round.

A first attempt at this instrumentation was itself wrong — the child wrote its
report to a descriptor it had already closed, and `shared2-6` reproduced the
anomaly through that hole — which is why the fix is a separate commit and why
the fault-injected host builds are part of the evidence: a grandchild that
exits before writing produces code 7, one that dereferences NULL produces code
8, and one that fails `shmat` produces code 1.

### 4.5 First measurement: the 11 boots of the original run

The first run used the program as committed in `36556caa` and the strict
`&&`-chained guest command:

```sh
THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/shm-flake-verify \
python3 tools/sysv_shm_stress.py --label rate --no-build \
    --rounds 10000 --boots 16 --max-attempts 24 --json rate.json
```

Eleven boots completed: 5 variants x 10000 rounds x 11 boots = **550000
rounds**, with the per-variant tallies in the first row of the table in
§4.3 and the two `shared` anomalies of §4.4. Boots `rate-12` onward never
ran: the C source of the stress program was edited mid-run, which invalidated
the rootfs build stamp, and the guard refused each later `--no-build` boot in
0.1 s with `guest test sources or rootfs build inputs changed; rebuild before
running`. Those refusals are recorded in the run's boot table as boots with no
console log; they are a stale-artifact guard, not a kernel result, and the
instrumented revision was rebuilt and measured separately (§4.3).

### 4.6 The follow-up hunt with attribution in place

`shared3` (10 boots, 200000 `shared` rounds, the instrumented build with both
report paths and the fatal-signal handler) produced **no event at all**. That
is the uncomfortable result rather than the satisfying one: at the observed
rate of 3 events per 490000 `shared` rounds, a clean 200000-round run happens
with probability about 29%, so it does not show the anomaly is gone, and the
anomaly is still unattributed. What the run does establish is that the
diagnostics cost the variant nothing on the clean path (0 defects, 0 reports,
`completed == rounds` in every boot) and that a recurrence will now name its
cause instead of printing `child-status`:

* `grandchild-report code=1 error=<errno>` — the grandchild's `shmat` failed.
  `EINVAL` here would mean the removed segment died while the child still held
  inherited attachments, which is the serious reading of this anomaly;
* `grandchild-report code=2 error=<byte>` — the grandchild attached but read a
  value other than 17;
* `grandchild-report code=8 error=<signo>` — the grandchild was killed by that
  signal (the handler reports SIGSEGV, SIGBUS, SIGPIPE, SIGABRT, SIGILL,
  SIGFPE);
* `child-read-incomplete data=0` — the grandchild exited without reporting at
  all, i.e. neither a syscall failure nor a signal this handler covers;
* `child-read-incomplete data=<errno>` — the child's `read` itself failed.

## 5. What remains unverified

* **The evidence is QEMU/KVM on the development host, not the N305.** All
  numbers here come from `qemu-system-x86_64` 10.2.2 with KVM on an Intel Core
  Ultra X7 358H, `--smp 4`, 512 MiB, shell profile, `q35-uefi` platform. Timer
  behaviour, TLB shootdown latency, CPU count and cache topology all differ on
  the real machine, and the window is a timing window: a rate measured under
  KVM is not a rate measured on hardware.
* **The boot-level rate of the real `sysv-shm` guest case was not re-measured
  here.** The case runs the window once per boot, so it is a weak instrument;
  this workstream attacks the same window thousands of times per boot instead.
  The pre-fix boot-level rate (~3 of ~11 archived boots) is quoted from the
  historical scan of earlier runs, not from a controlled experiment.
* **The timing distribution is not the same as a cold boot's.** The stress
  rounds run with warm caches and a busy four-vCPU guest; the first execution
  of the window after boot may hit different scheduling windows.
* **Only the default kernel configuration was measured.** No `--asid-fast-switch`,
  `--m5-candidate`, `--io-submit-batch` or `--io-notify-fastpath` variants, no
  `n305` platform profile, no `--smp 1` or `--smp 8` comparison.
* **The `shared` variant's non-window anomaly is not fully explained.** It
  recurred three times in 490000 rounds before the final instrumentation and
  not at all in the 200000 rounds after it; the next recurrence will name its
  cause (§4.4, §4.6), but until one does, "3 per 490000 rounds, cause
  unattributed" is the honest state of it.
* **The instrument is a program, not a proof.** The five variants cover the
  acceptance topology and four deliberate deviations from it. A retirement
  bug that needs a topology none of them builds would not be seen here, and
  the pre-fix controls only show that *this* bug is caught at 20–92% per
  round.

## 6. Reproducing this

One command runs the whole measurement. The driver builds the shell-profile
kernel and rootfs if they are missing or stale, dispatches every boot through
the shared heavy-command serializer (one slot per boot), keeps each boot's
console log and runner log under `<state-dir>/runs/<label>-<n>/`, and prints
the per-variant table with the exact one-sided 95% upper bound:

```sh
python3 tools/sysv_shm_stress.py --label mine --rounds 10000 --boots 16 \
    --state-dir /home/ava/.cache/thekernel-targets/mine \
    --source-cache /home/ava/.cache/thekernel-targets/source-cache \
    --json /home/ava/.cache/thekernel-targets/mine/result.json
```

* `--variant wait` (or any name, or `all`) restricts the run. The `shared`
  variant is the one that produced the non-window anomaly, so a hunt for that
  is `--variant shared --rounds 20000 --boots 10`.
* `--analyze-only --label mine` re-parses the stored console logs without
  booting anything, which is how the tables in §4.3 were produced from runs
  that were cut short.
* `--host-control` runs the same program on host Linux as a harness control,
  and needs a host build of the program:
  `gcc -O2 -o <state-dir>/bin/sysv-shm-exit-stress tests/guest/portable/sysv-shm-exit-stress.c`.
* After any change to the C program or to anything else the rootfs build
  fingerprints, drop `--no-build`: `--no-build` boots refuse to start against
  a stale artifact rather than measure a kernel that does not match the tree.

To reproduce the pre-fix control, put the tool source into a scratch worktree
checked out at the commit before the fix, remove the one restored call, build
and run the driver there:

```sh
git worktree add --detach /home/ava/Worktrees/TheKernel/shm-prefix 95c16a8c^
cp tests/guest/portable/sysv-shm-exit-stress.c \
   /home/ava/Worktrees/TheKernel/shm-prefix/tests/guest/portable/
# then, on top of the current tree, delete the single line
#   crate::syscall::ipc::clear_proc_shm_in_namespace(&namespace, pid);
# from kernel/src/task/process.rs, and boot that build the same way.
```

`tests/ci/test_sysv_shm_stress.py` pins the driver's arithmetic against exact
rational arithmetic and its parser against the program's real output format,
including the four ways a boot stops being evidence. Run it with
`python3 -m unittest tests.ci.test_sysv_shm_stress`.
