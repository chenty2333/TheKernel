# Scheduler and I/O workload probes

`tools/thekernel.py bench --suite scheduler|io|all --iterations 1000` runs the
workload in a KVM guest. It fails if a required operation fails; unavailable
io_uring, fixed resources, direct I/O or fsync do not silently select another
path. The benchmark executable is built with the existing guest tools and
installed at `/opt/thekernel-tests/bin/thekernel-kernel-bench`.

The executable accepts `scheduler|io|all ITERATIONS EXCLUSIVE_DATA_FILE`.
Iterations must be 32–1,000,000. The data file must not already exist. Choose
disk-backed storage, including when invoking the executable on the host. Host
builds belong under `/home/ava/.cache/thekernel-targets/`. Each result is a JSON
line emitted after its scenario completes: scheduler produces four rows, I/O
72 rows, and `all` 76 rows.

For a focused perf resource-lifecycle regression, run
`/opt/thekernel-tests/bin/thekernel-kernel-bench perf-lifecycle` in the guest.
It opens, enables, disables, and closes 256 task-local and 256 CPU-wide events,
keeping a live event in each scope to check that quota reclamation preserves
surviving groups. It makes no performance comparison.

## Scheduler pressure

The foreground parent-to-child pipe handoff runs against no background load,
CPU load, I/O load, and mixed load. `KERNEL_BENCH_WORKERS` selects workers per
kind, from 1 to 64; by default it uses the online CPU count capped at 64. The
comparison runner sets this explicitly to the guest vCPU count. Mixed pressure
runs that many CPU workers and that many I/O workers simultaneously.

A CPU work unit performs 65,536 dependent integer updates. An I/O unit writes
4 KiB, fsyncs the file, reads back, and checks generation-dependent content.
Workers use separate offsets in an exclusively created, unlinked 512 KiB file.
Both pressure and I/O files are created under `/root` on the rootdisk;
`fstatfs` must report ext4 before any workload runs. `/var/tmp` is unsuitable
because TheKernel mounts a memory filesystem there.
Their fsync calls operate on the shared inode. All workers complete an initial
unit and warm up for at least 100 ms before handoffs begin.

The handoff probe performs 128 warmup handoffs, then the requested measured
count. p50/p95/p99 use nearest-rank percentiles and include pipe transfer plus
scheduling, rather than instrumentation of the exact runnable-to-running
interval. `elapsed_ns` covers the measured handoffs. Background progress uses
`pressure_elapsed_ns`, a window of at least 250 ms that can extend beyond the
handoffs. Nested worker records report units, units/second and maximum observed
progress gap, including the initial wait and unfinished final unit. Completion observations after the stop publication are excluded. The window
ends immediately after that publication; the final gap extends from the last
counted completion (or window start) to that endpoint.

CPU and I/O units have different meanings: compare worker rates within a kind.
The comparison runner reports each kind's aggregate and slowest worker rate;
it rejects a run with a worker making zero measured progress. It also reports the same-kind Jain fairness index and largest complete-window
progress gap. These short windows are useful regression probes, not proof of
starvation freedom.

## Exact wake-to-run samples

Scheduler rows also contain `wake_trace`, measured for the handoff child rather
than the collecting parent. Per-CPU perf rings sample the real `sched_wakeup`
and `sched_switch` tracepoints with `CLOCK_MONOTONIC`. Field offsets and event
IDs come from tracefs. After each reply the parent drains the rings; no sampler
thread is introduced. The runner mounts tracefs in the guest when needed.

The collector retains target-child events, orders them by sample time across
CPUs, and pairs each wake with the first subsequent switch to that child.
`wake_to_run_p50_ns`, `wake_to_run_p95_ns`, and `wake_to_run_p99_ns` are nearest-
rank percentiles of those nanosecond intervals. `samples` reports their actual
count. Before each measured request, the parent waits for a child
`sched_switch` record with `prev_state == TASK_INTERRUPTIBLE` and a timestamp
strictly newer than both the previous request and the latest observed child
wake/switch-in. Thus an earlier block followed by a wake cannot open the gate.
The child has finished its reply and blocked on the empty request pipe; a
merely runnable re-dispatch cannot satisfy
this gate. The parent drains rings and yields while waiting, failing after ten
seconds without the required switch. It timestamps the request only after the
gate opens. Gate work remains included in foreground CPU/context-switch counts
and the full measured elapsed time. Preempted-but-runnable re-dispatches do not
create wake samples. These intervals end at the scheduler handoff observation,
before architectural context restoration.
The original pipe handoff percentiles remain separate end-to-end measurements.

