# tk-axpoll

Bounded I/O readiness registration and wakeup primitives for `no_std` systems.
This maintained workspace fork of upstream `axpoll` is not published to
crates.io (`publish = false`). Its Rust library name remains `axpoll`.
Use the workspace dependency or the local `crates/ax/tk-axpoll` path; no
standalone `tk-*` repository or registry release is implied.

The crate uses the root-pinned `nightly-2026-08-23` toolchain
(`rustc 1.100.0-nightly`, commit `c54751567`, dated 2026-08-22).
`rust-version` inherits the workspace's `1.100`; this is not a stable-Rust
compatibility guarantee.

## Registration lifecycle

`PollSet<const CAPACITY: usize = 64>` has a compile-time capacity and never
grows or silently evicts a waiter. Registration returns an opaque token:

```rust
use axpoll::{PollSet, RegisterError, RegistrationToken, UpdateError};
use core::task::Context;

fn arm_wait(
    waiters: &PollSet<8>,
    registration: &mut Option<RegistrationToken>,
    context: &Context<'_>,
) -> Result<(), RegisterError> {
    if let Some(token) = *registration {
        match waiters.update(token, context.waker()) {
            Ok(()) => return Ok(()),
            Err(UpdateError::InvalidToken) => *registration = None,
            Err(UpdateError::Closed) => return Err(RegisterError::Closed),
        }
    }

    *registration = Some(waiters.register(context.waker())?);
    Ok(())
}
```

Keep the token while a wait is pending and call `cancel(token)` when the wait
completes or its future is dropped. Reusing a slot increments its generation,
so stale tokens cannot cancel a later waiter. Tokens from another `PollSet` are
also rejected.

Every `register` call creates an independent token and consumes one slot, even
when two registrations use equivalent wakers. This is necessary because the
same executor waker may represent separate waits or event interests; cancelling
one must not cancel the other. A logical waiter that is polled again retains its
token and uses `update(token, waker)` rather than calling `register` again.

A registration presented to a full set gets `RegisterError::Full`; no old
waiter is replaced or spuriously woken. `update` changes a live registration's
waker without changing its token.

`wake()` is a one-shot drain and leaves the registry open. `close()` atomically
marks it closed, drains and wakes current waiters, and rejects later
registrations. Dropping the set closes it. Registration, cancellation, wake,
and close races are lock-linearized, while RawWaker clone, destruction, and
wake callbacks run outside the IRQ-safe lock and may re-enter the registry.
The package enables `kspin/smp` explicitly, so this guarantee does not depend on
feature unification in a larger kernel workspace.

## Explicit prepare/arm seam

`prepare(waker)` clones and owns the waker before any source slot is visible;
`arm(prepared)` publishes exactly one source and returns exactly one token. An
arm failure returns `ArmRegistrationError`, from which the unpublished
preparation can be recovered and dropped outside source locks. A multi-source
consumer should reserve its finite token storage, prepare every source, arm
them one by one, and cancel already armed tokens on the first failure.

This seam is intentionally not an aggregate-token API. The consumer still owns
the source topology, checked maximum, partial-arm rollback, interest update,
and cancellation of every individual token.

## Generic events, not Linux constants

`IoEvents` uses neutral names and crate-owned bit values:

| Inherited name | Generic name |
| --- | --- |
| `IN` | `READABLE` |
| `PRI` | `PRIORITY` |
| `OUT` | `WRITABLE` |
| `ERR` | `ERROR` |
| `HUP` | `HANGUP` |
| `NVAL` | `INVALID` |
| `RDNORM` / `RDBAND` | `READ_NORMAL` / `READ_BAND` |
| `WRNORM` / `WRBAND` | `WRITE_NORMAL` / `WRITE_BAND` |
| `MSG` / `REMOVE` / `RDHUP` | `MESSAGE` / `REMOVED` / `READ_HANGUP` |
| `ALWAYS_POLL` | `ALWAYS` |

The crate no longer depends on `linux-raw-sys`. A Linux ABI adapter should map
Linux `POLL*` input bits into these events and map readiness back at the syscall
boundary; raw Linux values must not be passed through as `IoEvents::from_bits`.

`Pollable` exposes object readiness and bounded registration without file
identities or Linux errno. `PreparedPollRegistration` and `PollRegistration`
own composite subscriptions, including partial-arm rollback, waker updates,
and drop cancellation. Aggregates currently share a fixed 65,536-credit
admission account; this limit is a workspace policy choice, not a Linux ABI
constant. The product readiness adapter owns errno mapping and operation retry
policy.

The crate always requires Rust's `alloc` runtime for `Waker` ownership and its
`Wake for PollSet` implementation. It has no empty `alloc` feature flag: turning
off a feature never falsely claims an allocator-free build.
