# RFC 0003: Cancellable FD and Readiness Registration Contract

- Status: draft
- Date: 2026-07-12
- Owners: TheKernel maintainers
- Target layers: `thekernel-axpoll` and `thekernel-linux-fd`

## Problem

Linux file descriptors combine several different lifetimes: the numeric
descriptor, the shared open file description, a poll interest, one or more
underlying wake sources, and a waiter or persistent epoll/io_uring request.
Treating readiness registration as a fire-and-forget waker copy leaks slots on
timeout/cancellation and makes close, `EPOLL_CTL_DEL`, one-shot rearm, and
source teardown race-prone.

A single logical operation may fan in to several sources. A pipe wait can
depend on data, space, and peer-close sources; `tee` can depend on both pipes;
a socket can expose different protocol and device queues. Therefore one
low-level token cannot honestly represent a complete `Pollable` registration.

## Evidence reviewed

The design was checked against these exact source snapshots on 2026-07-12:

- Linux `44696aa3a489d2baf58efa61b37833f100072bee`:
  - `fs/eventpoll.c` for the `(file, fd)` interest key, ready/overflow-list
    state machine, IRQ-safe callback path, add rollback, `POLLFREE` teardown,
    level/edge/one-shot behavior, bounded nesting/cycle checks, reverse path
    limits, and per-user watch accounting;
  - `fs/select.c` for the poll-table registration protocol, second readiness
    check after arming, temporary registration teardown, timeout, and signal
    interruption;
  - `io_uring/poll.c` for request ownership, one/two-source registration,
    wait-queue removal, pollfree synchronization, cancellation hash, update,
    multishot rearm, and completion ordering.
- FreeBSD `62e22d7cfc1ca1c25bede6aaeca370c163a9a1ef`:
  - `sys/kern/kern_event.c` for explicit knote attachment/detachment, stable
    filter operations, active/queued/disabled state, `EV_CLEAR`, `EV_ONESHOT`,
    `EV_DISPATCH`, in-flux serialization, and bounded timer resources.
- TheKernel's retained readiness experiment:
  - `a5ecd54047288c63510defd954ee3e283583f950` added bounded cancellable
    per-source registrations;
  - `87815fde7a4219b91641559c3618c947dc7b4934` proved fixed-capacity aggregate
    rollback;
  - `cc09058dc94bd0c3599e3f5538a55a8981026af5` migrated pipe, net, VFS, timer,
    signal, and epoll consumers end to end and exposed the cost of prematurely
    fixing every aggregate to eight sources.
- `thekernel-ax` commit `f0f9f3a8769c262b9aa827d86710f0d6b7665fd5`
  for the first independently packaged, single-source `PollSet` contract.

Linux source is GPL-2.0-only and FreeBSD source is BSD-licensed. This RFC uses
observable semantics and design lessons without copying their source.

## Decision

### 1. The generic primitive is deliberately small

`thekernel-axpoll` owns one readiness-source mechanism:

- crate-owned, non-Linux `IoEvents` values;
- compile-time bounded `PollSet` storage;
- one opaque token per registration, identified by registry, slot, and
  generation;
- explicit full, closed, token-space-exhausted, and invalid-token errors;
- register, waker update, cancel, wake, and terminal close;
- no waker clone, drop, or callback under its IRQ-safe lock;
- independent tokens even for equivalent wakers.

It does not publish a `Pollable` trait in 0.1.0. It does not claim aggregate
FD-readiness, Linux bit compatibility, or epoll semantics.

### 2. Descriptor and open-file-description ownership

`thekernel-linux-fd` keeps these objects distinct:

- `FdEntry`: descriptor number, close-on-exec and descriptor-local metadata;
- `OpenFileDescription`: shared status flags, offset, async owner, locks/leases,
  and the underlying file object;
- `FdTable`: bounded/accounted descriptor publication and close transactions;
- `ReadinessInterest`: Linux event mask, trigger mode, user data, and retained
  source registration;
