# RFC 0004: Accounted VM Pins and Generation-Safe Fault Delegation

- Status: draft
- Date: 2026-07-12
- Owners: TheKernel maintainers
- Target layers: generic VM/frame mechanisms and `thekernel-linux-mm`

## Problem

A raw physical-page reference is not a sufficient user-memory pin. Correct
pinning must define what happens when the VMA is unmapped, protected, forked,
COW-broken, truncated, invalidated by a filesystem, or torn down while I/O is
still in flight. Writable file-backed pins also interact with dirty tracking
and writeback.

Fault delegation has the inverse lifetime problem. A fault may sleep while an
external handler supplies a page, but the mapping that issued the request may
be replaced before the reply arrives. Completing by address alone can install
a page into the wrong mapping. Queues, outstanding requests, and blocked tasks
must also be bounded and cancelled on unmap, close, and process exit.

TheKernel has RAII frame/page-cache pins and range-pin tokens, but the public
policy contract is not stable enough to publish. It has no complete
userfaultfd-style broker, MMU-notifier equivalent, or long-term pin accounting
model yet.

## Evidence reviewed

The design was checked against these exact source snapshots on 2026-07-12:

- Linux `44696aa3a489d2baf58efa61b37833f100072bee`:
  - `mm/gup.c` and `Documentation/core-api/pin_user_pages.rst` for the
    `FOLL_GET`/`FOLL_PIN` split, long-term-pinnable restrictions, explicit
    unpin/dirty APIs, COW unsharing, and writable file-mapping hazards;
  - `include/linux/mmu_notifier.h` and `mm/mmu_notifier.c` for range
    invalidation, release, sequence/retry, non-blockable notification, and
    interval subscriptions;
  - `fs/userfaultfd.c`, `mm/userfaultfd.c`, and
    `include/uapi/linux/userfaultfd.h` for registered ranges, missing/minor/WP
    modes, fault/event queues, pollability, `mmap_changing`, copy/zeropage/
    continue/write-protect resolution, wake, fork/remap/remove/unmap events,
    and close teardown;
  - `lib/maple_tree.c` for preallocation, range indexing, RCU reads, and the
    complexity cost of a highly optimized VMA index.
- Fuchsia/Zircon `8fe57fc696e6ccd1d8f7f48959116d17db467eaa`:
  - `zircon/kernel/vm/page_source.cc` and
    `zircon/kernel/object/pager_dispatcher.cc` for owned page requests,
    overlapping-request coalescing, supply/fail completion, continuation,
    cancel, detach, and close;
  - `zircon/kernel/vm/pinned_vm_object.cc` and `vm_object_paged.cc` for RAII
    pin ownership, fallible commit-before-publication, pinned/resizable
    exclusions, COW-backed objects, and teardown.
- Asterinas `37411049265056135a5e18c8c75a0c3d16b18579`:
  - `kernel/src/vm/vmar/vmar_impls/{page_fault,fork}.rs` and
    `kernel/src/vm/vmar/interval_set.rs` for Rust-owned VMA operations, COW
    fork/fault handling, and a simpler ordered interval structure;
  - `ostd/src/mm/vm_space.rs` for range cursors, explicit TLB flushing, and RCU
    deferred frame/page-table destruction.
- TheKernel commit `c52dc6f` and current `kernel/src/mm/access.rs` for the
  existing physical-frame, page-cache, pin-window, and address-space range
  guards used by direct I/O.

Linux source is GPL-2.0-only, Zircon source uses an MIT-style license, and the
reviewed Asterinas files are MPL-2.0. This RFC adopts contracts and ownership
ideas without copying source.

## Decision

### 1. Split generic mechanism from Linux policy

The generic ax/VM layer owns:

- frame and page-cache pin counters with RAII release;
- mapping identity/generation and range leases;
- page-table range cursors, TLB invalidation, and deferred destruction;
- generic invalidation observers;
- an owned, bounded fault-request broker primitive;
- physical/page-cache segment descriptions for drivers.

`thekernel-linux-mm` owns:

- `mmap`, `mprotect`, `munmap`, `mremap`, `brk`, fork, exec, COW, and
  file-backed mapping semantics;
- Linux-visible pin admission across permissions, COW, long-term use,
  file/writeback, truncate, and resource limits;
- usercopy/fault-in policy and partial-fault error behavior;
- userfaultfd registration, events, resolution commands, readiness, and errno;
- interactions with credentials, VFS objects, FD readiness, signals, and
  process teardown.

Architecture/HAL code owns only page-table/TLB primitives. Syscalls decode and
copy arguments around the Linux MM operations.

### 2. Typed, owned pin request

A pin is requested with explicit intent:

```rust
pub struct PinRequest {
    pub range: UserRange,
    pub access: PinAccess,
    pub duration: PinDuration,
    pub use_kind: PinUse,
    pub owner: PinOwner,
}
```

The concrete API keeps fields private where invariants require it. It
distinguishes short synchronous access, asynchronous I/O, and long-term/DMA
pins. Read and write intent are separate. Every successful result owns:

- the address-space and mapping generation lease;
- exact physical-frame and/or page-cache pins;
- scatter/gather segments;
- owner/address-space accounting tokens plus one system-wide budget charge;
- dirty-on-release state for writable pins;
- invalidation/cancellation linkage when the use permits revocation.

Drop releases in the reverse order outside spin/page-table locks. Partial
construction rolls back every frame, page-cache, range, and accounting token.

### 3. Pin admission and COW

Pinning follows a check-fault-revalidate sequence:

1. validate arithmetic, userspace bounds, alignment, and requested access;
2. reserve system-wide and per-owner/address-space pages, bytes, and tokens;
3. snapshot mapping identities and generations;
4. fault pages in without holding the VMA index or page-table lock across
   blocking work;
5. break COW before publishing any writable pin;
6. obtain frame/page-cache pins;
7. revalidate mapping identity, access, and generations;
8. publish the complete pin or roll back and retry within a bounded budget.

A read pin cannot be silently upgraded to write. A writable anonymous/private
mapping is exclusive to the pinned address space before exposure. A fork may
share unpinned COW state but must preserve or explicitly reject the semantics
of an existing pin.

### 4. Long-term and file-backed restrictions

Long-term pins are a separate capability, not a longer timeout:

- they require a configured owner limit and global accounting;
- movable/reclaim-dependent frames and unsupported mappings are rejected;
- truncate, hole punch, writeback, and filesystem invalidation have an
  explicit block, revoke, or fail contract;
- writable file-backed long-term pins are rejected until the filesystem can
  preserve dirty/write-notify and stable-write semantics;
- async block/network operations retain the pin until completion or verified
  cancellation, including late device completion;
- unpin marks affected file-backed pages dirty when required before releasing
  the last write pin.

The initial fast path may fall back to a copied buffer. It must never claim a
pin while retaining only an unaccounted frame reference.

### 5. Invalidation observer contract

Mapping mutation publishes a typed invalidation range containing address-space
identity, old mapping generation, range, and reason. Observers are registered
before a device/request can use translated addresses.

The sequence is:

1. begin invalidation and prevent new translations for the old generation;
2. notify observers that can respond without blocking;
3. wait or return an explicit retry/error where revocation requires sleep;
4. change mappings and flush TLBs;
5. complete invalidation and release deferred objects after readers are safe.

Callbacks do not allocate under page-table locks. RCU or epoch reclamation may
implement safe readers later; sequence/retry and release ordering are the
stable contract.

### 6. VMA index remains an implementation detail

0.1 exposes ordered range operations, overlap detection, split/merge,
generation, cursor, and snapshot semantics. It does not expose a Maple Tree or
any node layout.

The initial implementation may use the current ordered map/interval set. A
Maple-Tree-like or augmented interval structure is adopted only if dual-arch
profiles show VMA lookup/update is material and its preallocation, iterator,
RCU, and failure contracts can be expressed safely in Rust.

### 7. Fault broker

The generic broker uses an owned request identity:

