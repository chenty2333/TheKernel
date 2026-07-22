# RFC 0008: Modern Performance, Driver, and Graphics Program

- Status: accepted
- Date: 2026-07-23
- Owners: TheKernel maintainers
- Target layers: architecture/HAL, generic task/MM/block/net/display mechanisms,
  Linux ABI-support crates, syscall adapters, and product/evidence profiles
- Implementation audit baseline:
  `7e41e62d7e4b3fc47f9048784ad65eb43e9c96f0`

## Intent

This RFC preserves the modernization strategy that follows the first Linux-ABI
contract program. It decides dependency order, adoption boundaries, and product
direction; it does not claim that every named mechanism is implemented.

TheKernel will optimize complete subsystems rather than benchmark shapes. The
OSComp suite remains a compatibility and fixed-cost regression probe. A
performance mechanism becomes default-on only after it preserves semantics
under pressure, has bounded resource and failure behavior, carries low-overhead
observability, and passes non-OSComp and dual-architecture gates.

The program has three concurrent outcomes:

1. a scalable kernel core with measured CPU, memory, storage, and network
   behavior;
2. a useful VM-first product that obtains broad hardware support through
   standardized VirtIO and host-side components; and
3. a safe policy-extension architecture that does not make eBPF, a dynamic
   native loader, or an unstable internal ABI a prerequisite for core
   correctness.

## Audited baseline

The implementation baseline already contains substantial Linux-visible and
resource-lifecycle work:

- extracted credential, VFS, FD, process, MM, io_uring, packet, and seccomp
  policy crates;
- IPI-backed RISC-V and LoongArch TLB and I-cache shootdown with grace-period
  ownership;
- mapping identities, transactional remap/protection work, COW, fault-around,
  bounded short pins, and a bounded userfaultfd MISSING profile;
- a bounded io_uring profile with shared rings, NOP, positioned read/write,
  one-shot poll, cancel, and fixed files;
- bounded seccomp and AF_PACKET ordinary-queue profiles; and
- source-exact CI and QEMU evidence machinery.

These are not production-completion claims. At this baseline:

- address-space switches still perform an entire-TLB flush on RISC-V and
  LoongArch and there is no address-space-ID generation allocator;
- SMP run-queue selection is round-robin rather than load-aware;
- the byte, page, and usage allocators retain global spin-locked state;
- there is no system-wide anonymous/file reclaim, pressure signal, swap, or
  mature OOM policy, and `MemAvailable` is still equivalent to `MemFree`;
- cached files use bounded per-file LRUs rather than a global pressure-aware
  page ownership and reclaim model;
- VirtIO block uses one queue of depth 16;
- the TCP/UDP stack retains a shared socket-set lock and polls until quiescent;
- eBPF has no JIT, BTF/CO-RE, general link lifecycle, XDP, or sched_ext-style
  attachment contract; and
- graphics is a VirtIO 2D framebuffer path, while the deterministic product
  runner remains headless and there is no DRM/KMS or render UAPI.

Local source state, retained test receipts, current exact-HEAD CI, packaged
consumer boots, registry publication, and an end-user release are separate
evidence authorities. One must never be used as a substitute for another.

## Decision 1: evidence closes before optimization claims

The MM performance runner is the first implementation checkpoint of this
program. Product measurements and diagnostic attribution are separate
profiles. Product runs must not compile or collect hot-path lock diagnostics.
Diagnostic runs may attribute stages and distribution shape, but their latency
values are not regression evidence.

The maintained matrix includes RISC-V and LoongArch at 4 and 8 online CPUs,
source-exact kernel/rootfs/command/runner receipts, p50/p99/p999 distributions,
counterbalanced baseline/candidate pairs, and explicit missing evidence.
QEMU TCG establishes correctness and relative regression evidence; absolute
performance and architectural event claims require real hardware and PMU
receipts.

Future observability proceeds in this order:

1. RISC-V SBI and LoongArch PMU counters;
2. typed, versioned static tracepoints;
3. per-CPU bounded overwrite rings;
4. stable eBPF link, per-CPU map, and ring-buffer lifecycles;
5. BTF/CO-RE; and
6. independently validated RISC-V and LoongArch JITs.

