# RFC 0005: Bounded io_uring Core and Kernel Adapter Contract

- Status: draft
- Date: 2026-07-15
- Owners: TheKernel maintainers
- Target layers: `thekernel-linux-io-uring` and the TheKernel FD, MM, VFS,
  readiness, signal, and syscall adapters

## Problem

`io_uring` is not just three syscall entries and a shared queue layout. The
kernel consumes concurrently mutable userspace indexes and SQEs, retains file
and memory resources beyond syscall return, races completion against cancel
and close, and must still produce at most one terminal CQE for every accepted
one-shot request.

A minimal implementation can therefore corrupt queue accounting or retain a
stale object even when its individual read and write operations look correct.
In particular:

- rereading an SQE after admission permits userspace to change request
  identity or arguments while the kernel is executing it;
- accepting more terminal work than the CQ can represent either drops
  completions or requires an unbounded overflow list;
- resolving a numeric descriptor at execution time can target a replacement
  descriptor after close and reuse;
- treating fixed files as raw indexes can release an open file description
  while an accepted request still uses it;
- a fire-and-forget poll callback can publish after cancellation, source
  teardown, or ring close;
- freeing ring pages on descriptor close can invalidate a still-live mapping;
- calling a copied synchronous path "async" does not provide io-wq, safe
  long-term pins, or asynchronous block execution.

The first TheKernel slice must establish a bounded, generation-safe contract
without claiming the full Linux surface or depending on evaluator/profile
features.

## Evidence reviewed

The contract was checked on 2026-07-15 against these exact snapshots:

- Linux stable `v6.12.35`, commit
  `783cd2c3dca8b6c434e955b84c20c8940588dc68`:
  - `include/uapi/linux/io_uring.h` for setup, mmap, SQE/CQE, enter,
    registration, probe, and feature-bit ABI;
  - `io_uring/io_uring.c` for queue validation, submission, CQ publication,
    overflow behavior, wait, and final context teardown;
  - `io_uring/register.c` and `io_uring/rsrc.c` for registration validation,
    fixed-resource ownership, resource-node lifetime, and deferred release;
  - `io_uring/cancel.c`, `io_uring/poll.c`, and `io_uring/rw.c` for request
    identity, one terminal owner, poll arming/removal, cancellation, retained
    files, and positioned I/O behavior.
- liburing `2.8`, commit
  `80272cbeb42bcd0b39a75685a50b0009b77cd380`:
  - `src/setup.c`, `src/queue.c`, `src/register.c`, and the public headers for
    the userspace layout, acquire/release queue protocol, mmap aliases,
    registration calls, and probe expectations used by a real consumer.
- Asterinas commit `435916bf0714a61e0fd1ebab5f6486532dedd8e4`,
  reviewed as a Rust Linux-ABI-kernel comparison. That snapshot has no
  io_uring implementation, so it is evidence that Rust ownership alone does
  not supply this ABI or its lifecycle contract, not an implementation source.

Linux kernel source is GPL-2.0-only, with Linux UAPI material carrying its own
syscall-note/MIT expression. The reviewed liburing library sources are
MIT-licensed, and its repository also contains LGPL-2.1 material. Asterinas is
MPL-2.0. This RFC adopts observable behavior and general architecture; no
source is copied from these snapshots.

## Decision

### 1. Split policy from the concrete kernel adapter

The `no_std`, unsafe-free `thekernel-linux-io-uring` crate owns:

- setup geometry and the supported setup/feature profile;
- decoding of one already copied 64-byte SQE;
- bounded, generation-scoped request identity and lifecycle;
- terminal CQ-credit admission and completion-publication plans;
- fixed-file table publication, generation, leases, and retirement;
- cancellation selection and single-terminal-owner policy;
- registration-header classification and close/drain state transitions;
- typed distinctions between malformed, unsupported, exhausted, stale, busy,
  closing, and allocation failures.