```rust
pub struct FaultKey {
    pub address_space: AddressSpaceId,
    pub mapping: MappingId,
    pub fault_epoch: MappingGeneration,
    pub page_address: FaultPageAddress,
    pub access: FaultAccess,
}
```

The page identity is absolute rather than VMA-relative, so an unchanged
request survives `mprotect` splits and a partial-unmap survivor whose VMA start
moves. The consumer-owned fault epoch remains stable across those topology
changes and changes only when mapping or registration authority is replaced.

One request records type, range, credential/security decision, cancellation
state, waiter ownership, and a completion token. Overlapping requests may
coalesce only when mapping identity, fault epoch, absolute page, access, and
handler match.
Every waiter remains independently cancellable.

The broker has bounded per-address-space and per-handler queues plus global
accounting. Admission occurs before a task sleeps. It can complete with page
supply, zero fill, continue, write-protect change, typed failure, cancellation,
or handler detach. A late reply to a stale generation is rejected and cannot
modify the new mapping.

### 8. userfaultfd adapter

`thekernel-linux-mm` maps Linux registration modes and commands onto the
broker. It preserves:

- one-write validated range registration and non-overlap rules;
- missing/minor/write-protect mode support only when genuinely implemented;
- poll/read readiness without hidden scanning;
- fork/remap/remove/unmap event ordering;
- `mmap_changing` serialization around topology mutation;
- copy/zeropage/continue/write-protect atomicity and partial-result rules;
- wake versus do-not-wake behavior;
- close/exec/mm teardown that cancels or fails every request and wakes every
  waiter.

The FD adapter owns queue readiness; the broker does not depend on the Linux FD
crate. This prevents an MM/FD dependency cycle.

0.1 may publish the broker seam while returning `ENOSYS` for userfaultfd. It
must not advertise a userfaultfd feature until the complete adapter and
teardown tests pass.

### 9. Fault execution and unsafe boundary

Architecture trap code produces a typed fault and calls the address-space
handler. The handler snapshots the VMA, drops locks before filesystem/pager
I/O or sleeping, resolves COW/file/delegated backing, then revalidates before
page-table publication.

Unsafe code is confined to page-table/HAL operations and raw user-memory
adapters. Public MM policy uses owned ranges, frames, and initialized buffers;
user-triggerable alignment/range errors return errors rather than panicking.

## 0.1 implementation checkpoint

As of 2026-07-16, the extracted 0.1 boundary is deliberately narrower than
the complete design in this RFC. `thekernel-linux-mm` now exists as a `no_std`,
`unsafe`-free policy crate. It owns:

- checked user/page ranges and page-size arithmetic;
- typed address-space and mapping identities, generations, snapshots, and
  invalidation reasons;
- bounded per-owner/address-space pin quotas plus a fixed-capacity system-wide
  budget, reservation/publication, mutation admission, release, and teardown
  accounting;
- remap geometry, affine relocation, page-covering, and memlock planners;
- typed fault admission and stale-completion validation seams; and
- bounded Linux v6.12 MISSING-registration policy, canonical partial-register
  deltas, mixed-owner mapping replacement, fault-epoch projection, and checked
  COPY/ZEROPAGE result policy.

TheKernel consumes the packaged crate without moving generic frame, page-table,
or page-cache mechanisms into syscall code. The current consumer checkpoint:

- admits direct-I/O pins only for already-resident 4 KiB pages with the
  requested hardware access; missing pages and unresolved writable COW return
  to the ordinary copy/fault path;
- obtains exact RAII frame or page-cache pins, releases conservative file-wide
  preparation windows before publication, revalidates the mapping generation,
  and keeps both the policy range token and aggregate system charge until all
  lower pins are released;
- uses a fallibly preallocated, system-bounded physical-frame pin table so the
  IRQ-disabled lookup/update path performs no allocation; the table has 64
  independent shards and each transaction covers at most 64 pages, rather than
  taking one system-wide lock once per page;
