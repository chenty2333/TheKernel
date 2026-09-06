# thekernel-linux-io-uring

`thekernel-linux-io-uring` is a `no_std`, `forbid(unsafe_code)` policy and
lifecycle core for a bounded Linux `io_uring` implementation. It accepts
already copied ABI bytes and caller-owned mechanism facts; it never
dereferences shared userspace memory or obtains an implicit current task, FD
table, address space, or filesystem context.

Version 0.1.0 provides:

- strict setup decoding for `CQSIZE`, `CLAMP`, and `NO_SQARRAY`, power-of-two
  SQ/CQ sizing, Linux-compatible initial mmap offsets, and checked mapping
  geometry, advertising only `SINGLE_MMAP`, `NODROP`, `SUBMIT_STABLE`, and
  `POLL_32BITS` after the adapter satisfies their transport contracts;
- x86_64 Linux wire codecs for `io_uring_params`, SQ/CQ offset blocks, probe
  headers and operation records, plus a complete 64-byte SQE field decoder;
- strict enter decoding for `GETEVENTS` and the legacy signal-mask form,
  including exact native `SignalSet` size validation and an explicit adapter
  obligation to restore the temporary mask on every exit;
- one stable 64-byte SQE copy which preserves `user_data` for operation-level
  error completion, classifies opcodes against Linux stable v6.12.35, and
  strictly decodes `NOP`, positioned `READ`/`WRITE`, one-shot `POLL_ADD`, and
  the default user-data form of `ASYNC_CANCEL`;
- typed registration-header decoding for fixed-file registration/unregister
  and probe, with malformed, pinned-UAPI unsupported, and unknown operations
  kept distinct before any userspace array access;
- caller-owned stable ring identities plus bounded, generation-safe request
  slots whose terminal CQ capacity is reserved before work becomes visible;
- explicit prepared, issued, terminal-claim, completion-publication, reap,
  cancel, drain, and close transitions with a single terminal owner, including
  an atomic cancellable/uncancellable executor hand-off;
- serialized two-phase CQE publication plans which keep the completion credit
  charged until userspace consumption is validated and prevent a later tail
  from overtaking an in-progress CQE write; and
- a bounded registered-file table with a non-reused registration epoch,
  generation-checked leases, an explicit aggregate lease budget, one-time
  publication, and incremental whole-table retirement which cannot destroy a
  file owner while accepted work still holds a lease.

## Layer boundary

The crate owns Linux UAPI byte codecs and request/resource/completion/
cancellation/close policy. The embedding TheKernel adapter owns allocation and
mapping of shared pages, acquire/release atomic access to SQ/CQ fields, UAPI
copyin/copyout transport, mmap and FD lifetime integration, VFS and readiness
resolution, MM pinning, wait and signal behavior, execution, and Linux errno
conversion.

An adapter must acquire-load the userspace SQ tail before copying each selected
SQE exactly once into a private 64-byte array. It parses only that private copy
here. For completion, it obtains the sole publication plan, writes the complete
CQE, release-stores the new CQ tail, and commits the plan back to the core.
Other publication and reap operations remain blocked while the plan is in
flight. Shared-page atomics and their architecture-specific mapping guarantees
are deliberately outside this crate.

A request is admitted only after both a request slot and one terminal CQ credit
are available. Cancellation, execution completion, and forced close race for
one terminal permit; losers observe a typed stale/already-claimed result rather
than publishing another CQE. `POLL_ADD` remains cancellable after issue because
its retained registration can be detached. Positioned `READ`/`WRITE` becomes
uncancellable before entering VFS execution, so cancel or close cannot publish
`-ECANCELED` while an irreversible side effect continues. A lost hand-off race
returns the prepared proof for adapter rollback. Beginning table retirement
hides every slot from new lookups but returns each retired owner only after its last
registration-epoch/slot-generation-bound lease is released. The adapter must
allocate a fresh `FileTableId` for every later `REGISTER_FILES` epoch on the
same ring. Failed installation and lease-release operations return the exact
owner or lease in their typed error, so rollback never destroys the final Arc
under the table lock.

## Deliberate 0.1 limits

This first slice does not implement native async workers or io-wq, `SQPOLL` or
`IOPOLL`, linked or multishot requests, personalities, timeouts, buffer
selection, or the full Linux opcode and registration surface. Registered
buffers and `READ_FIXED`/`WRITE_FIXED` use generation-safe leases; the adapter
retains MM pins and uses its existing direct-or-copy I/O fallback when hardware
DMA is unavailable. Positioned reads/writes and poll requests still need a real
consumer executor and retained readiness adapter. Package existence is
therefore not a claim of complete Linux `io_uring` support.

## Error and stability contract

`IoUringError` reports policy and lifecycle failures. The consumer maps those
typed failures to syscall- and operation-specific Linux results. The 0.1 API
freezes checked setup/enter/SQE/registration values, finite admission,
non-wrapping identity, single-terminal ownership, serialized completion
publication, completion-credit lifetime, registered-file lease lifetime, and
close/drain progress. It does not freeze locks, queue/index layout, executor
strategy, RCU/epoch reclamation, page representation, VFS/FD types, raw UAPI
structs, or usercopy.