The crate must not dereference userspace, perform shared-page atomics, read an
implicit current task/FD table/address space, execute VFS I/O, install a
readiness callback, allocate or map pages, or choose kernel locks.

TheKernel owns:

- exact UAPI structs, syscall argument copyin/copyout, and errno mapping;
- acquire/release atomic access to SQ/CQ fields in shared pages;
- ring and SQE page allocation, `mmap`, VMA ownership, and unmap teardown;
- numeric FD lookup, FD reservation/publication, and exact
  `Arc<FileDescription>` retention;
- VFS read/write execution and userspace-buffer fault/partial-I/O behavior;
- concrete `PollSet` registration, waiting, signals, callbacks, and wakeups;
- adapter synchronization, request execution, cancellation handshakes, and
  final destruction.

Generic readiness and shared-page mechanisms remain in their lower layers.
Linux-visible io_uring policy must not move into an architecture HAL, block
driver, filesystem, or benchmark profile.

### 2. Freeze a deliberately small initial ABI profile

The initial setup flags are exactly:

- no flags;
- `IORING_SETUP_CQSIZE`;
- `IORING_SETUP_CLAMP`;
- `IORING_SETUP_NO_SQARRAY`.

All other setup flags are rejected before allocation. Worker CPU/idle fields,
attached-workqueue fields, and reserved words must be zero. SQ and CQ sizes
are checked, bounded by the pinned Linux limits, and rounded or clamped only
where the selected Linux flag permits it.

The initial `io_uring_enter` flags are exactly no flags and
`IORING_ENTER_GETEVENTS`. The six-argument syscall ABI is used. A legacy
signal-mask pointer is accepted only when its supplied size is exactly the
native TheKernel `SignalSet` size. The adapter installs the copied mask only
for the scoped wait and restores the old mask on success, signal interruption,
copy fault, timeout, and every other exit. `IORING_ENTER_EXT_ARG` and its
extended argument structure are unsupported.

The initial submission operations are exactly:

- `IORING_OP_NOP`;
- positioned `IORING_OP_READ`;
- positioned `IORING_OP_WRITE`;
- one-shot `IORING_OP_POLL_ADD` using the full 32-bit poll-event field;
- the default user-data form of `IORING_OP_ASYNC_CANCEL`.

The initial registration operations are exactly:

- `IORING_REGISTER_FILES`;
- `IORING_UNREGISTER_FILES`;
- `IORING_REGISTER_PROBE`.

`REGISTER_PROBE` reports only operations implemented by this profile. An
opcode present in the pinned Linux UAPI but not implemented completes with
`-EOPNOTSUPP`; an opcode outside the pinned UAPI range completes with
`-EINVAL`. Operation-specific malformed fields and unsupported modifiers stay
typed until the adapter maps their CQE result.

### 3. Setup and FD publication are transactional

`io_uring_setup` first copies and validates the complete input. The adapter
then reserves every bounded resource needed for the core, shared mappings,
SQEs, readiness, and file publication before making a descriptor visible.

Ring creation uses the existing FD transaction:

1. reserve a numeric descriptor with the requested close-on-exec state;
2. allocate and fully initialize the ring and its fixed-capacity policy state;
3. construct the ring's `FileDescription` and prepare FD publication;
4. copy the resolved output parameters to userspace;
5. commit descriptor publication exactly once.

Every failure before commit releases the FD reservation, pages, request slots,
readiness state, and file-description ownership. No partial ring may be found
through the FD table. Destructors and page release run after the relevant FD,
MM, policy, and IRQ-safe locks have been dropped.

### 4. Shared mappings have explicit ordering and lifetime

The initial layout uses one shared SQ/CQ backing and a separate SQE backing.
`IORING_OFF_SQ_RING` and `IORING_OFF_CQ_RING` alias the shared backing when the
advertised `SINGLE_MMAP` feature is used; `IORING_OFF_SQES` names the SQE
backing. Only the published offsets, page-rounded lengths, and compatible
shared mappings are accepted.

