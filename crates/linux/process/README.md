# thekernel-linux-process

`thekernel-linux-process` provides bounded process, thread-group, session, and
zombie lifecycle state without a crate-owned singleton. A kernel explicitly
owns a `ProcessDomain<Z>`; its `ProcessRegistry<Z>` is passed to topology
queries, and independent domains may safely reuse the same PID values.

The generic `Z` is an opaque, caller-defined durable zombie payload retained
by `Arc`. The core never reduces it to a raw UID or a global side table. This
crate does not define Linux wait-status encoding, credential snapshots, CPU
usage, or errno mapping. A Linux ABI adapter can define one immutable payload
containing exactly the state its `wait*`, procfs, accounting, namespace, and
permission paths require.

## Toolchain

Version 0.1.0 is intentionally **nightly-only** and is tested with the rolling
`nightly` toolchain. It uses `Arc::try_new` through Rust's `allocator_api`
feature so process-object allocation can return `ProcessError::NoMemory`.
There is no `rust-version` claim for this package.

```rust
use std::sync::Arc;

use thekernel_linux_process::{ExitOutcome, ProcessDomain};

#[derive(Clone, Debug, Eq, PartialEq)]
struct LinuxZombie {
    wait_status: i32,
    uid: u32,
}

let domain = ProcessDomain::<LinuxZombie>::try_new().unwrap();
let init = domain.try_new_init(1, None).unwrap();
let child_admission = domain.prepare_fork(&init, 2, Some(17)).unwrap();
let initial_thread = child_admission.prepare_thread(2).unwrap();
let child = child_admission
    .commit_with_thread(initial_thread)
    .unwrap();

assert_eq!(
    domain.exit(
        &child,
        Arc::new(LinuxZombie {
            wait_status: 0,
            uid: 0,
        }),
        drop,
    ),
    Ok(ExitOutcome::BecameZombie),
);
assert!(domain.reap(&child).unwrap());
```

## Consumer migration

A kernel adapter defines aliases such as `Process<LinuxZombieSnapshot>` and
owns one explicitly initialized domain. Historical calls change as follows:

- `Process::try_new_init(...)` becomes `PROCESS_DOMAIN.try_new_init(...)`;
- `parent.prepare_fork(...)` becomes `PROCESS_DOMAIN.prepare_fork(&parent, ...)`;
- live thread admission becomes `PROCESS_DOMAIN.prepare_thread(&process, tid)`;
- an unpublished fork consumes its `ProcessAdmission` into
  `prepare_initial_thread()`, then publishes the type-bound pair with the
  infallible composite `commit()`;
- `process.exit(...)` becomes `PROCESS_DOMAIN.exit(&process, snapshot, ...)`,
  making the durable wait/security snapshot mandatory before zombie state;
- `process.reap()` becomes `PROCESS_DOMAIN.reap(&process)`;
- session/group creation and group moves become domain operations over unique,
  liveness-checked identities;
- final thread removal uses `PROCESS_DOMAIN.exit_thread(...)`; its
  `FinalThread(ProcessExitAdmission)` variant binds membership removal and
  zombie authority in one reversible transaction, while `group_exit(code)`
  records the group code atomically;
- an infallible late thread publication returns `ThreadPublicationOutcome`, so
  an adapter can terminate a reservation that crossed an already-linearized
  group exit before making the task runnable;
- child, process-group, and session snapshots receive
  `PROCESS_DOMAIN.registry()`; and
- the historical `ZombieSnapshot` and `ProcessUsage` fields move into the
  kernel's `LinuxZombieSnapshot`/accounting adapter.

The adapter must serialize runtime-resource construction around fork, but the
crate itself now linearizes process/thread publication with exit. Reserved
thread tokens count as lifecycle ownership, stale process Arcs cannot add a
thread after zombie/reap, and an initial-thread token cannot escape a rolled
back process admission.

Domain capacity now bounds total reserved/live thread memberships across all
processes, not merely each process in isolation. `kspin/smp` is mandatory even
for the unpacked standalone package, and concurrent admission, removal,
reparent, exit, and reap tests exercise the real inter-CPU locks.

Every process, group, session, and thread charge uses checked bounded
admission. Release uses non-wrapping compare/update or checked arithmetic; an
impossible duplicate internal release leaves a zero counter at zero instead
of manufacturing `usize::MAX` capacity. Public lifecycle calls return typed
outcomes for stale, duplicate, exhausted, or non-live state and do not depend
on `panic!`, `expect`, or wrapping arithmetic to enforce registry invariants.
Consuming thread and initial-process composite tokens also expose infallible
commit paths after all fallible admission has completed, so an embedding can
place irreversible runtime publication strictly after core lifecycle commit.