- `ReadyQueue`: admitted storage for coalesced ready interests.

`dup` shares the open file description but creates descriptor-local state.
`fork` shares descriptions through the copied table; `exec` closes only
close-on-exec entries transactionally. No API reads a global current FD table.

### 3. Two-phase aggregate subscription

A readiness object exposes a planning/arming contract rather than returning a
single token:

```rust
pub trait ReadinessObject {
    fn readiness(&self, interest: InterestMask) -> ReadyMask;
    fn prepare_subscription(
        &self,
        interest: InterestMask,
        budget: &mut SubscriptionBudget,
    ) -> Result<PreparedSubscription<'_>, ReadinessError>;
}
```

The exact Rust surface may use sealed traits and type erasure, but it preserves
these phases:

1. determine and reserve the maximum source count fallibly;
2. allocate owned/lifetime storage and accounting before publication;
3. arm each source, retaining its registry handle and token;
4. roll back all earlier tokens if any source rejects registration;
5. publish the complete aggregate once;
6. update the waker in place or cancel/rebuild when interest topology changes;
7. cancel every live source on success, error, timeout, signal, drop, close,
   `DEL`, `MOD`, process exit, or namespace teardown.

The source count is explicit per object. A small inline representation may be
used, but overflow is handled by fallible pre-reserved storage rather than an
arbitrary global limit or infallible growth.

### 4. Lost-wake and callback protocol

Every blocking operation follows:

1. check readiness or attempt the nonblocking operation;
2. prepare and atomically arm the subscription;
3. check readiness or retry the operation again;
4. sleep only if the second check still blocks;
5. detach or rearm with a generation change before consuming a wake.

Object operations, usercopy, allocation, and sleepable-lock acquisition belong
to the check/prepare phases outside the task's synchronous block session. The
session itself may poll only already-published bounded readiness, interrupt,
and timer tokens. An object callback or a lazy future must not smuggle an
operation back into that session.

A logical call computes one absolute deadline and lazily admits at most one
bounded timer reservation. The reservation survives spurious readiness and
consecutive wait sessions without extending the timeout or repeating timer
admission; each session removes its task waker on completion or cancellation,
and final reservation destruction refunds the slot exactly once. After any
wake, interrupt, expiry, admission failure, or block failure, one final
authoritative operation attempt decides whether completed work wins.

Wake callbacks only publish a bounded token/hint and wake tasks. They do not
allocate, copy to userspace, take sleepable locks, or invoke destructors while
holding source locks. A delayed callback carries a generation and cannot
retire or enqueue a newer registration.

Source teardown first closes/detaches registrations, then waits for or defers
in-flight callbacks, and only then frees callback-visible storage. RCU/epoch
may implement deferred destruction later, but the lifetime handshake is part
of the 0.1 contract.

### 5. Linux bit and error adapters

Raw Linux `POLL*`/`EPOLL*` values are decoded and validated in
`thekernel-linux-fd`; they are never passed through as generic `IoEvents`
bits. Error/hangup conditions are reported according to the Linux interface
even when not included in the requested normal-interest mask.

The generic errors remain distinct until the adapter maps them:

- per-owner/watch quota and configured source capacity;
- fallible storage allocation;
- closed source;
- stale/foreign token;
- unsupported trigger or object kind;
- transient retry.

No full registry may silently replace a waiter, report success, or fall back
to busy polling.

### 6. poll/select and ordinary blocking I/O

`poll`, `ppoll`, `select`, and `pselect` build ephemeral subscriptions from a
single frozen FD-table snapshot. Duplicate FDs preserve Linux result behavior
without unnecessarily duplicating a source subscription when safe to share at
the Linux aggregate layer. Timeout and signal-mask replacement are scoped and
always tear down registrations.

Ordinary blocking read/write/connect/accept uses the same subscription
protocol, so cancellation and close behavior cannot drift from poll/epoll.
Nonblocking status is an open-file-description property and unsupported
changes return an error; no `set_nonblocking()` path silently succeeds.

