# KVM scheduler baseline

`tools.kvm_scheduler_baseline` is the first small, repeatable scheduler
comparison lane. It runs the same statically-linked helper in a TheKernel
guest and, when explicitly supplied, a Linux guest. The lane fixes the
virtual topology at x86_64 `q35` + OVMF + KVM + `-cpu host`, retains every raw
latency observation, and computes nearest-rank P50, P99, and P99.9 on the
host.

The guest helper covers:

- private futex ping-pong on one guest CPU and across two guest CPUs;
- pipe ping-pong on one guest CPU and across two guest CPUs; and
- a deterministic CPU worker on one or two guest CPUs.

Each invocation has an explicit warmup count. `--repeat` starts a fresh guest
for every repetition and workload/placement pair. Results are written below
`--output` as `raw-samples.tsv`, `summary.tsv`, `summary.json`, per-run serial
logs, and a `manifest.json` describing the exact topology. Raw samples are
not inferred from the guest's summary line.

## TheKernel lane

Build the x86_64 shell image and rootfs (the rootfs installs
`/opt/thekernel-tests/bin/thekernel-scheduler-baseline`) and run:

```bash
./tools/thekernel.py build --profile shell
./tools/thekernel.py rootfs
PYTHONPATH=. python3 -m tools.kvm_scheduler_baseline run \
  --target thekernel \
  --kernel .state/out/x86_64/q35-uefi/shell/smp4-mem1g/kernel-x86_64 \
  --rootfs .state/out/rootfs/x86/rootfs-x86.img \
  --esp .state/out/x86_64/q35-uefi/shell/smp4-mem1g/kernel-x86_64.esp \
  --output .state/kvm-scheduler/thekernel \
  --cpus 4 --warmup 200 --iterations 2000 --repeat 5 \
  --vcpu-cpus 4,5 --io-cpus 6
```

`--vcpu-cpus` pins vCPU threads round-robin to the listed host CPUs;
`--io-cpus` pins the dedicated virtio-blk iothread. Both are optional. The
pinning wrapper records observed thread IDs and whether each requested class
was actually seen in `thread-pinning.json`; an unobserved thread is reported
as such, never as a successful pin.

The formal v5 pin report closes every observed TID with a `terminal_proofs`
record. Normal traced tasks use `ptrace-exit-stop`; KVM's exact
`kvm-nx-lpage-re` `PF_USER_WORKER` uses `kvm-vhost-stop` only after prearm,
housekeeping affinity, QEMU teardown, and departure from the original QEMU
thread group prove the vhost stop lifecycle. It is not represented as a
ptrace exit readback. The pinner automatically prearms unless
`/sys/module/kvm/parameters/nx_huge_pages` is exactly `never`; an unreadable
parameter is fail-closed (`UNSUPPORTED`) rather than assumed disabled.
While tracing, genuine QEMU signal-delivery stops (including vCPU shutdown
kicks) are resumed with their original signal; ptrace protocol traps and
group-stop boundaries are not reinjected as guest signals.

Each completed guest run also writes `host-pmu.json`.  It is collected through
Linux `perf_event_open`, without a `perf` executable, as a grouped generic
hardware-event window (`cycles`, `instructions`, `cache_misses`, and
`branch_misses`).  The events exclude host/guest kernel and hypervisor
execution, but explicitly leave `exclude_guest=false`: inherited QEMU vCPU
threads may contribute guest-mode user execution.  The group is attached to
the scheduler controller with child inheritance and spans that run's QEMU
launch, boot, workload, and shutdown.  It therefore includes
controller/pinner/QEMU descendants and is not host-only, guest-only,
QEMU-PID-exclusive, or per-sample measurement.  Its
`time_enabled_ns`, `time_running_ns`, multiplexing state, and scale are
retained.  `raw_counters` are deliberately unscaled raw event values, while
`counter_scaling=raw_with_scale_factor` makes their accompanying scale
explicit; an unavailable or failed counter has an explicit reason and is
never written as zero.  PMU values are intentionally absent from
`raw-samples.tsv` and quantile summaries.

The top-level scheduler manifest is v2 for this run-scoped PMU collection
semantics.  The raw guest TSV and its standalone statistical summary remain
v1 because their per-sample wire format is unchanged.  Each target's
`kernel`, `rootfs`, `esp` (when applicable), and `initrd` (when applicable)
manifest entries record their canonical path plus byte size and SHA-256; the
performance evidence is therefore bound to input contents, not a mutable
pathname.  After every run, the QEMU receipt's launch-time kernel/rootfs/ESP
evidence must match the target manifest and every target artifact is rehashed;
an input mutation rejects that run.  Linux initrd is likewise opened before
launch, passed through an inherited `/proc/self/fd/N` handle, and recorded in
the receipt's `launch_handles.initrd` source/post evidence.  The receipt's three-source
identity must also exactly match the declared source combination and every
checkout must be clean; dirty or mismatched source evidence rejects the run.
The scheduler captures that identity before launching any run, checks the
receipt's launch-time identity against it, and captures it again after every
run.  The final manifest retains `source_identity.preflight`,
`source_identity.postflight`, and their combination ID; any clean-state,
commit, tree, or repository-root change makes the run incomplete.  Ignored
`.state` output files do not make a checkout dirty because the identity uses
Git's normal porcelain status.

`latency-plus-run-scoped-pmu-prerequisite` means only that prerequisite raw
data was collected.  A collected run has `guest_status=ok`, but
`formal_performance_evidence=false` and
`performance_change_gate_eligible=false`: the inherited PMU counter never
substitutes for a per-sample witness.  Runs missing this PMU evidence are
`collection-incomplete` and fail collection; all requested runs being
`collected` returns success without claiming a performance change gate.

The per-run manifest also records a `latency-sum-derived` operations/sec
estimate when raw samples are complete: `sample_count / sum(latency_ns)`.  It
is a serial estimate, not wall-clock QEMU throughput and not a model of
concurrent workers.

The host must expose `/dev/kvm`, and every requested host CPU must be inside
the runner's current affinity mask. If KVM is unavailable the command exits
78 (`UNSUPPORTED`) without producing benchmark results.

## Linux adapter

The Linux adapter is symmetric at the workload/protocol layer but deliberately
does not discover or download an image. Supply a Linux x86_64 `bzImage` and
rootfs yourself; this lane uses QEMU direct-kernel boot rather than an ESP:

```bash
PYTHONPATH=. python3 -m tools.kvm_scheduler_baseline run \
  --target linux \
  --linux-kernel /path/to/arch/x86/boot/bzImage \
  --linux-rootfs /path/to/linux-rootfs.img \
  --output .state/kvm-scheduler/linux \
  --guest-program /opt/thekernel-tests/bin/thekernel-scheduler-baseline \
  --ready-marker THEKERNEL_SHELL_READY
```

The supplied Linux image must expose an interactive shell after the chosen
ready marker and contain the same helper path (or use `--guest-program` to
select another compatible binary). Missing Linux artifacts are an explicit
unavailable condition; the TheKernel lane remains runnable independently.

To recompute statistics without launching guests:

```bash
PYTHONPATH=. python3 -m tools.kvm_scheduler_baseline stats \
  --input .state/kvm-scheduler/thekernel/raw-samples.tsv \
  --output /tmp/kvm-summary.json \
  --summary-tsv /tmp/kvm-summary.tsv
```

These measurements compare this specified workload and topology only. They
are not a claim about bare-metal tail latency or overall performance.
