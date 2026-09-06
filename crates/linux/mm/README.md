# thekernel-linux-mm

`thekernel-linux-mm` is a `no_std`, `forbid(unsafe_code)` Linux-visible memory
management policy core. It provides checked values and bounded lifecycle
sidecars that a kernel adapter can consume without importing a page table,
frame allocator, VFS object, task, implicit current address space, usercopy
implementation, raw pointer, HAL address type, or lock.

Version 0.1.0 provides:

- checked nonzero `UserRange` and page-aligned `PageRange` values with explicit
  caller-selected page size, overflow rejection, and page-covering plans;
- caller-owned `AddressSpaceId`, `MappingId`, and non-wrapping
  `MappingGeneration` values;
- immutable mapping snapshots, exact expected-generation revalidation, and
  typed invalidations for protect, unmap, remap, COW, fork, truncate, discard,
  filesystem invalidation, exec, and teardown;
- field-private `PinRequest` values carrying access, duration, use, range, and
  accounting owner;
- a fixed-capacity, allocation-free pin policy registry with per-owner and
  per-registry pages, bytes, and token quotas;
- a separate fixed-capacity `PinBudget` whose opaque charges enforce one real
  system-wide page, byte, and token bound across independent address spaces;
- two-phase reservation/publication, rollback on failed generation or access
  revalidation, read/read overlap, write-exclusive overlap, mutation admission,
  non-wrapping tokens, and explicit close/teardown states;
- cross-VMA pin publication through ordered `revalidate_next` calls, so an SG
  I/O range is not incorrectly forced into one backend or mapping snapshot;
- generation-safe `FaultKey` identity over address space, logical mapping,
  consumer fault epoch, absolute page address, and access; typed fault
  requests/dispositions; distinct new-request versus exact-coalesced-waiter
  admission; finite capacity values; stale-reply validation; and a `FaultPort`
  seam for a lower generic broker;
- a Linux v6.12 userfaultfd policy core with transactional API negotiation,
  anonymous-private MISSING registrations, O(1)-stack multi-VMA
  preflight/commit and mapping-refresh transactions, and checked
  COPY/ZEROPAGE mode and signed-prefix progress; and
- arithmetic-only page-covering, memlock, affine-origin, and remap-fragment
  planners, including canonical low-address rebasing and one whole-remap
  backend anchor pair for every fragment.

## Layer boundary

The generic VM/ax layer owns mapping identity storage, frame/page-cache pin
counters, range leases, page-table cursors, TLB invalidation, physical scatter
segments, observer registration, and the concrete bounded fault broker. This
crate consumes immutable identities and mechanism facts; it does not allocate
those identities or implement those mechanisms.

The kernel adapter owns VMA indexing and locks, COW/file fault execution, frame
and page-cache pins, dirty-on-unpin work, filesystem invalidation, security
hooks, signals, errno mapping, and publication into page tables. Syscall code
owns raw ABI decoding and all userspace copyin/copyout.

`FaultPort` is only a dependency-inversion seam. The crate does not contain a
queue, waiter table, observer list, wakeup implementation, readiness source, or
userfaultfd file. Its userfaultfd types are reusable Linux policy for a future
adapter. TheKernel currently composes them with `thekernel-axfault` and its own
MM/file/syscall adapters to expose a bounded anonymous-private 4-KiB MISSING
profile. That consumer integration is not part of this crate, does not make it
available to every embedding kernel, and is not a claim of complete
`userfaultfd(2)` coverage.

## Initial userfaultfd profile

The policy is pinned to Linux v6.12 API `0xaa`. Creation accepts only
`O_CLOEXEC`, `O_NONBLOCK`, and `UFFD_USER_MODE_ONLY`; the bounded first
profile requires `UFFD_USER_MODE_ONLY`. This keeps unknown flags
distinguishable from the permission gate so an adapter can return `EINVAL`
versus `EPERM`.