### 7. epoll

The 0.1 epoll core preserves:

- identity by open file description plus the descriptor used for registration;
- add/modify/delete rollback and close-driven detach;
- bounded per-owner interests, reverse parents, graph walk, nesting, and ready
  storage;
- cycle rejection before publication;
- level, edge, one-shot, and explicit rearm state;
- coalesced IRQ-safe enqueue with admitted ready-queue capacity;
- no lost edge while userspace copyout proceeds without the ready lock;
- fair/bounded scanning and exact partial-copy/error behavior;
- generation-tagged cancellation of retained source subscriptions.

Ready queue capacity is admitted with interest publication. An unexpected
overflow requests a bounded rescan; it does not allocate in the callback,
drop an edge silently, or spin until space appears.

Hidden NAPI or other busy polling is forbidden. A future explicit, bounded,
observable busy-poll option belongs to Linux socket policy and is never the
fallback for a broken wake path.

### 8. io_uring seam

0.1 does not claim io_uring support. It freezes the prerequisites:

- stable request identity plus generation;
- owned aggregate subscription;
- update and cancel by request identity;
- close/teardown handshake;
- one-shot and multishot rearm state;
- completion publication after request ownership is acquired;
- pinned-buffer lifetime supplied by the MM layer.

Later io_uring poll requests reuse the readiness source contract rather than
installing a second fire-and-forget callback system.

## Rejected alternatives

- One `Pollable::register() -> RegistrationToken`: cannot represent fan-in or
  atomic rollback.
- Deduplicating by `Waker::will_wake`: two logical interests may share an
  executor waker, and cancelling one would cancel the other.
- Fixed aggregate capacity of eight as a public invariant: the experiment was
  useful, but topology must be planned per object and overflow must remain
  fallible.
- An unbounded vector grown during arming: partial failure and IRQ-context
  allocation make its behavior unsafe.
- Periodically scanning every FD or epoll interest: hidden busy polling wastes
  CPU and masks missed-wake bugs.
- Copying Linux eventpoll's exact rbtree/RCU internals: preserve semantics and
  lifecycle first; choose indexes and reclamation from profiling.

## Validation gates

### Primitive and aggregate

- exact capacity, zero capacity, independent equivalent wakers, ABA/foreign
  tokens, token-space exhaustion, close, and reentrant waker/drop tests;
- partial aggregate arming failure rolls back every source and accounting
  token;
- wake/cancel/update/close races have one linearized outcome;
- timeout, signal, future drop, and process exit leave zero retained slots.

### Linux semantics

- FD versus OFD behavior across dup/fork/exec/close;
- poll/select duplicate descriptors, invalid descriptors, timeout, signal-mask
  restore, and concurrent close;
- epoll add/mod/del, duplicate OFD/fd keys, LT/ET/ONESHOT, nested epoll, cycle,
  graph limits, watched close, epoll close, and copyout fault;
- pipe, eventfd, timerfd, signalfd, inotify/fanotify, PTY, Unix/TCP/UDP/vsock,
  VFS, AIO, mqueue, and process-exit sources;
- no missed wake in check-arm-check races and no stale callback after teardown.

### Reliability and performance

- deterministic allocation and source-registration failpoints;
- RISC-V and LoongArch build/boot plus LTP poll/select/epoll coverage;
- concurrency stress with close, `DEL`/`MOD`, timeout, signal, and task exit;
- registration, wake-to-run, and epoll scan benchmarks with diagnostics off;
- RCU, epoch, per-CPU queues, or cache-heavy indexes only after profiling.

## 0.1 extraction gate

`thekernel-linux-fd` 0.1.0 may be published only after every in-tree
`FileLike` source implements retained aggregate registration, syscall glue no
longer owns duplicated poll/epoll semantics, and both architectures consume
the packaged `thekernel-axpoll` artifact. The internal map/list and reclamation
algorithms remain private and may evolve during 0.x.
