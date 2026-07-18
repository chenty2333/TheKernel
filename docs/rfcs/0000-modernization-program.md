# RFC 0000: TheKernel Modernization Program

- Status: accepted
- Date: 2026-07-11
- Owners: TheKernel maintainers

## Decision

TheKernel will evolve through small, dependency-ordered vertical slices. The
program separates generic mechanisms, Linux-visible policy, syscall glue, and
test/profile policy before deciding a crate or repository home.

The work proceeds in nine tracks:

1. migrate the product identity from StarryOS names to TheKernel names while
   retaining complete StarryOS, ArceOS, and third-party provenance;
2. restore upstream manifests, licenses, tests, checksums, and patch ledgers;
3. make fast, pull-request, boot, nightly, and fault-injection gates reusable;
4. remove the remaining unsafe, fake-success, unbounded, and lifecycle P0
   paths in signals, scheduling, files, Unix sockets, and PTYs;
5. introduce immutable credentials, namespace ID mapping, complete exec
   transitions, dumpability, and typed stackable security hooks;
6. stabilize VFS/path, FD/readiness, and VM/fault contracts in that dependency
   order;
7. create a 0.x `thekernel-linux-abi` workspace and migrate the user-copy,
   process, and signal leaves;
8. extract credential, VFS, FD, and MM Linux-support crates after their public
   contracts pass the freeze gate;
9. implement TCP diagnostics, seccomp, packet sockets, io_uring, and
   userfaultfd only after their prerequisite contracts exist.

Explicit `ENOSYS`, `EOPNOTSUPP`, or `EAFNOSUPPORT` is acceptable for an
unimplemented independent capability. Accepted-but-fake behavior, silent
success, hidden busy polling, unbounded resources, and user-triggerable kernel
abort are not acceptable.

## Research and adoption rule

Every major slice starts with a focused source and literature review. The
review must pin the versions or commits inspected and compare at least the
canonical Linux behavior with another relevant production system or research
design when one exists.

Mechanisms are adopted only when they fit TheKernel's no-std Rust ownership,
SMP, interrupt, memory-pressure, and two-architecture constraints. TheKernel
may preserve an external design's semantics while choosing a simpler internal
mechanism. Lock-free, RCU, epoch, per-CPU, and cache-heavy designs require
measurement showing that their complexity pays for itself.

Initial comparison families include:

- Linux immutable credentials, prepare/commit, user namespace mappings, and
  stacked LSM hooks;
- Linux epoll, BSD kqueue, and io_uring readiness registration;
- Linux FOLL_PIN/GUP and MMU notifier rules;
- Linux userfaultfd and Zircon pager fault requests;
- Maple Tree, interval trees, and ordered maps for VMA indexing;
- RCU, epoch reclamation, per-CPU caches, and deferred destruction;
- seL4, Asterinas, and Theseus ownership and isolation techniques.

The list is a research route, not a commitment to copy any mechanism.

## Contract gate

A vertical slice is complete only when it specifies and tests:

- ownership and lifetime;
- fallible admission before publication;
- capacity and per-owner accounting;
- lock ordering and operations forbidden while a lock is held;
- cancellation, timeout, close, process-exit, and namespace-teardown behavior;
- rollback after every partial failure;
- Linux-visible errors, short operations, signal interruption, and restart;
- RISC-V and LoongArch behavior;
- diagnostics that are bounded and default-off on hot paths.

## Public API freeze gate

A module may move into a long-lived public crate only when:

- it does not read implicit kernel globals such as the current task, global FD
  table, or global filesystem context;
- callers pass immutable snapshots, capabilities, and operation context
  explicitly;
- public errors preserve OOM, quota/capacity, unsupported, retry, and semantic
  failure distinctions until the Linux adapter maps them to errno;
- user-triggered paths contain no infallible allocation;
- allocator calls, waker cloning, and destructors do not run under spin or
  interrupt-disabled locks;
- semantic, concurrency, failure, packaging, and two-architecture consumer
  tests exist;
- several consecutive vertical slices have not required incompatible changes
  to its public types or signatures.

Until then, public surfaces remain 0.x, sealed where practical, and
`pub(crate)` by default.

## Synchronous wait checkpoint

Kernel waits prepare every fallible or sleepable resource before entering a
synchronous `block_on` session. Polling may inspect only the waiter's bounded
IRQ-safe state; it may not acquire an operation mutex, copy user memory,
perform fallible admission or allocation, register a callback, or publish a
hardware deadline. Timeout, interruption, and setup failure are finalized only
after the block session has closed.

Futex wake and terminal cancellation share one waiter-state linearization
point. A wake that owns the waiter removes it and contributes to the wake
count; otherwise cancellation marks it terminal before queue removal and a
later wake does not count it. This applies equally to a waiter that was
requeued and to every member of a wait-vector registration.

This terminal ordering was checked on 2026-07-19 against Linux revision
`2687c848e578`: single wait calls
[`futex_unqueue`](https://codebrowser.dev/linux/linux/kernel/futex/waitwake.c.html#689)
before testing timeout or signal state, and wait-vector calls
[`futex_unqueue_multiple`](https://codebrowser.dev/linux/linux/kernel/futex/waitwake.c.html#558)
before the same errors. The GPL source is used only as an observable semantic
reference; no implementation code is copied.

Clock sleeps use fixed per-CPU, per-clock shards. The current implementation
admits 256 slots per shard: one is reserved for the kernel alarm owner and 255
are ordinary slots. Tokens retain owner CPU, slot, and generation, so a
migrated task polls or cancels through the owning shard's spin lock. A timer
callback drains only its local shard in fixed batches. Remote cancellation may
leave a harmless early hardware interrupt on the owner CPU, but it must never
delay a live deadline or cause a late wake.

Ordinary shard or callback-capacity exhaustion is a temporary admission
failure and reaches Linux callers as `EAGAIN`; it is not reported as `EBUSY`
and never falls back to polling or unbounded allocation. The system-reserved
admission class remains distinct. Per-process fairness and aggregate admission
across CPU migration are not claimed by this checkpoint and require workload
evidence before they become policy.

## Checkpoint discipline

Branding, provenance, CI infrastructure, semantic changes, performance
mechanisms, and physical crate moves use separate commits. Each checkpoint is
independently reviewable and revertible. Benchmark results may justify
investigation, but benchmark names and harness output shapes must not affect
kernel semantics.

## Repository policy

The owned Linux-support crates begin in one `thekernel-linux-abi` workspace so
that early 0.x changes remain atomic. Generic ArceOS, smoltcp, FAT, ext4, and
driver mechanisms remain separate upstream or fork lines. Renaming a fork does
not remove its original copyright, license, authors, or history.
