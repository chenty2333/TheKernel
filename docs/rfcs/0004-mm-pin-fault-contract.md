# RFC 0004: Accounted VM Pins and Generation-Safe Fault Delegation

- Status: draft
- Date: 2026-07-12
- Last implementation audit: 2026-07-19
- Owners: TheKernel maintainers
- Target layers: architecture/HAL, generic VM/fault/readiness mechanisms,
  `thekernel-linux-mm`, TheKernel address-space integration, and syscall/FD
  composition

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

At the time this RFC was drafted, TheKernel had RAII frame/page-cache pins and
range-pin tokens but no connected userfaultfd-style broker. The current source
checkpoint now connects a deliberately bounded anonymous-private MISSING
profile. That source checkpoint is not a complete userfaultfd implementation or
a release claim: the public pin contract is still draft, and there is still no
MMU-notifier equivalent, revocable/long-term pin model, or DMA pin contract.

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

The stable boundary is an ownership chain, not one large MM or HAL trait:

| Owner | Contract |
| --- | --- |
| Architecture/HAL and page-table crates | PTE construction/publication, local and remote TLB/I-cache primitives, and deferred page-table ownership. They do not own Linux VM or userfaultfd policy. |
| Generic ax/VM mechanisms | RAII frame/page-cache pins, physical/page-cache segment descriptions, and generic mapping/invalidation primitives. |
| `thekernel-axfault` plus generic readiness/task mechanisms | Fixed-capacity request/waiter ownership, exact-key coalescing, Pending/Delivered/terminal transitions, cancellation, handler detach, credit reclaim, PollSet registration, task blocking, and wake mechanics. They contain no Linux flags, VMAs, errno, page tables, or FD rules. |
| `thekernel-linux-mm` | `no_std`, unsafe-free Linux policy: checked ranges and identities, registration and mutation plans, MISSING admission/completion rules, pin admission/accounting policy, remap/COW-visible contracts, partial resolver results, and errno-class decisions. It does not own concrete tasks, VFS objects, file descriptions, or page tables. |
| TheKernel `AddrSpace` adapter | The product linearization boundary across concrete VMA/PTE state, registration policy, broker admission/completion, mapping sidecars, and address-space lifecycle. |
| TheKernel file/syscall adapters | Linux UAPI layout, argument usercopy, API/ioctl ordering, OFD status/readiness, task/signal composition, fd reservation, and final fd publication. |

Credentials, VFS objects, signals, process teardown, and FD readiness are
consumers coordinated by the product adapter; they are not dependencies that
may be pulled into the pure `thekernel-linux-mm` policy crate.

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

The full RFC target maps Linux registration modes and commands onto the broker.
It preserves:

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

The current product consumer activates only the bounded profile documented in
the implementation checkpoint below. It requires `UFFD_USER_MODE_ONLY`,
advertises no optional feature bits, and publishes only the ioctls that this
profile actually implements. A connected bounded profile must not be described
as complete Linux userfaultfd support.

### 9. Fault execution and unsafe boundary

Architecture trap code produces a typed fault and calls the address-space
handler. The handler snapshots the VMA, drops locks before filesystem/pager
I/O or sleeping, resolves COW/file/delegated backing, then revalidates before
page-table publication.

Unsafe code is confined to page-table/HAL operations and raw user-memory
adapters. Public MM policy uses owned ranges, frames, and initialized buffers;
user-triggerable alignment/range errors return errors rather than panicking.

## 0.1 implementation checkpoint

As of 2026-07-19, the extracted 0.1 boundary is deliberately narrower than
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

TheKernel consumes the packaged policy crate without moving generic frame,
page-table, task, readiness, or page-cache mechanisms into syscall code.

### Bounded userfaultfd profile

The current source checkpoint exposes this exact profile:

- Linux v6.12 `UFFD_API` (`0xaa`) with no optional feature bits. Creation
  accepts only `O_CLOEXEC`, `O_NONBLOCK`, and `UFFD_USER_MODE_ONLY`, and this
  unprivileged profile requires `UFFD_USER_MODE_ONLY`. Matching Linux's check
  order, an absent USER_MODE_ONLY bit is `EPERM` even when an unknown bit is
  also present; an unknown bit combined with USER_MODE_ONLY is `EINVAL`.
- The syscall reserves an fd before attaching a handler, so `EMFILE` has no MM
  side effect. Later fallible owners remain unpublished and roll back through
  Drop; final fd-table publication is infallible. `UFFDIO_API` copies its
  response before committing the one-way API transition and clears its output
  on validation or commit failure.
- Context ioctls are `UFFDIO_API`, `UFFDIO_REGISTER`, and
  `UFFDIO_UNREGISTER`. Range ioctls are `UFFDIO_WAKE`, `UFFDIO_COPY`, and
  `UFFDIO_ZEROPAGE`. Registration mode is exactly MISSING.
