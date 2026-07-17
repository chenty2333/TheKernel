# Nightly system adapters

These adapters exercise a real guest path and return one of three outcomes:

- `0`: the named semantic checks ran and passed;
- `1`: infrastructure launched the check, but the check failed;
- `78`: required infrastructure or an explicit capability was unavailable.

Exit `78` is not a pass. `nightly-gate.sh` records it as `unsupported` and
returns `78` after all enabled categories have been considered.

The repository-owned adapters cover:

- `pressure.sh`: bounded concurrent task and root-filesystem pressure while a
  multi-threaded scheduler workload runs, on each selected architecture;
- `oom-failpoint.sh`: deterministic anonymous-mapping admission failure under
  strict overcommit policy, policy restoration, and a successful recovery
  mapping on each selected architecture;
- `fs-powercut.sh`: a writable ext4 image, a durable write, abrupt QEMU
  `SIGKILL` after an exact guest marker, a second recovery boot, clean unmount,
  and host `e2fsck` verification;
- `nonloopback-network.sh`: a nonce-authenticated TCP exchange from the guest
  VirtIO NIC through QEMU user networking to a one-shot host peer;
- `smp-tlb-shootdown.sh`: an RV/LoongArch matrix at 4 and 8 requested CPUs.
  Before mutating page tables, it pins two syscall-free, non-yielding spin tasks
  to every online CPU and requires both to advance in each of three consecutive
  one-second windows. For every non-control CPU it then warms translations
  remotely, keeps that worker in a syscall-free heartbeat spin across the
  page-table mutation, verifies its actual CPU before and after the critical
  accesses, and checks 1-page and 64-page `mprotect`, `munmap` plus fixed
  replacement, fixed `mremap`, and fork COW transitions. Any stalled liveness
  window, stale access, incomplete CPU/case matrix, topology mismatch, or
  operational failure rejects the run;
- `mm-performance.sh`: an RV/LoongArch matrix at 4 and 8 requested CPUs. It
  records VMA-scale mapping latency, `mremap` latency, an `mprotect` plus touch
  TLB-sensitive proxy, and regular-file direct-I/O pin latency, throughput, and
  concurrent contention. Every metric contains count/p50/p99/p999; unavailable
  paths remain explicit `missing` records with a reason and errno, and make the
  mandatory matrix fail instead of being reported as a completed baseline.

These gates deliberately do not overclaim. The OOM adapter does not substitute
for a future kernel-allocator failpoint framework or OOM-victim policy test.
The network adapter proves a real non-loopback NIC path but does not substitute
for TAP, packet loss, multi-peer, or physical-NIC testing. The power-cut test
models sudden VM process loss after explicit durable writes; storage devices
with volatile caches still require hardware-appropriate flush/fence testing.
The MM adapter verifies that the guest actually brought the requested CPU count
online and rejects a topology mismatch. Its protect-and-touch metric is a
user-visible TLB-sensitive proxy, not a hardware TLB-shootdown event counter.
Likewise, concurrent direct I/O is an end-to-end proxy that reaches the pin
path; it does not isolate time spent in one particular spinlock. The standalone
parser can still normalize an explicit `missing` record for diagnostic use,
but the nightly adapter requires all five metrics to be present.

The SMP TLB adapter is a semantic gate, not a hardware event counter. Its
qualification must include a mutation run from a disposable build in which
remote maintenance delivery is suppressed: at least one warmed remote access
must produce a `status=stale` result and the adapter must fail. Do not add that
fault injection as a production default or accept a clean pass from the
known-bad build. Timer preemption can still evict a translation naturally, so
the matrix covers every remote CPU and both a single hot page and a range; the
mutation run remains the proof that the workload can turn the intended defect
red. Validate that expected-failure log with
`validate-smp-tlb-log.sh LOG EXPECTED_CPUS stale`; this mode requires a complete
case matrix, at least one actual stale case, and an exact aggregate stale marker.
It rejects timeouts and operational failures.

`THEKERNEL_NIGHTLY_ARCHES` accepts `rv`, `la`, or `both` (the default). Missing
QEMU binaries, cross compilers, rootfs build tools, or filesystem tools cause
exit `78`. A runner may provide a category-specific `*_COMMAND` override for
hardware-only testing; its exit `78` retains the same unsupported meaning.

The MM matrix defaults to `THEKERNEL_MM_PERF_CPUS="4 8"`. Kernel build cache
identities include the CPU count from `THEKERNEL_KERNEL_CPUS` (or make's
`SMP`) and the complete maintained source trees used through Cargo path
dependencies. `THEKERNEL_QEMU_CPUS` controls the runner topology; the adapter
sets the product variables to the same value and the guest record provides the
third, runtime check. The rootfs is rematerialized through its content-addressed
builder so the guest helper cannot silently predate the checked-out source.

`mm-performance.sh` refuses to label a dirty checkout as exact-HEAD evidence.
Its manifest records the full TheKernel, `thekernel-ax`, and
`thekernel-linux-abi` commits; workload parameters; guest online topology;
kernel and rootfs SHA-256; QEMU path/version; and the immutable per-run kernel,
command, metrics, and log paths. It rechecks all three clean HEADs after the
matrix. The guest also completes fixed-destination replacement, shared
`old_size == 0` alias/coherence, and grow/shrink prefix-integrity checks before
emitting the semantic-pass marker.

`smp-tlb-shootdown.sh` applies the same clean-source rule to TheKernel,
`thekernel-ax`, and `thekernel-linux-abi`, forces a topology-specific kernel and
content-addressed rootfs rebuild for every matrix cell, and rechecks all three
repositories afterward. Its manifest records the three commits, requested and
guest-observed topology, kernel/rootfs SHA-256 values, QEMU path/version, and
per-run copies of the kernel, command stream, and complete console log. A
separate provenance receipt records the exact three repository commits at both
preflight and finalize.