`UFFDIO_API` is a copyout-aware transaction. The adapter prepares negotiation,
copies the response to userspace, and commits initialization only after that
copy succeeds. Every negotiation error clears the complete userspace response.
The initial profile advertises no optional features: WP, MINOR, lifecycle
events, thread IDs, exact byte addresses, shmem/hugetlb, SIGBUS, poison, move,
and async WP remain unsupported.

Registration accepts anonymous-private MISSING fragments only. A Linux range
may cross multiple VMAs and gaps. The adapter supplies one raw registration
intent plus the ordered compatible VMA snapshots; the crate's canonical delta
planner implements Linux's same-handler partial registration law. It emits no
delta for a covered subset, unions prefix/suffix extensions, transitively
bridges same-handler fragments, preserves unmapped gaps as distinct outputs,
and reports a foreign handler as `Busy`/`EBUSY`. Every fragment considered for
folding must still match the supplied mapping identity and generation; stale
sidecar lineage fails closed and must be repaired through the explicit mapping
replace transaction rather than being silently refreshed by REGISTER.

The constant-size delta proof reports exact removal/replacement counts before
replaying into caller-owned bounded storage. Capacity, non-wrapping ID, stale
revision, and sealed-revision failure leave the table unchanged. The emitted
slices feed the table's existing all-or-none register/replace transaction;
syscall code performs no interval union. Direct table batches still reject
non-canonical same-handler overlap. Mapping split, trim, grow, and generation
changes use the matching replace transaction so old fragments remain visible
until every replacement has passed preflight. Mapping-level replacement pairs
each new fragment with its source token, allowing one all-or-none mutation to
refresh several non-overlapping handler owners in the same address space.
`canonical_union` lets that adapter fold strictly ordered adjacent replacement
fragments only when one post-state anonymous-private VMA covers them and their
handler, mapping, fault epoch, mode, and page geometry agree. It preserves one
representative source but requires the eventual mixed-owner transaction to
remove every consumed source token; overlap and reversed candidates fail
closed instead of being normalized.
Allocation-free table/intersection iterators let the adapter collect affected
IDs into its own bounded storage first. A saturated table revision is sealed
against publication but still permits revalidated pure removal, unregister,
detach, and final teardown without wrapping.

For userfaultfd, the generation supplied to `FaultKey` is a
registration/fault epoch, not the current VMA start. An `mprotect` or
topology-only split refreshes the registration fragments while preserving that
epoch, so a pending key continues to identify the same absolute page in a
surviving fragment. A replacement mapping receives new mapping/epoch authority;
handler detach or final close revokes the independent handler authority in the
lower broker. `refreshed_fragment` constructs split/trim/grow requests without
adopting a topology snapshot's generation or access, and `epoch_for_mapping`
fails closed if live fragments disagree on their fault epoch. Revalidation
still requires exact address-space, mapping, epoch, page coverage, and
alignment. Admission additionally checks the fault access. Resolver/completion
validation deliberately does not: a MISSING fault blocked before
`mprotect(PROT_NONE)` may still be populated, then its retry observes the new
protection.

An adapter planning an in-place mapping grow may not yet have a post-grow VMA
snapshot. `tail_extension_replacement` and `head_extension_replacement` build
source-bound replacements from the frozen address-space/mapping identity and a
same-start or same-end strictly larger range while preserving the
registration/fault epoch. They are safe to feed into the table's
mapping-replacement preflight before the MM transaction. The adapter remains
responsible for proving that the source registration reaches the corresponding
old mapping boundary and for publishing the replacement only after the mapping
grow succeeds.

REGISTER and UNREGISTER consume the same VMA-profile validator: API
initialization, one address space, anonymous-private kind, page geometry,
strict ordering, non-overlap, and actual intersection remain Layer 2 policy.
Raw ioctl ranges may contain unmapped gaps; the adapter only supplies the
ordered fragments that exist.