- Registration and resolution accept only an actual 4 KiB anonymous-private
  COW backend. File, shared, device, shmem, hugetlb, and larger backend
  granules are outside this profile. A raw REGISTER/UNREGISTER range may cross
  holes and multiple VMAs; only compatible mapped fragments enter the bounded
  table. Same-handler partial registration is canonicalized, while a foreign
  handler overlap fails with `EBUSY` without partial mutation.
- Only user-originated missing faults delegate. Under one address-space
  critical section, admission verifies an absent PTE, freezes mapping and
  registration identity, charges policy and broker capacity, and publishes an
  independently cancellable waiter. New-request readiness is emitted only
  after unlock; the task then waits through an interruptible PollSet
  check-arm-check loop. Kernel-originated usercopy faults never delegate or
  sleep and return to exception-fixup policy.
- Fault-around stops before a future registered missing page. A present
  registered COW or permission fault retains one ordinary-page forward-progress
  path instead of being misclassified as MISSING.
- Events are fixed 32-byte page-fault messages. Read claims Pending as
  Delivered before copyout; a failed event copyout is not replayed and is no
  longer poll-readable. Before API initialization, or for a blocking
  userfaultfd OFD, poll reports error. An initialized nonblocking OFD is
  readable only while the broker has a Pending event.
- COPY supports mode zero and `DONTWAKE`; ZEROPAGE supports mode zero and
  `DONTWAKE`; WAKE is handler-scoped. Resolver installation itself is an
  address-space capability, so the invoking OFD need not own the destination
  registration. A proactive fill with no pending request is valid.
- `UFFDIO_COPY_MODE_WP` is a known Linux bit rather than malformed input, but
  this MISSING-only profile cannot publish a UFFD-WP PTE. The implementation
  first preflights the target and then reports the target error or `-EINVAL`
  through the signed `uffdio_copy.copy` field; WP remains unadvertised.
- A full resolver prefix writes a positive byte count and succeeds. A short
  positive prefix remains installed, writes that count, and returns `EAGAIN`.
  Zero progress writes the negative lower errno. The signed result is copied
  out before any implicit wake. If result copyout returns `EFAULT`, installed
  pages and immutable deferred completions remain intact and no waiter is
  auto-woken; a later explicit WAKE can recover them.

### Page publication and architecture coherence

Each resolver page is prepared with at most three reusable page-table frames
and one completely initialized data frame before taking `Mutex<AddrSpace>`.
Under that mutex, the adapter revalidates the complete VMA/registration lease,
the page's PTE state, and every open same-page broker transition before
publishing one leaf. Read, write, and execute faults are distinct generic
broker keys, so the fixed completion batch validates every matching access
variant that actually exists in `Pending` or `Delivered` state before
publication. After the leaf is visible, their immutable terminal results are
recorded without waking; result-copyout/WAKE later owns the wake. A previous
terminal is never overwritten by a refill.

Executable publication adds an affine proof boundary. Once the frame is fully
initialized, the first locked attempt returns
`NeedsIcacheSynchronization` without publishing it. The caller drops the
address-space mutex, performs global instruction-cache synchronization, obtains
a move-only `UffdIcacheSynchronization` witness, and consumes that value on a
second attempt that revalidates every authority before making the executable
PTE reachable. The witness proves that synchronization occurred; its ordering
after initialization of the current frame is maintained by the single resolver
control flow rather than by embedding frame identity in the value.

On RISC-V the writer executes `fence rw, rw` before its local `fence.i` and
before the maintenance broker requests remote `fence.i`. On LoongArch one TLB
entry covers the adjacent even/odd 4 KiB pair, so targeted local invalidation
aligns its operand to the containing 8 KiB pair. A faulting CPU repairs its own
cached-invalid translation before retrying; every coalesced waiter performs
that local repair, while a fresh-map publisher does not issue a global TLB
shootdown.

This executable resolver contract does not close generic `mprotect`-adds-X.
The generic protect transaction still performs global TLB/I-cache maintenance
inside its serialized `AddrSpace` commit path and does not yet carry the same
lock-external typed proof. Moving that maintenance out of the mutex without
exposing executable bytes prematurely remains an open MM task.

### Lock boundaries

The implementation is intentionally not described as lockless:

| Boundary | Work serialized inside | Work that must stay outside | Remaining limit |
| --- | --- | --- | --- |
| `Mutex<AddrSpace>` | VMA/PTE/mapping-identity state; REGISTER/UNREGISTER scans and table mutation; fault admission and broker charge; mapping sidecar commit; each resolver page's lease/PTE/completion revalidation and leaf publication | Resolver data/prepared-page-table-frame allocation, COPY source usercopy, task blocking, PollSet wake, resolver global I-cache work, signed result copyout, and large owner destruction | Ordinary faults, fork, fixed `mremap`, registration scans, and per-page resolver publication remain serialized. A multi-page resolver is a sequence of critical sections, not one range-atomic transaction. |
| userfaultfd OFD API mutex | One-way API negotiation state only | It is released before taking `Mutex<AddrSpace>`; event waiting and wake do not retain it | This does not make the MM path concurrent. |
| System pin-budget `SpinNoIrq` | O(1) reserve/release accounting | VMA/PTE scans and lower pin ownership | One shared accounting point remains. |
| 64 physical-pin `SpinNoIrq` shards | Allocation-free frame lookup/refcount update in transactions of at most 64 pages | Table allocation, unused-owner destruction, and public range transaction planning | A skewed physical-address distribution can exhaust one shard before aggregate quota and still incurs per-frame lookup/refcount cost. |

Fault readiness and mutation receipts are explicit lock-external ownership:
their `finish`/wake operation is valid only after releasing `Mutex<AddrSpace>`.
No temporary RwLock or split-lock claim weakens the atomicity required by VMA,
COW, fork, and fixed-`mremap` transactions.

### Resource ceilings

| Resource | Fixed ceiling |
| --- | ---: |
| userfaultfd handlers per address space | 16 |
| registration fragments per address space | 64 |
| live broker requests per address space | 64 |
| live broker requests per handler | 64 |
| live broker waiters per address space | 128 |
| live broker request ownership system-wide | 4,096 |
| PollSet registration slots per PollSet | 256 |
| registration/mapping transaction fragments | 64 |
| projected `mprotect` candidates | 192 |
| simultaneously prepared mapping-sidecar plans | 2 |
| same-page read/write/execute resolver completions | 3 |
| reusable prepared page-table frames per resolver | 3 |
| user-I/O pin tokens per address space and system-wide | 64 each |
| user-I/O pinned pages per address space and system-wide | 16,384 each |
| user-I/O pinned bytes per address space and system-wide | 64 MiB each |
| physical segments in one user-I/O pin result | 32 |
| pages in one user-I/O pin scan/revalidation window or IRQ-off shard transaction | 64 |
| physical-pin metadata slots system-wide | 32,768 across 64 shards |
| logical mapping lineages per address space | 65,536 |
| VMA fragments per address space | 65,536 |

An exact coalesced fault consumes a waiter slot but no new request slot or
global request credit. The 4,096 global pool bounds only live request
ownership; it does not bound system-wide handlers, registrations, waiters, or
the bytes of lazily allocated per-address-space UFFD state. A first handler
preallocates storage for that address space's 64 requests, 128 waiters, scratch
vectors, and two plan slots. That state is retained after final handler detach
until address-space teardown so a later handler can reuse it. A system-wide
UFFD state-byte budget remains unimplemented. PollSet's 256 is registration
capacity, not event-queue capacity.

A protection split consumes a VMA fragment but not another logical lineage.
Admission fails with `ENOMEM` before publication when either mapping ceiling
would be exceeded. These fixed ceilings are not Linux's runtime-tunable
`vm.max_map_count`.

### Mapping lifecycle and explicit non-claims

REGISTER/UNREGISTER are all-or-none registration-table transactions, not a
whole main-MM transaction. `munmap`, `mprotect`, `mremap`, `brk`, and growdown
preflight bounded sidecars and commit them only with the concrete VMA outcome.
`mprotect` preserves fault epochs and canonicalizes only compatible fragments.
Moved or duplicated destinations remain unregistered because REMAP events are
not advertised. Boundary growth may extend the same authority; a `brk` tail is
deliberately unregistered. Close/detach releases or completes every owned
request and wakes outside the lock. After exec, a non-CLOEXEC OFD remains
initialized but its weak old-mm binding is inert; WAKE against a retired mm is
a successful no-op. Fork creates no child UFFD authority and emits no event.

Linux v6.12 fixed `mremap` may retire a replacement destination and an old
source tail before a later move failure. TheKernel stages the destination
before retiring any source byte, so its failure can preserve the complete
source while still retiring the old destination. UFFD sidecars follow this
concrete MM outcome. This is a stronger rollback guarantee, not exact Linux
failure-state parity.

This profile does not implement WP, MINOR, CONTINUE, WRITEPROTECT, MOVE,
POISON, shmem/hugetlb registration, SIGBUS mode, thread-id reporting,
fork/remap/remove/unmap events, exact byte fault addresses, the privileged
non-USER_MODE_ONLY sysctl/CAP_SYS_PTRACE route, or a userfaultfd-specific
anon-inode security hook. It also does not implement fault-in pinning,
long-term/DMA pins, revocation, an MMU-notifier/interval-notifier equivalent,
IOMMU/SVA integration, lockless/RCU VMA lookup, or a system-wide UFFD state-byte
budget. The RFC therefore remains `draft`.