Tracing begins before handoff warmup. Only the first warmup request is ungated,
since its initial blocking switch may precede trace enable; all subsequent
warmup requests use the same gate. Warmup records are drained and counted for
loss checks but excluded from the measured target-event array and percentiles.
The measurement window begins before the first measured gate. Kernel wake
timestamps are captured at the successful enqueue commit and preserved through deferred, lock-external perf
publication; switch timestamps are captured at the switch observer boundary.
This prevents producer lock/BPF overhead from changing the event timestamp.
The same tracing and drain overhead applies to every compared kernel. The
collector allocates a bounded target-event array and 64 data pages per event
per CPU; its memory is part of the caller RSS measurement. Overflow,
LOST, throttling, malformed records, missing samples, unmatched wakes, or a
final event count inconsistent with collected records fail the experiment.
A bounded edge buffer fails explicitly if unusually many target events fill it.

## Foreground resource measurements

Every row includes a `measurement` object with scope `foreground_caller`.
`cpu_user_ns` and `cpu_system_ns` are differences of `getrusage(RUSAGE_SELF)`
CPU times, converted from microseconds to nanoseconds. `voluntary_switches`
and `involuntary_switches` are differences of the corresponding rusage counts.
`cpu_migrations` is the task-local `PERF_COUNT_SW_CPU_MIGRATIONS` count; perf
support and permission are required, with no fallback. The event does not
inherit into children. All zero counts are valid. Linux records a pending
migration at the next sched-in; TheKernel currently compares successive
execution CPUs. In particular A→B→A before running differs. The report retains
raw values but refuses cross-OS migration-counter improvement inference.
Neither counter measures every ineffective migration.

These counters bracket the measured handoffs in the scheduler parent or the
measured I/O phase in the submitting process. Counter enable/disable and rusage
sampling add small boundary overhead outside the workload timer. Scheduler
children and background workers are excluded, as are I/O warmup, registration
and write readback. `maxrss_kib` is the caller's absolute process lifetime RSS
high-water mark, not a per-scenario delta or kernel memory measurement; earlier
scenarios may establish that high-water mark. It cannot alone establish the
planned resource-use guardrail.

The comparison keeps per-trial zero values and absolute changes, but does not
calculate a geometric ratio or declare improvement when any trial is zero.
Migration and context-switch counts are diagnostic costs, not proof of a win.

## I/O matrix

The io_uring matrix contains 4 KiB pseudo-random and 128 KiB sequential accesses,
queue depths 1/8/32, ordinary and registered files/buffers, buffered and direct
I/O, and reads, writes, or writes followed by fsync after each completed batch.
It opens both buffered and direct descriptions before unlinking the file,
initializes 16 MiB, and releases the storage when the descriptions close.

The workload uses a deterministic permutation of blocks, offset-dependent
content, and changing write generations to detect dropped writes. Every CQE
must have the expected byte count and a unique submission identity. Reads
validate content; write cases perform readback outside write timing. Each
scenario warms up for 128 operations. `includes_buffer_work` records that timing
includes buffer preparation, CQE consumption and read integrity checks. Buffers
are reused after completions and unregistered before their storage is freed.

`fsync_per_batch` means the whole batch is durable at its boundary, not that
each request completes durably. Compare this only at the same queue depth;
different depths provide different persistence boundaries. Buffered cases use
a warm dataset, direct cases request `O_DIRECT`, and long runs wrap the dataset.
The executable has a 600-second watchdog; its guest runner may impose a shorter
limit.

`THEKERNEL_IO_BEGIN` identifies each scenario before timing starts. On `SIGALRM`,
`THEKERNEL_IO_TIMEOUT` reports the active phase and batch, submissions returned
by `io_uring_enter`, consumed completions, missing `user_data` values, and SQ/CQ
indices and loss counters before exiting nonzero. A completion can already be
in the CQ while still appearing in the missing list if the caller has not
consumed it. Read the timeout snapshot together with the latest begin marker;
a shorter host timeout that kills the VM cannot produce this guest diagnostic.

## Equivalent guest comparisons

The product CLI's Linux comparison uses
`tools/qemu_runner/kernel_benchmark.py` and the existing QEMU runner. TheKernel
must use its shell profile with a drive-rootfs ESP; Linux uses the existing ESP
builder with `config/x86_64/grub-linux-shell.cfg`, which boots the same shell-init
script from the same rootfs. Linux must identify itself as 7.2.3 in its console.
After Linux reaches the shell-ready marker, the benchmark commands run
`/bin/busybox dmesg -n 4` to keep routine kernel diagnostics from interleaving
with serial workload JSON. This preserves the boot banner for version validation
and leaves kernel errors visible. Failure to configure the console prevents the
workload from running; the parser still requires intact records.
A host Linux run only validates the workload and is not this comparison.

Start with a single functionality trial before collecting measurements:

```sh
python3 tools/thekernel.py bench --suite scheduler --accel kvm --trials 1 \
  --linux-kernel /home/ava/.cache/thekernel-targets/linux-7.2.3-oracle/build-dev-image/arch/x86/boot/bzImage
```

