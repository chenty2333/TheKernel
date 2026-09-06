# thekernel-linux-rseq

`thekernel-linux-rseq` is a `no_std`, `forbid(unsafe_code)` policy leaf for
the Linux v6.6 restartable-sequence ABI. It provides:

- ABI-compatible 32-byte, 32-byte-aligned `RseqArea` and
  `RseqCriticalSection` values with compile-time layout assertions;
- proof-bearing descriptor construction requiring an explicit exclusive user
  address limit for the descriptor object, `start_ip`, post-commit IP, and
  `abort_ip`;
- Linux CPU-ID sentinel decoding and equality checks;
- a one-registration lifecycle with non-wrapping epoch reservations;
- a thread-scoped preempt/signal/migrate event mask with non-wrapping resume
  revisions; and
- the pure `RestartDecision::{NoActive, ClearOnly, Abort}` restart gate.

The profile intentionally accepts only zero `rseq.flags` and zero
`rseq_cs.flags`; any non-zero value on the in-critical restart path is
`EINVAL`. This keeps the component's restart state machine small and
verifiable even though Linux exposes additional historical flag bits.

## Side-effect contract

The crate never performs `rseq(2)`, usercopy, signature-word reads, IP/area
writes, IRQ masking, scheduling, or task ownership. The embedding kernel must
perform those operations in its final IRQ-disabled gate.

Every operation that can be followed by an external side effect reserves all
fallible epoch/revision transitions first:

1. call `prepare_register`, `prepare_unregister`, `begin_resume`,
   `prepare_fork`, or `prepare_exec`;
2. perform the adapter-side usercopy, area clear, signature read, or register
   update; then
3. call the matching `commit_*`/`on_exec_success` (infallible for the returned
   token), or `cancel_*` when the adapter operation failed. A failed fork must
   call `cancel_fork`.

An event revision raised between the side effect and finalization therefore
cannot strand a successful registration or abort behind a stale-plan error.
Abort reservations consume only the events captured by that abort; events
raised while the adapter is handling it remain pending. `ClearOnly` and
`NoActive` never consume pending events.

## Linux lifecycle distinctions

`fork_child(ForkMode::CloneVm)` models `CLONE_VM`: the child starts with no
registration and no pending events. `fork_child(ForkMode::PrivateVm)` models a
private-VM fork: registration and pending events are inherited. Both child
states advance their epoch and revision without wrapping or resetting them to
zero. When an adapter has an external fork side effect, use
`prepare_fork`/`commit_fork`/`cancel_fork` so child construction cannot fail
after a successful fork. A fork plan is a real parent reservation: lifecycle
operations and event publication are rejected until it is committed or
canceled. Its revision/epoch remain consumed on cancel so a later operation
cannot reuse the failed transaction identity.

Exec uses `prepare_exec` followed by `on_exec_success`; only the successful
commit clears registration and pending events. A failed exec must call
`cancel_exec` and leaves the state intact.

## Restart gate ordering and errno boundaries

When `RseqArea::rseq_cs == 0`, pass `None` for the descriptor: the gate returns
`NoActive` without reading a descriptor. For an active pointer, the adapter
must first check the event mask. With no pending preempt, signal, or migration
event, the gate returns `NoActive`: the adapter publishes the CPU fields but
must not copy the descriptor, read its signature, clear `rseq_cs`, or change
the saved IP. With an event pending, the adapter passes a copied
`RseqDescriptor` and the actual word read immediately before `abort_ip`,
together with the registered signature. The gate checks that signature before
the IP interval or flags. Once the signature is valid, an IP outside the
interval is `ClearOnly`; only an in-range IP validates the zero flags and can
return `Abort`.

Descriptor pointer/address/range/abort-location errors map to Linux restart
`EINVAL`. The pure descriptor proof checks only that the descriptor pointer
itself is below the exclusive user limit; the adapter's 32-byte descriptor
usercopy owns cross-limit/copy failures and maps them to `EFAULT`. An
abort-signature address underflow or adapter signature-word usercopy failure
likewise maps through the adapter's `EFAULT` path. An actual abort-signature
mismatch is a distinct `RestartSignatureMismatch` and maps to restart
`EINVAL`; registration-signature mismatch remains `EPERM` for `rseq(2)`
unregister/duplicate checks.

`ThreadRseq`, `RseqRegistrationState`, and every prepare/resume/exec/fork
transaction token are intentionally not `Clone`; copying a pending
reservation would create two possible finalize paths. Use the explicit
`prepare_fork`/`commit_fork`/`cancel_fork` API to obtain a child snapshot.

The crate does not fabricate `EFAULT` after a user-memory access because it
does not access user memory. `DescriptorReadFault` and
`SignatureAddressUnderflow` are the explicit handoff classifications for the
adapter's fault path.