## Decision 2: remove CPU foundations before replacing CFS

The next CPU scalability dependency is an ASID generation allocator and
precise invalidation, not a new fair-scheduling policy. The implementation must
cover:

- non-wrapping address-space identity and bounded architecture ASID reuse;
- local and remote invalidation by address and ASID where supported;
- rollover generation, grace, CPU online/offline, and failed-CPU behavior;
- fork, exec, address-space teardown, and page-table retirement; and
- context-switch and syscall-workload evidence on both architectures.

After this foundation, scheduling proceeds through load-aware wake placement,
idle pull or bounded work stealing, and deterministic event tests before an
EEVDF policy is considered. Linux EEVDF's lag, eligibility, and virtual
deadline model is an algorithmic reference, not source code to copy. The
[`kSTEP`](https://www.usenix.org/conference/osdi26/presentation/cao)
methodology is the preferred reference for isolated-CPU, repeatable scheduler
events and trace-driven fuzzing.

## Decision 3: global page ownership precedes advanced reclaim policy

The MM does not adopt MGLRU, DAMON actions, or transparent huge-page promotion
until it owns the mechanisms those policies control. The prerequisite slice
provides:

- unified anonymous and file-page ownership and accounting;
- accessed, dirty, writeback, mapped, pinned, and refault state;
- watermarks and direct/background reclaim with bounded progress;
- dirty throttling and background writeback;
- a pressure model and honest available-memory estimate;
- OOM selection and failure behavior; and
- teardown, truncate, COW, pin, and page-table invalidation integration.

Linux
[`MGLRU`](https://docs.kernel.org/admin-guide/mm/multigen_lru.html) and
[`DAMON`](https://docs.kernel.org/mm/damon/index.html) are clean-room policy
references after that slice exists. The first DAMON-like checkpoint is
read-only sampling and heat reporting; actions remain disabled until reclaim
and migration are safe.

[`CortenMM`](https://web.cs.ucla.edu/~tamir/papers/sosp25.pdf) is retained as
an experimental reference for transactional MMU interfaces, scalable locking,
and formal specifications. Its one-level design does not justify deleting
TheKernel mapping identities or Linux-visible VMA/file-offset/COW/pin state.

## Decision 4: queue locality precedes bypass

Storage follows the Linux blk-mq separation of per-CPU software staging,
hardware dispatch queues, and tag-based completion while preserving TheKernel
ownership and error types. The immediate VirtIO goals are negotiated
multiqueue, queue-local admission, IRQ affinity, event-index/notification
suppression, completion tags, flush/fence correctness, and reset/teardown.

Networking follows this dependency order:

1. interrupt-to-bounded-poll handoff;
2. VirtIO-net multiqueue;
3. IRQ affinity and receive steering;
4. queue-local recyclable page/buffer pools;
5. checksum and segmentation offloads; and
6. only then XDP, AF_XDP, or io_uring zero-copy receive experiments.

Linux
[`blk-mq`](https://docs.kernel.org/block/blk-mq.html),
[`NAPI`](https://docs.kernel.org/networking/napi.html),
[`page_pool`](https://docs.kernel.org/networking/page_pool.html), and
[`AF_XDP`](https://docs.kernel.org/networking/af_xdp.html) are semantic and
mechanism references. Their GPL kernel implementations are not imported into
the Apache-2.0 TheKernel tree.

The current bounded io_uring profile is extended in dependency order.
Official [`liburing`](https://github.com/axboe/liburing) and its regression
tests are preferred over a TheKernel-specific asynchronous userspace API.
Registered buffers require long-term pin, accounting, invalidation,
revocation, and late-completion rules first. Linux
[`ublk`](https://docs.kernel.org/block/ublk.html) is a reference for a future
isolated user-space block service and its tag, crash, recovery, reissue, and
fail policies.

## Decision 5: keep portable eBPF and trusted Rust policies distinct

eBPF remains the portable Linux-compatible plane for trace, filter, and small
policy programs. Core kernel performance does not depend on completing Linux's
entire BPF ecosystem. sched_ext and XDP remain experiments until stable links,
BTF, helper capabilities, watchdog/fallback behavior, and the underlying
scheduler or network mechanisms exist.

TheKernel also studies a native trusted Safe-Rust policy plane inspired by
[`Rex`](https://www.usenix.org/conference/atc25/presentation/jia):

- extensions receive sealed typed capabilities rather than internal pointers;
- helper calls have explicit blocking, allocation, interrupt, and lifetime
  contracts;
- RAII accounts and releases every retained resource;
- panic cleanup, stack bounds, execution budgets, and fallback are mandatory;
  and
- artifact identity includes target architecture, toolchain, helper ABI, and
  policy version.

The first checkpoint is statically linked policy traits over stable helper
contracts. It is not a dynamic native module loader. Dynamic signed artifacts,
version negotiation, and unload grace require a later RFC. Hardware drivers do
not share this policy trust domain.

## Decision 6: VM-first is the primary product architecture

The near-term supported product is a RISC-V and LoongArch VM/server appliance.
Linux or the hypervisor owns physical-device diversity; TheKernel owns its
VirtIO front ends, Linux-visible UAPIs, resource lifetimes, diagnostics, and
failure semantics.

Driver reuse follows this order:

1. permissively licensed Rust/ArceOS mechanisms and standard VirtIO;
2. a TheKernel user-space DriverKit after MSI-X, IOMMU, DMA pin/revoke, reset,
   and device ownership exist;
3. a Linux driver domain or VM exporting a narrow VirtIO/vhost-user service;
4. a tailored compatibility environment for one frozen Linux subsystem,
   version, architecture, and device family when a product requirement and
   maintenance owner exist; and
5. no general claim that Linux `.ko` modules load unchanged.

Linux userspace ABI compatibility is not Linux internal driver KPI
compatibility. A Linux driver normally depends on the device model, devres,
PCI capabilities, MSI/MSI-X and IRQ domains, DMA/IOMMU/scatterlists, RCU,
workqueues, firmware and power management, reset/hotplug, and subsystem
frameworks such as netdev/NAPI, blk-mq, USB, or DRM/GEM/TTM/dma-buf.

The DriverKit object model takes design guidance from Linux
[`IOMMUFD`](https://docs.kernel.org/userspace-api/iommufd.html) and Fuchsia
[`DFv2`](https://fuchsia.dev/docs/concepts/drivers/driver_framework): explicit
device ownership, I/O address spaces, revocable mappings, IRQ events, reset,
node topology, driver hosts, and capability routing. GPU is not the first
user-space DriverKit device; an NVMe or NIC proof has fewer MM, synchronization,
firmware, and scheduling dependencies.

## Decision 7: reuse the virtual graphics ecosystem through DRM UAPI

Graphics progresses through independent display and render slices:

1. make the current 2D framebuffer an opt-in, deterministic product topology
   with EDID/mode/input, damage-aware update, screenshot, and teardown tests;
2. implement generic display objects and a Linux DRM/KMS display profile with
   card node, dumb buffers, mmap, connector/CRTC/plane, vblank/page-flip, and
   atomic test/commit semantics;
3. implement the render profile: contexts, capsets, resources, blobs,
   host-visible memory, fences/timelines, GEM handles, render node, and sync
   objects; and
4. consume Mesa VirGL first, then Venus, gfxstream, and finally selected DRM
   native-context paths when each preceding contract passes.

Current QEMU
[`virtio-gpu`](https://www.qemu.org/docs/master/system/devices/virtio/virtio-gpu.html)
backends, Mesa
[`VirGL`](https://docs.mesa3d.org/drivers/virgl.html) and
[`Venus`](https://docs.mesa3d.org/drivers/venus.html), virglrenderer, and
Rutabaga are reused on the host or in guest userspace under their respective
licenses. TheKernel does not implement a private OpenGL or Vulkan stack and
does not begin with a direct AMDGPU, i915, Nouveau, or Nova port.

The vendored `virtio-drivers` fork is rebased only through an explicit patch
ledger. Upstream releases are useful baselines, but an upstream version bump
does not by itself provide VirGL, Venus, DMA revocation, or a complete DRM
contract.

## Adoption classes

Every external result is assigned one class before implementation:

- **Direct component or standard**: a stable protocol or license-compatible
  component can be used at its proper host, userspace, or kernel layer after
  provenance and dependency review.
- **Clean-room mechanism**: use published semantics, algorithms, and tests but
  implement TheKernel ownership and failure rules without copying incompatible
  kernel source.
- **Experimental branch**: measure a research design behind a non-default
  interface; it cannot become a release dependency from paper results alone.
- **Deferred**: the prerequisites, maintenance owner, architecture evidence,
  or license boundary are absent.

Examples of direct adoption include ISA/SBI specifications, liburing tests,
permissive VirtIO libraries, and host/userspace graphics components. Clean-room
references include EEVDF, MGLRU, DAMON, blk-mq, NAPI, sched_ext fallback, and
AF_XDP ring ownership. CortenMM, PageFlex, dynamic Rex-style modules, XDP,
sched_ext, and zero-copy receive begin as experiments. General LinuxKPI,
unchanged `.ko` loading, and direct physical GPU ports are deferred.

## Layer ownership

- Layer 0 owns PMU, ASID/TLB, IOMMU, interrupt, MSI-X, DMA translation, and
  architecture cache-coherence primitives.
- Layer 1 owns generic allocators, scheduling substrate, reclaim, block/net
  queues, buffer pools, VirtIO ownership, display objects, fences, and bounded
  diagnostics.
- Layer 2 owns Linux MM, io_uring, packet, BPF, driver-facing, DRM/KMS, and
  render UAPI policy over those mechanisms.
- Layer 3 copies and validates user arguments, maps typed errors, and composes
  file/task/MM objects; it does not implement device or reclaim policy.
- Layer 4 owns QEMU topology, benchmark matrices, feature profiles, and
  evidence collection. It cannot alter kernel semantics based on executable or
  workload identity.

## Validation and release gates

Each performance or product slice must establish:

- semantic and failure tests before performance comparison;
- bounded admission, accounting, cancellation, close, and teardown;
- RISC-V and LoongArch compilation and runtime evidence;
- 1-, 4-, and 8-CPU coverage where the mechanism is SMP-relevant;
- low-memory and pressure behavior where pages or buffers are retained;
- p50 and p99 latency plus throughput and resource-usage guardrails;
- non-loopback networking and non-memory-backed storage when making data-plane
  claims;
- diagnostics disabled in authoritative product measurements;
- exact source, binary, rootfs, topology, command, runner, and log identity;
  and
- a clean exact-HEAD public CI and packaged-consumer boot before release
  language is used.

## Explicit non-goals

This RFC does not claim:

- that QEMU TCG numbers represent physical hardware performance;
- that a Linux UAPI implementation makes Linux internal drivers reusable;
- that a paper's peak speedup transfers to TheKernel;
- that eBPF, Safe-Rust policies, DriverKit, DRM, or GPU acceleration is already
  implemented;
- that every mechanism must be lock-free, RCU-based, or dynamically
  programmable; or
- that evaluator-specific behavior may enter default kernel policy.

## Implementation checkpoint: first bounded foundations

The first bounded implementation slice landed on 2026-07-23.  Its source
anchors are TheKernel `2d1d2be7110cc55526054e3e94a03715d7c721fb`,
`thekernel-ax` `da0a5f9b861dd8e363f5b633d008e9ac6c34bc40`, and
`thekernel-linux-abi` `df13793f1c372432d0c5144d44636dfd53ccec45`.
This checkpoint narrows several immediate bottlenecks, but does not supersede
the dependency order or release gates above.

### ABI contracts and evidence

The existing bounded io_uring, userfaultfd, seccomp, and AF_PACKET profiles
remain deliberately incomplete.  This slice adds an io_uring adapter test to
the per-commit gate and validates that every RFC index status matches its
document.  It expands evidence, not Linux-visible capability: registered
buffers, io-wq, SQPOLL, multishot io_uring operations, packet mmap/TPACKET,
AF_XDP, userfaultfd write protection, and line-rate capture remain outside the
implemented profiles.

### ASID pilot

`asid-fast-switch` is an opt-in RISC-V and LoongArch pilot and is off by
default.  It probes the architecture width, assigns a unique nonzero numeric
ASID for the rest of the boot, and retains translations only between valid
nonzero identities in the same non-reused boot generation.  ASID 0, an
unexpected width, exhaustion, a generation mismatch, or the same numeric ASID
with a different root takes the full-flush path.  Generic page-table cursors
remain all-ASID conservative because they do not yet carry owner scope.

This is not the complete generation allocator required by Decision 2.  There
is no numeric reuse, rollover, CPU-hotplug quiescence protocol, or performance
counter for avoided flushes.  RISC-V can assign at most 65,535 nonzero IDs and
LoongArch at most 1,023 under the supported architectural widths; after
exhaustion, every new address space permanently falls back to ASID 0.  The
existing remote shootdown and delayed-retirement contracts remain
authoritative.

### Clean file-cache pressure baseline

The first pressure worker uses low/high watermarks and bounded persistent
registry and per-inode cursors.  It can reclaim only clean, disk-backed,
unmapped file-cache pages.  Dirty, pinned, writeback, mapping-listener,
in-memory/tmpfs, and lock-busy candidates are skipped.  Work per wake, registry
walks, inode scans, reclaim batches, and no-progress retry intervals are all
bounded.  `/proc/memory_pressure` exposes versioned progress and exclusion
counters, while explicit `/proc/meminfo` collection estimates `MemAvailable`
from free pages plus a conservative clean-cache estimate above the low
watermark.

This remains background-only file-cache reclaim.  It provides no direct
allocation retry, anonymous reclaim, swap, PSI, mature OOM selection, dirty
throttling, mapped-file reclaim, or unified page ownership.  MGLRU, DAMON
actions, and transparent huge-page policy therefore remain later work.

### Load-aware placement before EEVDF

Initial task placement and affinity-forced migration now use one bounded scan
of at most `MAX_CPU_NUM` entries, filter by affinity and initialized run queues,
and select the lowest advisory runnable load with a rotated deterministic tie.
Per-CPU ready and non-idle-running counts are available as lock-free diagnostic
snapshots.  In the current no-hotplug system, an initialized run queue is the
online-CPU proxy.

An ordinary wake stays on its affinity-allowed source CPU.  Only an affinity
change that excludes that source invokes the load-aware selector.  This avoids
turning every wake into a remote enqueue before a remote-preemption contract
exists.  Idle stealing, general wake balancing, NUMA placement, and CPU
hotplug are not implemented.

EEVDF does not replace this cross-CPU placement mechanism: it changes which
eligible task one run queue selects.  A later opt-in EEVDF slice may replace
the simplified CFS policy only after it provides an allocation-free augmented
tree, lag and virtual-deadline arithmetic, sleeper and reweight semantics,
migration invariants, a reference model, and watchdog/fallback gates.  The
maintained readiness design is
[`thekernel-ax/docs/design/0001-eevdf-readiness.md`](https://github.com/chenty2333/thekernel-ax/blob/da0a5f9b861dd8e363f5b633d008e9ac6c34bc40/docs/design/0001-eevdf-readiness.md).

### Integration evidence and remaining gate

The source-equivalent pre-commit integration runs completed the full system
test on RISC-V 2 CPU/128 MiB and LoongArch 2 CPU/256 MiB with the default ASID
path, on both architectures with the opt-in ASID path, and on RISC-V 4 CPU/
1 GiB with the opt-in path.  The last profile exercises the maximum supported
pressure-test memory and the multi-CPU placement topology.  Focused host
evidence covered 167 MM tests, six ASID allocator tests, three context-switch
predicates, six file-cache pressure tests, the build-tool suite, and the CI
script suite.

These local runs are integration evidence, not the final clean exact-HEAD
release receipt.  The exact-HEAD per-commit gate, dual-architecture SMP TLB
stress, physical-hardware counters, and product performance comparison remain
required before the ASID pilot can become default-on or any speedup is claimed.
An earlier experimental remote-wake placement amplified a nondeterministic
QEMU-TCG `RLIMIT_CPU` latency failure; that policy was removed.  The retained
source-local wake policy passed the final integration matrix, while the timer
path remains a separate diagnostic target rather than evidence for EEVDF or
idle stealing.
