# TheKernel Linux process adapter

This unpublished adapter binds TheKernel's durable wait/accounting/security
payload to the explicit-domain `thekernel-linux-process` 0.1.0 core. It
deliberately owns no hidden global process registry or zombie side table. The
kernel creates one `ProcessDomain` and chooses the credential parameter used
by `ZombieSnapshot<C>`.

The production `C` is an immutable reference-counted credential which owns its
user namespace. Keeping it generic avoids a dependency from this adapter back
into the kernel while preserving exact `ns_capable()` and ID-mapping inputs
after runtime task state is gone. A raw UID, a default/zero credential, or a
generational shadow registry is not an acceptable substitute.

The adapter adds no compatibility registry and no second process state
machine. In particular, kernel integration must preserve these core 0.1.0
contracts:

- create init through the kernel-owned `ProcessDomain`;
- explicitly serialize a parent's fork admission through final commit or
  rollback against exit and child reparenting for that same parent;
- consume a fork admission through `ProcessAdmission::prepare_initial_thread()`
  and publish the type-bound process/thread pair with its infallible
  `commit()`;
- reserve later threads through `ProcessDomain::prepare_thread()` and use the
  consuming infallible commit after all fallible runtime construction;
- perform session creation, process-group creation, group moves, exit, and
  reap through the same explicit domain;
- pass `ProcessDomain::registry()` to child, group, session, and process
  topology queries;
- construct the complete `Arc<ZombieSnapshot<C>>` before
  `ProcessDomain::exit()`, including the immutable credential/user-namespace
  owner and including for a process that will be reaped immediately; and
- treat the first successful exit snapshot as immutable and handle typed
  duplicate/stale lifecycle results instead of assuming success.

## Fixed-cost exit snapshot ownership

`PreparedZombieSnapshot<C>` moves the only fallible snapshot allocation into
process construction, before either the process or its first thread becomes
visible. `try_new()` reports `PreparedZombieSnapshotError::NoMemory` instead of
using an infallible `Arc` allocation in the last-thread exit path.

The final exit supplies wait status, accounting, and the exact immutable
credential to `initialize()`. That infallible consuming operation writes the
complete payload into the reserved allocation and returns the same allocation
as an `Arc<ZombieSnapshot<C>>`; it neither allocates nor clones the credential.
Because the preparation is consumed, it cannot publish twice. Dropping it
before initialization safely releases uninitialized storage and drops no
partial payload.

The kernel integration should keep exactly one preparation in each unpublished
`ProcessData`, complete it only after the final thread and accounting state are
known, and pass the returned `Arc` directly to `ProcessDomain::exit()` even for
an immediately reaped process.