- reserves the complete user range before lower pinning, then captures mapping
  expectations, scans PTEs, owns lower pages, and revalidates in windows of at
  most 64 pages; the reservation is the overlapping-mutation fence between
  windows and the final token publication is an O(1) state transition;
  allocation and deferred owner destruction stay outside both the address-space
  lock and physical-registry shard locks;
- indexes logical mapping identity by lineage and invalidates sorted, coalesced
  ranges with one forward sweep, so unrelated VMA changes no longer invalidate
  every mapping through one address-space topology generation;
- marks writable page-cache pins dirty on release and rejects overlapping
  truncate/remap/protect/unmap mutations while a pin is active;
- uses transactional COW clone and moving-`mremap` rollback for destination
  PTEs, frame references, source flags, locks, and topology publication;
- uses owner-acknowledged page-cache invalidation in `axfs-ng`: pages are
  staged before lower mutation, every address-space listener must acknowledge
  detachment, foreign-owner contention restores the staged cache state, and
  only the populating owner may defer its own detach until `PopulateOutcome`
  completion;
- has a dormant, per-address-space userfaultfd adapter with fixed handler,
  registration, request, waiter, and readiness capacities. REGISTER/UNREGISTER
  are all-or-none registration-table transactions only; neither operation is a
  main-MM/VMA transaction. Ordinary `munmap`/`mprotect` sidecars are preflighted
  before the main MM transaction and committed only after it succeeds.
  `mprotect` uses RAII to abort an uncommitted sidecar, while `munmap` explicitly
  aborts its copy-only plan on every recoverable MM failure; and
- projects `mprotect` through MemorySet's exact touching/flags/lineage/backend
  merge law. A split preserves every source fragment and its fault epoch; a
  permission restore canonically folds compatible fragments back to one in a
  single mixed-owner transaction. Different handlers, fault epochs, modes,
  holes, page geometry, or actual post-VMA boundaries remain separate, and a
  partial source projection fails closed;
- freezes both fixed-`mremap` sidecar outcomes before destroying a destination.
  A fixed move owns a destination-only failure plan and a destination-plus-
  source success plan in two bounded slots; the transaction aborts the
  unchosen plan before committing the chosen plan. A fixed duplicate retires
  only destination authority, a nonfixed move retires source authority only
  after commit, and a nonfixed duplicate changes no registration. Moved or
  duplicated destinations remain unregistered because this checkpoint does not
  advertise `UFFD_FEATURE_EVENT_REMAP`; and
- extends a boundary registration only when the same logical mapping grows in
  place or through `MAP_GROWSDOWN`. The extension preserves the registration
  fault epoch and is published before a growdown fault retry. A `brk` tail is
  deliberately unregistered even when MemorySet merges it into the same VMA
  and backend. Population rollback reports whether the new range was preserved
  or remains published, so registration authority and the Linux-visible break
  never describe a range different from the concrete VMA state.

Linux v6.12 fixed `mremap` unmaps the replacement destination and, for
`old_size > new_size`, the old source tail before later move validation can
fail. TheKernel currently completes its destination staging transaction before
retiring any source byte, so such a failure preserves the complete source while
still possibly retiring the old destination. The two preflighted sidecar plans
intentionally follow TheKernel's concrete MM outcome: the failure plan removes
only destination authority. This is a stronger rollback guarantee, not a claim
of exact Linux failure-state parity. Source-tail authority must not be retired
on failure unless the main VMA transaction is first changed to publish that
same effect and its typed outcome is extended accordingly.

The current address-space consumer also has two independent, per-address-space
resource ceilings: 65,536 live logical mapping lineages and 65,536 live VMA
fragments. A protection split consumes a fragment but not another lineage.
Admission fails with `ENOMEM` before publication when either ceiling would be
exceeded. These are fixed TheKernel bounds, not a claim to implement Linux's
runtime-tunable `vm.max_map_count`; exposing a compatible control requires a
separate policy and accounting decision.

