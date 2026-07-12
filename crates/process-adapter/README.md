# TheKernel Linux process adapter

This unpublished adapter binds TheKernel's durable wait/accounting payload to
the explicit-domain `thekernel-linux-process` 0.1.0 core. It deliberately owns
no hidden global process registry. The kernel creates one `ProcessDomain` and
passes it through its lifecycle integration; this crate only preserves concise
concrete type names while that migration lands.

The adapter adds no compatibility registry and no second process state
machine. In particular, kernel integration must preserve these core 0.1.0
contracts:

- create init through the kernel-owned `ProcessDomain`;
- explicitly serialize a parent's fork admission through final commit or
  rollback against exit and child reparenting for that same parent;
- reserve a fork child's first thread through
  `ProcessAdmission::prepare_thread()` and publish both identities only with
  `ProcessAdmission::commit_with_thread()`;
- reserve later threads through `ProcessDomain::prepare_thread()` and handle
  the fallible publication result;
- perform session creation, process-group creation, group moves, exit, and
  reap through the same explicit domain;
- pass `ProcessDomain::registry()` to child, group, session, and process
  topology queries;
- construct the complete `ZombieSnapshot` before `ProcessDomain::exit()`,
  including for a process that will be reaped immediately; and
- treat the first successful exit snapshot as immutable and handle typed
  duplicate/stale lifecycle results instead of assuming success.