The adapter prepares and allocates a mapping before mutating the address-space
topology. Every committed VMA retains an owner that keeps the exact ring and
its pages alive. Closing a numeric FD or the last descriptor must not free
pages still reachable through a VMA. Final draining and page destruction occur
only after request executors, readiness callbacks, fixed-file leases, and
mapping owners can no longer access the ring.

Shared fields are untrusted concurrently mutable input. The adapter:

- acquire-loads userspace-published SQ tail and CQ head values;
- validates wrapping distances against the configured power-of-two capacity;
- writes a complete CQE before release-storing the new CQ tail;
- release-stores an SQ head only after that entry has been admitted;
- never treats an invalid forward jump as permission to index outside a
  backing allocation.

The public policy crate expresses offsets and publication plans, not raw
atomic pointers. Alignment checks and the small architecture-specific unsafe
boundary stay in TheKernel's shared-memory adapter.

### 5. Every SQE is copied exactly once

After acquiring a valid SQ tail, the adapter resolves the next index through
the SQ array or direct `NO_SQARRAY` rule, bounds-checks it, and copies exactly
64 bytes into private kernel storage. Parsing, FD selection, cancellation
matching, diagnostics, and execution use only this stable copy. No path rereads
an SQE field after the copy.

The copied `user_data` and opcode remain available even when later parsing
produces an operation-level error. Once SQ consumption is committed, such an
error is represented by that request's terminal CQE, not by silently dropping
the entry or retroactively changing its identity.

The initial profile does not support `IORING_SETUP_SUBMIT_ALL`. A copied SQE
which fails opcode or operation-field preparation is counted as consumed and
receives its terminal error CQE, but stops the current submission batch. Later
SQEs remain unconsumed. An error produced by executing an otherwise valid
request remains an ordinary CQE result and does not retroactively become a
submission failure.

### 6. Terminal CQ credit precedes acceptance

Every one-shot SQE must reserve both a generation-scoped request slot and one
terminal CQ credit before the adapter advances SQ head or hands work to an
executor. The credit remains charged through prepared, issued, terminal,
pending, and published states. It is refunded only when a validated userspace
CQ head reaps the CQE, or during explicit final draining after userspace can no
longer consume the mapping.

This establishes `IORING_FEAT_NODROP` without an unbounded overflow list. If
the next SQE cannot reserve terminal credit, that SQ position remains
unconsumed. When no SQE was accepted by the call, `io_uring_enter` returns `0`;
it must not report submission, increment a dropped counter, busy-poll, or
overwrite an unread CQE.

Admission follows a reversible state sequence:

```text
free -> reserved -> prepared -> issued
                    |           |
                    +-> terminal-owned <-+
                              |
                              v
                     completion-pending -> published -> reaped
```

Reservation failure changes no shared head. A failure after reservation but
before SQ commit rolls back the slot and credit. After commit, execution,
cancellation, preparation failure, and close race for exactly one terminal
permit; every losing path observes a stale/already-terminal state and cannot
publish a second CQE.

### 7. Read and write retain exact open file descriptions

`READ` and `WRITE` are positioned operations in the first profile. An offset
of `u64::MAX`, which requests current-file-position semantics, completes with
`-EOPNOTSUPP`. The initial implementation does not silently serialize or
modify the shared open-file-description offset.

For an ordinary descriptor, admission resolves and retains the exact
`Arc<FileDescription>` before the descriptor can be reused. Execution uses the
same shared positioned-I/O helpers as `pread64` and `pwrite64`, preserving
access mode, append policy where applicable, short I/O, EOF, copy faults,
signals, and VFS errors. It never looks the numeric descriptor up again.

The first adapter may execute these operations inline. This is useful ABI
support, but it is not native asynchronous file execution. It neither enables
an experimental async block queue nor claims io-wq behavior.

### 8. Fixed files use generation-scoped OFD leases