This checkpoint does not implement fault-in pinning, long-term/DMA pins,
revocation or an MMU-notifier equivalent. The bounded generic broker and
dormant address-space registration adapter are not yet connected to page-fault
delegation or a published userfaultfd FD/syscall. COPY/ZEROPAGE installation,
complete event/teardown behavior, fork/remap/remove events, and Linux
differential/runtime gates remain open. `userfaultfd` therefore remains
unsupported and this RFC remains `draft`.

The correctness checkpoint is not a claim that the direct-I/O pin path is
performance-complete. Each 64-page lower-pin/revalidation window still performs
bounded VMA, PTE, page-cache, and physical-registry work while holding the
address-space lock, and a pathological physical address distribution can
exhaust one fixed shard before the aggregate logical pin budget, forcing the
semantics-preserving copied fallback. Shared-mapping growth and file-registration
paths also retain serialized preparation. The pinned async direct path remains
default-off.

The repository-owned RISC-V/LoongArch 4/8-CPU matrix records VMA-scale,
`mremap`, protect-and-touch, direct-pin throughput, and concurrent direct-pin
P50/P99/P999 baselines with exact source and artifact identities. Those are
user-visible proxies and regression checkpoints: protect-and-touch is not a
hardware TLB event counter, and direct-I/O latency does not isolate a single
registry lock. A first clean baseline therefore closes the missing evidence
checkpoint without justifying CortenMM-style RCU page tables, production tail
latency claims, lock-free pinning, or production hardware scalability.

## Rejected alternatives

- Equating an `Arc<Frame>` with a pin: it does not freeze mapping/COW or account
  long-term use.
- Letting `munmap`/`mprotect` proceed while an I/O path retains raw translated
  addresses: creates use-after-remap and device corruption risks.
- Address-only fault completion: a late response can target a replacement VMA.
- Unbounded userfault queues or one kernel thread per request: violates
  pressure and teardown requirements.
- Copying Linux GUP flags or Maple Tree node layout into the public Rust API:
  freezes internals rather than semantics.
- Default-on lock-free/RCU VMA indexing before measurement: complexity is not
  justified until the simple implementation is profiled.

## Validation gates

### Pin semantics

- read/write, zero-length, unaligned, partial fault, COW, shared/private,
  anonymous/file-backed, and scatter/gather cases;
- pin racing `mprotect`, `munmap`, fork, COW fault, truncate, hole punch,
  writeback, and address-space teardown;
- long-term/revocable admission, accounting, quota, dirty-on-unpin, and copied
  fallback;
- failpoint at every allocation/pin stage leaves all counters and mappings
  unchanged.

### Mapping and fault semantics

- mmap flag/protection matrices, split/merge, fixed replacement, mremap, brk,
  private/shared file coherence, SIGSEGV versus SIGBUS, and fork/exec teardown;
- stale-generation pager replies, overlapping/coalesced requests, partial
  supply, failure, cancellation, handler detach, and mm close;
- userfaultfd missing/minor/WP, events, poll/read, copy/zero/continue/WP, wake,
  fork/remap/remove/unmap, and queue/account limits.

### Concurrency, architecture, and performance

- SMP stress with concurrent faults, unmap/protect/fork, I/O completion, and
  handler close;
- RISC-V and LoongArch page-table/TLB behavior and TheKernel boots;
- LTP mmap/fork/userfault coverage plus fsx, stress-ng, and targeted pressure;
- VMA lookup/fault/COW/pin/unpin latency and memory overhead with diagnostics
  disabled;
- complex indexes, RCU, epoch reclamation, and per-CPU caches only after the
  measurements identify a bottleneck.

## 0.1 extraction gate

The 0.1 package boundary is the bounded policy core described by the
implementation checkpoint above. It may be tagged or published only after its
public-contract, package/extract, and TheKernel consumer gates pass, including
RISC-V and LoongArch consumption of the packaged artifact.

That package checkpoint does not close the broader RFC. Fault-in and long-term
pinning, revocation, fault execution, broker queues, and userfaultfd remain
subject to the full semantic, teardown, pressure, and dual-architecture gates
above. A fault-policy seam must never be presented as userfaultfd support.