Omit `--linux-kernel` to build the configured oracle. After all functionality
gates pass, use `--suite all --trials 10` for the complete comparison. Use
`--host-cpus 0,1,2,3` to select an available common host CPU mask. To include an
already validated candidate, supply both `--candidate-kernel` and
`--candidate-esp`; the runner does not implement or choose a candidate policy.
The CLI prints the path to its `results.json` after a successful experiment.

Both guests receive the same immutable starting rootfs contents, private raw
writable copies, the same VirtIO devices, memory and 1/4-vCPU configuration.
Rootfs snapshot mode is disabled so guest fsync can reach the private host file.
Kernel and ESP inputs are copied privately into the existing experiment run
directory and the ESP payload is checked against the copied kernel. All trials
use these copies, so concurrent builds cannot change the experiment inputs.
The copies are deleted on success or failure. The private rootdisk is deleted
after each run. Identical host CPU masks are
inherited by QEMU. Each guest starts paused; the existing QMP controller obtains
`query-cpus-fast` thread IDs, validates they belong to that QEMU, pins guest
vCPU i to host CPU i in the supplied ordered mask, and verifies each affinity
before `cont`. Emulator and I/O threads retain the common mask. Failure to
confirm the complete mapping aborts the run before guest execution. Results
include the verified mapping for every trial and warmup guest. The caller's
CPU mask and temporary-directory environment are restored even on failure.

One trial is a functionality smoke pair and emits no statistical inference.
The default ten trials run sequentially with rotating target order, after a
discarded guest warmup round. Both guests must shut down normally and emit the
complete, duplicate-free scenario matrix with matching workload parameters.
An optional correctness-tested candidate is a third target.

For each identical scenario, comparisons use paired trial ratios and a 95%
percentile bootstrap interval around their geometric mean. Trials, not
individual operations from one run, are the resampling unit. The output includes
per-trial values, confidence intervals and metric threshold results. A metric
meets the 10% improvement threshold only if the interval's lower bound reaches
10%. More than 5% point regression is flagged separately. These flags do not
select a default kernel policy or replace functional validation.

## Predeclared M5 targets

Before baseline measurements, the scheduler primary target is fixed as
`wake_to_run_p99_ns` under **4 vCPUs and mixed CPU/I/O pressure**. It is the
handoff child's actual enqueue-to-handoff interval. Handoff latency, background
throughput, CPU time, whole-workload resource costs and real desktop frame time
are guardrails, not substitutes for this primary target. Results do not change
the chosen scenario retrospectively.

The I/O primary target is fixed as `elapsed_ns / iterations` for **4 KiB
random reads, registered resources, buffered I/O and queue depth 32**. Other
operations, including writes, fsync and direct I/O, remain guardrails. No I/O
winner is declared by selecting the best row after collecting the 72-scenario
matrix.

## Performance acceptance still required

No scheduler or I/O candidate policy is selected by these probes. The final M5
experiment still requires correctness-tested candidate implementations, actual
baseline/candidate/Linux guest measurements, desktop frame times, whole-workload
resource accounting, broader fairness checks, and resource-use guardrails.
The exact trace and foreground counters above still require guest validation.
Every semantic gate must pass before a candidate can enter the default path.

Graphics frame statistics come from consecutive Wayland frame callbacks in the
real EGL/Vulkan/SHM clients, using the guest monotonic clock. They measure client
callback cadence, not physical scanout timestamps or presentation feedback.
The kernel benchmark does not produce desktop frame statistics.

TheKernel ordinary buffered I/O currently dispatches through an owned worker,
while fixed buffered I/O executes in the submitting context. Ordinary/fixed
differences therefore include dispatch costs and cannot isolate registration
reuse. Candidate comparisons must hold that scenario and execution path fixed.

The `sched-wake-locality` candidate is disabled in baseline builds. It classifies
Normal tasks using complete running bursts ending in a committed block: at least
two consecutive bursts must each be at most 500 microseconds, with a one-quarter
EWMA also at most 500 microseconds. Preempted running slices accumulate; ready
queue waiting and aborted blocking attempts do not train the classifier. Within
the existing block-owner transaction, a qualifying task retains an idle source
CPU, otherwise prefers an initialized affinity- and utilization-eligible idle
CPU. When all eligible CPUs are busy, changing wake ownership requires at least
a two-task load advantage. This is an experimental placement policy, not an
established improvement; weights, EEVDF eligibility and RT/deadline policy remain
the same. The predeclared mixed 4-vCPU wake-latency target and all guardrails still
apply.

The `io-submit-batch` candidate retains a separate generation-bound lease for
 every request while avoiding repeated current-slot validation and combining