`REGISTER_FILES` copies a bounded signed-FD array, treats `-1` as a sparse
slot, resolves every other entry to its exact `Arc<FileDescription>`, and
builds an unpublished fixed-capacity table. The operation is all-or-nothing:
copy, lookup, allocation, or quota failure releases every retained owner and
leaves the old registration state unchanged.

Publication assigns a table identity and a non-wrapping generation to each
installed slot. A fixed-file request acquires a lease for that exact
ring/table/slot/generation before it is accepted. The lease, not a later slot
lookup, retains the OFD through execution and terminal cleanup.

`UNREGISTER_FILES` first prevents new lookup and lease acquisition, then
retires owners whose lease counts are zero. An owner with an outstanding
request lease is released only when the last exact lease returns. Re-register
and slot reuse cannot make an old token refer to a new OFD. `Arc` destruction
and final file close occur outside the table lock.

### 9. Poll, cancel, and close share one lifecycle

`POLL_ADD` is one-shot. The adapter retains the exact OFD, checks current
readiness, arms a bounded retained `PollRegistration`, and rechecks readiness
to close the check/arm race. Its callback performs only bounded,
allocation-free notification; completion processing revalidates the
generation and wins the same terminal permit used by all other paths.

`ASYNC_CANCEL` initially matches the oldest still-cancellable request with the
requested `user_data`, excluding its own request identity. Successful cancel
detaches the retained readiness registration and gives the target exactly one
`-ECANCELED` terminal CQE. The cancel request has its own CQ credit and
completes with `0`; a missing or already-terminal target completes the cancel
request with `-ENOENT` without changing the target.

Final ring close proceeds explicitly:

1. change the request registry from open to closing so no admission succeeds;
2. detach or cancel retained poll registrations and ask every executor to
   quiesce, releasing the exact fixed-file leases retained by those polls;
3. hide fixed-file tables from new leases and retire their remaining owners;
4. let completion, cancel, and close contend for one terminal permit per
   accepted request;
5. after executors and callbacks are quiescent, enter draining if no userspace
   mapping can consume remaining CQEs;
6. discard only through the explicit drain API, release credits and retired
   OFDs outside locks, and finish close only when every registry is empty.

A late readiness callback, device completion, or cancellation token from an
old generation is ignored as stale; it cannot target a reused request slot,
fixed-file slot, or ring.

### 10. Registered buffers remain honestly unsupported

Well-formed `IORING_REGISTER_BUFFERS` and
`IORING_UNREGISTER_BUFFERS` requests return `EOPNOTSUPP`. Null-pointer, zero or
excessive-count, and reserved/header combinations return `EINVAL` before the
unsupported result is selected.

TheKernel does not promote its short synchronous pin or copied direct-I/O
path into a registered-buffer contract. Registered buffers require the
long-term pin, COW, invalidation, truncate/writeback, accounting, cancellation,
and late-completion rules described by RFC 0004. Until those rules and the MM
adapter gates pass, no buffer-registration probe or feature bit may imply
support.

### 11. Feature advertisement requires adapter proof

The initial core can describe only these feature bits:

- `IORING_FEAT_SINGLE_MMAP` after both offsets are proven to retain one shared
  backing;
- `IORING_FEAT_NODROP` after every accepted one-shot request reserves terminal
  credit through reap;
- `IORING_FEAT_SUBMIT_STABLE` after the adapter proves the one-copy SQE rule;
- `IORING_FEAT_POLL_32BITS` after the concrete poll adapter consumes and
  returns the complete 32-bit event contract.

TheKernel advertises a bit only after its concrete adapter and guest tests
prove the premise. Package types or an uncalled code path are not proof.
`FeatureFlags::SUPPORTED` is only the core vocabulary upper bound;
`SetupRequest::resolve` requires the adapter to pass the exact proved subset
and never inserts adapter-dependent feature bits implicitly.

### 12. Allocation and lock rules

