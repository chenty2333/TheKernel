# Vendored source record: `starry-signal`

## Immutable published baseline

- Registry package: `starry-signal` `0.3.0`
- crates.io archive SHA-256:
  `f72adf2bff529986c36c6b3920332afbefd0f6f6178855347f1bac15f4304d37`
- Repository declared by the package:
  <https://github.com/Starry-OS/starry-signal>
- Source commit recorded by Cargo:
  `0a39846c582895555816145f47f82ceb0c89aa62`
- Cargo VCS dirty flag: absent; the recorded commit is the release source
  identity.
- Authors: Mivik `<mivikq@gmail.com>` and 朝倉水希
  `<asakuramizu111@gmail.com>`
- License: `Apache-2.0`; `LICENSE` matches the registry archive.
- Original manifest: `Cargo.toml.orig`
- Cargo source record: `.cargo_vcs_info.json`

The checksum above was verified against the downloaded crates.io archive.

## TheKernel patch lineage

- `53d3c0acc40fdd7cab7df3c0cec7662b7466047a` imported the crate and
  centralized syscall-restart enrollment.
- `abcba9ccef47cf4cd9b9083a4ee4c5ceedfbb6b1` added compatibility accessors.
- `f8b882702c158444dc4e02a1112ffd84b769b73d` and
  `becd8e37f77a6e9768f77b724ab1542f2a6d374c` refined blocked/ignored signal
  behavior and ABI fields.
- `d38fb1b96d108942e8c52218a7d934db1a24fe72` added fallible endpoint
  construction, publication rollback, and delivered-signal restart metadata.
- `2ab12a4` made `rt_sigreturn` frame restoration transactional, and
  `dfa182f` kept userspace signal-action addresses as raw integers across the
  copy boundary.

The current bounded-signal modernization slice additionally maintains:

- restartability metadata returned with delivered signals;
- fallible thread-signal endpoint allocation and explicit registration tokens;
- pending/active/cancelled registry publication, so rollback entries are never
  selected for delivery;
- one fixed inline information slot per standard signal and intrusive FIFO
  nodes only for real-time signals;
- shared atomic queue accounts, rollback-safe double charging, and exact
  refund on dequeue, per-signal flush, manager teardown, or allocation failure;
- allocation-free publication under the pending spin lock: allocation,
  account-Arc acquisition, and unused-node destruction all happen outside;
- an explicit publication outcome, separating a record owned by this send
  from ignored or coalesced signals while retaining wakeup selection;
- transferable pre-publication ownership for `PreparedSignal`, allowing
  one-shot kernel facilities to reserve an accounted RT record before they
  consume their registration;
- complete pre-publication siginfo inspection and same-signo replacement on
  `PreparedSignal`; replacement retains the admitted queue node and both
  account charges instead of degrading retained state to a signal number;
- a copy-on-write thread registry plus an `action_update` writer mutex.
  TheKernel enables the crate's `multitask` feature, making this mutex the
  sleepable `axsync::Mutex`; the short actions/pending locks only publish or
  detach already-owned state;
- `kspin/smp` in the maintained `Cargo.toml` so standalone concurrent tests
  use a real inter-CPU lock instead of the single-core no-lock specialization;
- ABI flag and `SignalInfo` compatibility updates.

The bounded queue and low-resource behavior are compared against Linux commit
`dd3210c47e8d3ac6b4e9141fc68acc03b38c0ba3`, primarily
`kernel/signal.c`, `include/linux/signal_types.h`, and the
`ucounts/UCOUNT_RLIMIT_SIGPENDING` accounting path.

TheKernel's kernel-side integration of this crate additionally follows the
ownership topology in Linux's `kernel/ptrace.c`,
`kernel/time/posix-timers.c`, `ipc/mqueue.c`, `fs/signalfd.c`, and
`include/uapi/linux/signalfd.h` at that same source identity:

- a ptrace signal-delivery stop retains the complete `PreparedSignal`, target,
  siginfo, intrusive RT node, and account charge. `PTRACE_GETSIGINFO` and
  `PTRACE_SETSIGINFO` operate on that retained record; timer ID/generation
  fields remain kernel-owned. Continue with the same signal publishes the
  exact record, while discard or signal replacement releases it exactly once;
- POSIX timers use strict RT admission. Temporary queue pressure leaves a
  generation-tokened deferred event with a sleeping exponential retry from
  1 ms to 1 s. Later periodic expiries merge their overruns into that same
  outstanding retry rather than accumulating stale alarm entries; rearm,
  deletion, consumption, or ptrace discard invalidates the generation. This is
  deliberately different from permanent timer
  preallocation and is recorded as kernel integration, not a crate feature;
- `mq_notify(SIGEV_SIGNAL)` adopts Linux's `SIGQUEUE_PREALLOC` ownership idea:
  registration reserves the strict queued record and captures registering
  PID, real UID, target process identity, and the full pointer-width value. A
  global weak queue registry plus an atomic active token makes reservations on
  unlinked-but-open queues discoverable and cancellable during owner exit,
  including the race where a sender already took the notifier;
- signalfd projection exposes timer ID/overrun/value, queued or mqueue
  PID/UID/value, and SIGIO fd/band in the Linux 128-byte ABI layout instead of
  silently returning zeroed source-specific fields.

These kernel integration rules have focused state/ownership tests in addition
to the crate tests. They do not claim that ptrace, POSIX timers, mqueue, or
signalfd as a whole have been extracted into this crate.

All six manifest-declared integration tests, the human-readable maintained
manifest, Cargo VCS record, and Apache-2.0 license remain present. The pristine
registry archive is identified by the checksum above; local dependency/feature
changes live only in the maintained `Cargo.toml` and are part of this explicit
patch ledger rather than being attributed to upstream. `Cargo.toml.orig`
remains the exact published manifest. The test adapter avoids a
`MaybeUninit` convenience API unavailable on TheKernel's pinned nightly, and
the blocked-plus-ignored test reflects Linux behavior: generation is queued
while blocked even though it would be discarded while unblocked.

## Known boundary

Linux updates dispositions under a pre-existing `sighand->siglock` without
allocating. This fork snapshots the fallible thread registry before committing
an ignored-action flush, so `try_register` and `try_replace_action` can still
report allocation failure. It does not expose a contention retry failure or
silently perform a partial flush. TheKernel explicitly enables `multitask`;
featureless standalone builds retain axsync's SpinNoIrq fallback and are used
only as the no-runtime test/build baseline.

When rebasing, use the verified crate archive as the pristine baseline and
preserve the explicit registration/rollback contract. Do not infer safety or
Linux semantic completeness merely from the package name.