COPY policy recognizes every Linux v6.12 mode bit, including `DONTWAKE` and
`WP`; this distinguishes a known request from malformed raw bits. A consumer
without write-protected publication, including TheKernel's current
MISSING-only adapter, must first resolve target-mm/range error precedence and
then report its profile rejection through the signed `uffdio_copy.copy` field.
ZEROPAGE accepts only zero or `DONTWAKE`. Positive lower completion is a
page-aligned prefix: a full prefix returns success, a short positive prefix is
reported and returns `EAGAIN`, and a zero-page failure reports the
adapter-mapped negative errno. Installed pages survive result-copyout failure;
wake happens only after successful result copyout. Resolver lookup is an
address-space capability and intentionally does not require the invoking
handler to own the destination registration.

The lower broker remains the sole source of truth for fault queues and credits.
Its coalescing lookup supplies the complete exact, still-coalescible
`FaultRequest`, not only a page address. Layer 2 rechecks registration, current
mapping, access, and lifecycle for every admission. A genuinely new request
also checks per-address-space, per-handler, and system-wide request quotas; an
exact non-visible request is already charged, so adding a waiter skips only
those request quotas. A visible terminal is not coalescible and must be
classified as a new bounded request even while its older waiter retains it.
The resulting permit reports which request-credit class was admitted, but does
not prove that lower admission succeeded. Lookup, policy, and lower admission
must share one externally serialized broker critical section, and the broker
still atomically rechecks exact identity, coalescibility, and finite waiter
capacity.

The broker must atomically claim FIFO `Pending -> Delivered` before copying one
`uffd_msg`; a failed message copyout leaves the request Delivered, not
requeueable. Before API initialization, read returns `EINVAL` and poll reports
`ERR`. Linux poll also reports `ERR` for a blocking FD; only an initialized
`O_NONBLOCK` FD may report `IN` when a pending event exists. A read buffer must
hold at least one 32-byte message, the first message may block, and subsequent
messages in the same read are claimed without waiting.

## Pin transaction

The consumer follows this sequence:

1. freeze the current address-space identity and build a `PinRequest`;
2. reserve a `PinBudgetCharge` from the consumer's system-shared budget before
   acquiring any lower pin ownership;
3. call `PinRegistry::reserve` to charge owner/per-registry quotas and reserve
   a unique address-space token before blocking work;
4. snapshot and fault each covered mapping without holding the VMA/page-table
   lock across blocking work, breaking COW before writable exposure;
5. acquire the exact lower frame/page-cache owner for a bounded window, then
   call `revalidate_next` for every contiguous covered segment in that window;
   either retain one topology-publication critical section for the full range,
   or keep the reservation live as a mutation fence and release the lock
   between windows while routing every overlapping mapping mutation through
   `admit_mutation`;
6. call the constant-time `commit` only after the whole page range has
   revalidated; and
7. after synchronous completion, verified async completion, or cancellation,
   release lower frame/page-cache ownership, the registry token, and finally
   the system charge.

Any `revalidate_next` generation, range, or access failure removes the pending
record and refunds both quotas; the consumer still drops any lower partial
frame/page-cache pins it prepared. An abandoned reservation must be explicitly
cancelled; forced teardown cancels all unpublished reservations and waits for
active mechanism pins to release. Both reservations and active pins block an
overlapping mapping mutation, so every such mutation must ask `admit_mutation`
before publication. This permits bounded revalidation windows without letting
the validated prefix become stale. System charges are intentionally separate
from registry teardown so the embedding kernel can keep them alive until its
last mechanism owner has actually released.

## Error and stability contract

`MmError` is a stable typed policy error. The embedding adapter maps it to the
correct syscall-specific Linux errno or signal; the crate does not freeze one
errno for operations whose Linux call sites intentionally differ.

The 0.1 contract freezes checked arithmetic, identity/generation matching,
bounded accounting, rollback, overlap, mutation, close/teardown, stale fault
completion, and remap geometry. It does not freeze a VMA tree, lock, RCU/epoch
scheme, page-table implementation, frame type, physical-address format,
filesystem interface, or fault-broker storage layout.