Ring setup performs all capacity allocation fallibly. Steady-state request
admission, generation changes, CQ publication, cancellation selection,
fixed-file lease operations, and close progress do not grow an unbounded
container.

No user-triggerable path uses infallible allocation. No usercopy, page fault,
VFS operation, waiter sleep, waker clone/drop, `Arc` destruction, or backing
page release occurs while holding a spin, IRQ-disabled, page-table, shared-ring,
request-registry, or fixed-file-table lock. Readiness callbacks do not
allocate, block, perform usercopy, or destroy the final owner.

Lock implementations, internal indexes, and executor queues remain private.
RCU, epoch reclamation, per-CPU completion queues, and native workers require
measurement and a separate contract extension; they are not part of the 0.1
public API.

## Non-goals for the initial profile

The first slice does not implement or advertise:

- `SQPOLL`, `IOPOLL`, io-wq/native workers, attached workqueues, or registered
  ring descriptors;
- linked, hard-linked, multishot, drain, skip-success, timeout, personality,
  buffer-selection, or resource-tag semantics;
- current-file-position read/write, vectored I/O, fsync, accept/connect,
  socket send/receive, splice, or the remaining Linux opcodes;
- registered buffers, provided-buffer rings, long-term pins, zero-copy DMA, or
  an asynchronous direct-pinned default path;
- an unbounded CQ overflow list, hidden busy polling, or a worker per request;
- automatic enablement of async block, async dirty scatter/gather writeback,
  cached readahead, or any evaluator/profile optimization.

Unsupported independent capabilities return an explicit Linux error. They do
not return fake success and do not change behavior based on workload or binary
name.

## Rejected alternatives

- Reading SQEs in place: userspace can mutate operation, pointer, FD, or
  `user_data` after validation.
- Advancing SQ head before CQ admission: a later capacity failure either loses
  the request or requires unbounded overflow state.
- Looking up an FD when execution finally runs: close/reuse can redirect the
  operation to another OFD.
- Holding only a fixed-file index: unregister and re-register can create an
  ABA lifetime error.
- Reusing an `Arc<Frame>` as registered-buffer proof: it lacks long-term pin,
  mapping, COW, invalidation, dirtying, and accounting semantics.
- Completing directly from a fire-and-forget readiness callback: cancel and
  close cannot establish one terminal owner or safe teardown.
- Advertising every feature the pure core can name: mmap, atomics, FD,
  readiness, and execution facts belong to the adapter and require guest
  evidence.
- Enabling experimental block paths to make inline read/write appear async:
  this couples Linux ABI support to an unrelated profile and weakens failure
  boundaries.

## Validation gates

### Pure policy core

- setup flag, reserved-field, power-of-two, clamp, maximum, zero, arithmetic
  overflow, exact offset, and mapping-region tests;
- copied-SQE tests for every supported operation, known/unknown opcodes,
  reserved fields, unsupported flags, fixed-file selection, pointer-length
  overflow, and `u64::MAX` offsets;
- request capacity, CQ credit, wraparound, invalid CQ head, rollback,
  generation exhaustion, stale/foreign token, duplicate `user_data`, and
  exactly-one-terminal-owner tests;
- fixed-file sparse table, atomic publication failure, lease/unregister race,
  table rebuild, generation ABA, allocation failure, and destruction-outside-
  lock tests;
- registration-header tests that distinguish malformed registered-buffer
  requests from well-formed unsupported requests;
- `cargo test`, formatting, clippy with warnings denied, rustdoc, no-default-
  feature, package/extract, and no-std RISC-V/LoongArch target checks.

### TheKernel host integration

- exact 64-bit UAPI size/alignment/offset tests for params, SQ/CQ offsets,
  SQE, CQE, and probe records;
- FD reservation, params copyout, publication, mmap preparation, and every
  allocation failpoint roll back without a visible FD, VMA, page, request
  credit, readiness slot, or OFD leak;
- acquire/copy/commit and CQE/release-tail ordering tests around wrapping and
  malformed shared indexes;