### Direct-I/O and pin reality

Eligible regular-file operations automatically attempt the synchronous pinned
direct path; this is not a dormant profile knob. The path accepts only
already-resident 4 KiB pages with the requested PTE access. Missing pages,
unresolved writable COW, unsupported mappings, excessive scatter/gather, and
resource pressure fall back to the semantics-preserving copied path.

The complete logical range is reserved before lower pinning, but every
64-page window still takes `Mutex<AddrSpace>` to scan VMAs and PTEs, touch
page-cache/physical-pin state, and revalidate expectations. Exact frame or
page-cache pins and the aggregate system charge remain owned until completion;
writable page-cache pins are dirtied on release, and overlapping destructive
mapping/file mutation is rejected. Logical identity now uses a lineage-keyed
ordered index and affected-lineage sweeps rather than invalidating all mappings
through one address-space topology generation, but this is not lockless pinning.
Shared-mapping growth and file-registration preparation remain serialized.
Transactional COW clone and moving-`mremap` retain rollback ownership for PTEs,
frame references, source flags, locks, and topology publication. Page-cache
invalidation remains owner-acknowledged: pages are staged before lower
mutation, every address-space listener must acknowledge detachment, foreign-
owner contention restores the staged state, and only the populating owner may
defer its own detach until population completes.

The current syscall consumer passes `try_async = false` to the lower pinned
read/write interface, and the former `user_direct_async` control is not a
supported runtime profile. There is no accepted asynchronous pinned-direct
consumer path, no long-term device ownership, and no pin-throughput or
contention result that justifies a scalability claim.

### Historical performance evidence scope

The bounded historical archive named
`2026-07-19-mm-reliability-2a71804` records these exact source identities:

- TheKernel `2a71804d97a947a9a207e4bc2a01f55077ef8b35`;
- `thekernel-ax` `769b70b89b943ce32081f0a50385ad26fe9addf7`;
- `thekernel-linux-abi` `83e31bd2cf6ce301d4fb99a2c1a749a06b8add08`.

That archive records a 90/90 per-commit gate, reproducible RISC-V and
LoongArch rootfs construction, and SMP TLB runs at RV4, RV8, LA4, and LA8. Its
MM performance smoke ran only at RV4 and LA4, with ten user-visible metrics,
4,096 sparse VMAs, and four pinned workers. It did not measure UFFD.

The counterbalanced null series was rejected because the pin-throughput P99
ratio spread exceeded 20%. Consequently there is no accepted paired regression
baseline, demonstrated speedup, production P99/P999 or SLO, isolated
`Mutex<AddrSpace>` hold-time result, hardware TLB event count, lock-free claim,
or production hardware scalability result. Historical evidence for that exact
commit cannot substitute for validation of the current UFFD source revision.

### Current validation checkpoint

The currently recorded checks are intentionally narrower than release
acceptance:

| Check | Current result |
| --- | --- |
| TheKernel targeted UFFD host tests | 72 passed |
| `thekernel-linux-mm` tests | 47 passed |
| `thekernel-axfault` tests | 39 unit tests and 1 doc test passed |
| Native Linux helper | Passed through `THEKERNEL_USERFAULTFD_EXEC_COPY_OK` and the final marker, including the executable COPY fixture |
| Exact current RISC-V guest | Pending |
| Exact current LoongArch guest | Pending |
| Current RV/LA 4- and 8-CPU UFFD/TLB execution matrix | Pending |
| UFFD pressure, race, differential, and performance gates | Pending |
| Clean exact-HEAD CI, package/release gate, and publication | Pending |

The host counts exercise state machines and ABI composition; the native Linux
run validates the helper/reference fixture. Neither proves TheKernel guest or
architecture behavior. Older guest logs from a smaller helper, or a failed run
whose executable/copyout fixture was later changed, are diagnostic artifacts
and are not current acceptance receipts.

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

## Full RFC validation gates

These are closure gates for the complete pin/fault-delegation design, not a
claim that the bounded MISSING profile already implements every listed mode or
event. The current checkpoint receipts are recorded above.

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

That package checkpoint does not close the broader RFC. The bounded generic
broker and anonymous-private MISSING consumer are connected, but fault-in and
long-term pinning, revocation, full userfaultfd modes/events, generic
`mprotect`-adds-X publication, pressure/race coverage, and exact dual-
architecture gates remain open. A working narrow profile must not be presented
as complete userfaultfd, release acceptance, or a lockless MM.
