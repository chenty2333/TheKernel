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
- owner and global accounting tokens;
- dirty-on-release state for writable pins;
- invalidation/cancellation linkage when the use permits revocation.

Drop releases in the reverse order outside spin/page-table locks. Partial
construction rolls back every frame, page-cache, range, and accounting token.

### 3. Pin admission and COW

Pinning follows a check-fault-revalidate sequence:

1. validate arithmetic, userspace bounds, alignment, and requested access;
2. reserve per-owner/global pages and bytes;
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
    pub generation: MappingGeneration,
    pub page: PageOffset,
    pub access: FaultAccess,
}
```

One request records type, range, credential/security decision, cancellation
state, waiter ownership, and a completion token. Overlapping requests may
coalesce only when mapping identity, generation, access, and handler match.
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

`thekernel-linux-mm` 0.1.0 may be published after mmap/COW/pin ownership,
generation/invalidation, fault execution, and teardown pass the gates above
without implicit current-task/address-space access. The fault broker seam may
be present without claiming userfaultfd support. TheKernel must consume the
packaged usercopy/MM artifacts on both architectures before tags are cut.