fixed-buffer capability/range validation. Its notification token is an explicit
stack owner: up to 32 nonblocking NOP completions may coalesce waiter wakeups,
with immediate CQ publication and eventfd accounting. Before non-NOP preparation
or execution, linked dispatch, errors or return, pending notification is flushed.
Asynchronous completions never inherit the token. Fixed buffered I/O can block,
so the candidate does not defer notifications across those operations; no
batch-notification improvement is claimed for the primary fixed-read scenario.

### Isolating the I/O candidate

`build --io-submit-batch` enables only the existing I/O candidate. It uses a
separate `mem1g-io-submit-batch` artifact directory at the default memory size.
`--m5-candidate` also enables the scheduler candidate and therefore cannot
isolate the I/O change. Benchmark baseline commands reject either candidate
flag; pass a separately built candidate kernel and drive-rootfs ESP instead.

```sh
./tools/thekernel.py build --profile shell --rootfs-transport drive --io-submit-batch
./tools/thekernel.py bench --suite io --accel kvm --iterations 1000 --trials 10 \
  --candidate-kernel "$HOME/.cache/thekernel-targets/out/x86_64/q35-uefi/shell/mem1g-io-submit-batch/kernel-x86_64" \
  --candidate-esp "$HOME/.cache/thekernel-targets/out/x86_64/q35-uefi/shell/mem1g-io-submit-batch/kernel-x86_64-drive.esp"
```

Keep the same rootfs, memory, CPU count and registration mode for both builds.
`THEKERNEL_STATE_DIR` can isolate the experiment from an interactive guest's
build lock; all targets and run files must remain on disk. When using an
existing image, pass the same explicit `--rootfs` to build and benchmark.

For fixed I/O, the candidate avoids repeated current-slot checks inside one
exclusive resource-table transaction and obtains a retained buffer's capability
and exact range together. Each request still owns a separate generation-bound
lease. This is validation work saved on real read/write paths, not a combined
physical I/O request or a shared completion owner. Ordinary-resource rows are
controls; comparing ordinary and fixed absolute times does not isolate this
change. The fixed-read target above remains primary, and the full matrix still
checks read contents, write readback and unique completion identities.

The VFS `successful_byte_count_trusts_provider_for_source_and_content_effects`
test records a separate API boundary: an intentionally incorrect provider can
report an in-range success without sourcing input or changing its target, on
both published and immediate completion paths. Passing resource-lifecycle tests
or this benchmark does not establish truthful provider effects. The test does
not accuse a production provider of returning a false result.

### I/O-only candidate result (2026-09-07)

The isolated candidate completed ten rotating, paired trials plus one discarded
warmup round against the default-policy baseline and Linux 7.2.3. Each guest ran
all 72 I/O scenarios with 1,000 operations per scenario: KVM, four vCPUs pinned
to host CPUs 0–3, 1 GiB RAM, and private writable rootdisks from the same input
image. All 33 guests passed content, completion-identity, complete-matrix and
clean-shutdown checks. Both TheKernel builds also passed the io_uring and
registered-buffer guest smoke tests.

For the predeclared 4 KiB random fixed-buffer buffered-read QD32 target:

| Metric | Baseline | I/O candidate |
| --- | ---: | ---: |
| Geometric mean elapsed time per operation | 297.62 µs | 316.07 µs |
| Paired change in elapsed time | — | +6.20% |
| 95% bootstrap interval for that change | — | −11.05% to +24.30% |

Positive time change means slower. The interval crosses zero, so this run does
not establish a stable regression or improvement. It fails the predeclared
10% improvement criterion, and its point estimate triggers the 5% regression
flag. Across all 72 latency rows, 19 trigger that point-regression flag (13
fixed-resource and six ordinary-resource controls); none meets the 10%
improvement criterion. These are per-scenario guardrail flags, not 19 claims of
statistically established regression. The Linux target's geometric mean was
7.13 µs per operation; this comparison does not identify the cause of the
TheKernel cost.

Decision: retain the default policy and make no performance-win claim for this
candidate. The functional evidence supports preserving the tested resource and
completion boundaries for this limited validation optimization. It does not
prove arbitrary provider effects, physical-request batching, or the stronger
operation-constrained executor design. This was a shared development host, not
an otherwise idle controlled machine; foreground-only counters and absolute
maxRSS also leave whole-system resource costs unmeasured.

The focused host checks passed: 72 kernel io_uring tests with the candidate off,
75 with it on, 57 lower io_uring crate tests, and the VFS false-success boundary
test. The combined CLI, process-runner and benchmark Python suite passed 90
tests. The experiment also exposed and fixed two runner issues: firmware LF-CR
line endings blocking exact readiness markers, and Linux diagnostic output
interrupting workload JSON. Their regressions preserve strict marker, Linux
boot-version and JSON/matrix validation.
