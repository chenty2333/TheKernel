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
  VirtIO NIC through QEMU user networking to a one-shot host peer.
- `mm-performance.sh`: an RV/LoongArch matrix at 4 and 8 requested CPUs. It
  records VMA-scale mapping latency, `mremap` latency, an `mprotect` plus touch
  TLB-sensitive proxy, and regular-file direct-I/O pin latency, throughput, and
  concurrent contention. Every metric contains count/p50/p99/p999; unavailable
  paths remain explicit `missing` records with a reason and errno.

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
path; it does not isolate time spent in one particular spinlock.
Explicitly missing metrics make an evidence capture complete, but do not prove
that the missing capability is implemented.

`THEKERNEL_NIGHTLY_ARCHES` accepts `rv`, `la`, or `both` (the default). Missing
QEMU binaries, cross compilers, rootfs build tools, or filesystem tools cause
exit `78`. A runner may provide a category-specific `*_COMMAND` override for
hardware-only testing; its exit `78` retains the same unsupported meaning.

The MM matrix defaults to `THEKERNEL_MM_PERF_CPUS="4 8"`. Kernel build cache
identities include the CPU count from `THEKERNEL_KERNEL_CPUS` (or make's
`SMP`), while `THEKERNEL_QEMU_CPUS` controls the runner topology; the adapter
sets the product variables to the same value and the guest record provides the
third, runtime check.

`mm-performance.sh` refuses to label a dirty checkout as exact-HEAD evidence.
Its manifest records the full TheKernel, `thekernel-ax`, and
`thekernel-linux-abi` commits; guest online topology; kernel and rootfs SHA-256;
QEMU path/version; and the per-run metrics/log paths. The guest also completes
fixed-destination replacement, shared `old_size == 0` alias/coherence, and
grow/shrink prefix-integrity checks before emitting the semantic-pass marker.