- retained ordinary and fixed OFD tests across dup, close, descriptor reuse,
  unregister, request completion, and final drop;
- shared positioned-I/O tests for access modes, short operations, EOF,
  append-sensitive behavior, usercopy faults, and VFS errors;
- check/arm/check poll, wake/cancel, wake/close, source close, reused request,
  and reentrant callback/drop races;
- legacy signal-mask installation and restoration on every wait result.

### Guest ABI and compatibility

- raw setup tests for every supported flag and malformed/reserved combination;
- `mmap` tests for shared SQ/CQ aliasing, SQEs, wrong offsets/lengths, FD close
  with live mapping, unmap, and process exit;
- raw NOP tests for submission/CQ wrap, batched enter, default stop after the
  first submission-preparation failure, CQ pressure, unchanged SQ head, no
  dropped terminal completion, and corrupt userspace indexes;
- positioned read/write through ordinary and fixed files, including dup/close
  reuse, sparse slots, short I/O, EOF, invalid buffers, and unsupported current
  position;
- one-shot poll for already-ready and later-ready sources plus cancel-not-
  found, cancel-versus-wake, source close, ring close, and teardown pressure;
- register files/unregister/probe and the exact malformed-versus-unsupported
  registered-buffer results;
- a pinned liburing 2.8 subset for queue setup/mmap, NOP, probe, fixed files,
  positioned I/O, poll, cancel, and CQ-pressure behavior.

### Architecture, reliability, and performance

- RISC-V 64 and LoongArch64 builds and boots consuming the packaged crate;
- the raw ABI and liburing subsets on both architectures, including SMP runs;
- deterministic allocation, usercopy, readiness-registration, FD-publication,
  and VMA-publication failpoints;
- stress with concurrent submit, reap, poll wake, cancel, unregister, FD
  close/reuse, VMA unmap, process exit, and ring final close;
- bounded-memory and zero-leak checks under CQ saturation and repeated setup/
  teardown;
- submission, NOP round-trip, positioned-I/O, poll wake-to-CQE, and close
  latency measurements with diagnostics disabled and without enabling an
  experimental storage profile.

## Status and acceptance gate

The RFC remains `draft` while any required raw guest, liburing, failure,
pressure, SMP, or dual-architecture gate is missing. Host unit tests or the
existence of `thekernel-linux-io-uring` are not sufficient to mark the feature
implemented.

The 2026-07-15 checkpoint establishes the following evidence:

- the policy crate passes its 40 focused tests and the canonical Linux-ABI
  workspace formatting, clippy, rustdoc, no-default-feature, packaging,
  provenance, publish-dry-run, and RISC-V/LoongArch `no_std` checks;
- TheKernel passes host compilation for tests with the maintained `bpf`
  feature, formatting, CI-script tests, and release-consumer/provenance fixture
  checks; executing host kernel unit tests remains blocked by the repository's
  existing per-CPU non-PIC linker model;
- release-mode kernels build for both architectures, and the repository-built
  raw guest gate passes on RISC-V and LoongArch through the final
  `THEKERNEL_IO_URING_OK` and `THEKERNEL_SYSTEM_TEST_PASS` markers;
- the semantic runner stops after the final marker, so the separately observed
  LoongArch platform shutdown defect is not misclassified as an io_uring
  teardown failure.

This is a bounded initial-profile integration checkpoint, not production
completion. The pinned liburing 2.8 runtime subset, SMP execution, deterministic
adapter failpoints, and the complete concurrency, pressure, and repeated
setup/teardown matrix above are still required before the status can change.

It may move to `implemented` only when the packaged policy core and TheKernel
consumer pass the complete initial-profile gates above on RISC-V and
LoongArch, and the user-visible probe advertises no capability beyond that
evidence. Later opcodes, native async execution, registered buffers, and
performance mechanisms require dependency-ordered RFC extensions rather than
silently widening this 0.1 contract.
