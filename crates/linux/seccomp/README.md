# thekernel-linux-seccomp

`thekernel-linux-seccomp` is a `no_std`, `forbid(unsafe_code)` Linux seccomp
policy core. It accepts only already copied syscall/filter values and delegates
generic cBPF execution through an explicit executor port; it never dereferences
userspace or obtains an implicit current task, credential, thread group, or
signal context.

Version 0.1.0 provides:

- the native-endian 64-byte Linux `seccomp_data` view and x86_64
  audit-architecture value;
- the seccomp-only classic-BPF wire and opcode profile over a fallible,
  immutable Layer 1 program, with one-to-4096-instruction validation and an
  explicit execution port;
- immutable exact-identity filter ancestry, Linux v6.12's 32768-instruction
  post-migration path accounting, signed action precedence, and newest-filter
  data on a tie;
- an explicit aggregate logical byte budget for live programs and nodes,
  shared clone charges, rollback on failed construction, final-owner refunds,
  and iterative maximum-depth teardown;
- typed Linux v6.12 action values and classification without silently treating
  an unknown action as allow; and
- task-local disabled, strict, and filter modes, stale-safe prepared filter
  publication, and non-mutating per-sibling TSYNC eligibility/preparation.

## Layer boundary

The AX mechanism repository owns the generic classic-BPF instruction,
structural verifier, interpreter, JIT, and input trait. This crate does not
depend on that repository; the embedding kernel converts the verified Linux
profile into its chosen generic executor and supplies it through
`SeccompExecutor`.

This crate owns the Linux seccomp input/opcode profile, immutable filter-stack
policy, aggregate program accounting, action precedence, and reusable
task-state transition values. Its public UAPI constants describe Linux v6.12;
the presence of a constant or `ActionClass` variant is not a claim that an
embedding kernel exposes that operation or lifecycle.

The kernel adapter owns `sock_fprog` copyin, install permission checks,
task-local publication locks, fork/clone inheritance, exec preservation,
strict-mode enforcement, syscall entry ordering, signals, ptrace rechecks,
audit logging, process termination, errno conversion, and `/proc` reporting.
It must reject any action or flag whose complete lifecycle it does not support.

User notification remains a separate FD/readiness/cancellation protocol with
single-completion ownership. This crate contains no listener, queue, response
table, wakeup, or readiness source and does not claim `USER_NOTIF` support.

## Filter publication

A consumer performs the fallible work before its task publication gate:

1. copy the complete userspace instruction array into kernel-owned storage;
2. validate it as a `VerifiedProgram`;
3. snapshot the task's current `FilterChain`;
4. prepare a new immutable leaf with `FilterChain::try_append`, charging the
   consumer's shared `FilterBudget`; and
5. under the task publication gate, call `try_publish_filter` with the exact
   expected and prepared chains.

A stale writer, invalid parent, path limit, budget limit, budget-domain
mismatch, or allocation failure leaves the live task state unchanged. Fork and
clone share the immutable state and its existing charge rather than copying
or charging the program again.

`prepare_synchronized_from` is only a policy primitive. It neither locks nor
mutates a sibling and does not make TSYNC all-or-none. A real TSYNC consumer
must hold a process-wide seccomp mutation gate, freeze a stable sibling set,
validate every sibling, preallocate all seccomp and `no_new_privs` state, and
perform one infallible group commit with Linux failing-TID/ESRCH behavior.

## Path-accounting baseline

Linux v6.12 checks a stacked filter path after classic-BPF preparation and
charges the stored `bpf_prog::len`, not the userspace `sock_fprog.len`.
`VerifiedProgram::len` therefore remains the source cBPF length used for the
per-program 4096-instruction limit and logical byte budget, while
`VerifiedProgram::path_charge` records the separate unblinded cBPF-to-eBPF
migration length used by `FilterChain`.

The frozen conversion charge is three prologue instructions, two instructions
for `RET_K`, five for `DIV_X`, and one or two for a conditional depending on
whether Linux can make one target fall through. A negative immediate compare
also materializes its value in a temporary register. Other accepted seccomp
instructions charge one. Appending a filter adds its charge plus four for each
existing ancestor; a resulting path of exactly 32768 is accepted and a larger
path is rejected without publishing a leaf.

This is an arithmetic compatibility contract, not an eBPF translator or JIT.
It deliberately models the Linux v6.12 unblinded migration path for the
supported x86_64 profile. Direct cBPF-JIT
architectures, native JIT image bytes, `bpf_jit_limit`, and constant-blinding
expansion under `bpf_jit_harden` are not claimed exact by version 0.1.

## Error and stability contract

Program, filter-install, budget-creation, and state-transition errors are typed
policy results. The embedding syscall/prctl adapter maps them only after
applying Linux's call-specific validation and permission order.

The 0.1 contract freezes the seccomp data/profile rules, bounded filter
ancestry and accounting, action selection, exact identity ancestry, prepared
single-task publication, and per-sibling synchronization eligibility. It does
not freeze a task/thread-group lock, credential slot, signal or ptrace model,
audit sink, listener protocol, JIT, eBPF subsystem, or packet-filter adapter.

The crate requires TheKernel's rolling nightly because fallible standard `Arc`
allocation currently uses `allocator_api`. It is checked as a `no_std`
consumer on x86_64.
