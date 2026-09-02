//! Shared, cancellation-safe ownership for kernel asynchronous operations.
//!
//! An operation has precisely one terminal claimant.  Submission mechanisms
//! retain their own payload and completion queues, while this small common
//! object provides the ordering edge shared by AIO, io_uring, FUSE, NFS, and
//! readiness waiters: cancellation becomes visible before waiters are woken,
//! and a worker can never publish after another actor has claimed terminal
//! ownership.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, Ordering};

use axpoll::PollSet;

const ACTIVE: u8 = 0;
const CANCEL_REQUESTED: u8 = 1;
const TERMINAL: u8 = 2;
const TERMINAL_CANCELLED: u8 = 3;
const ISSUING: u8 = 4;

/// Outcome selected by the actor that wins terminal ownership.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalClaim {
    Completed,
    Cancelled,
}

/// A ref-counted cancellation and terminal-claim token.
///
/// The token deliberately carries no user-memory, file, or provider state.
/// Its owners retain those resources independently, so cancellation cannot
/// invalidate a worker's borrowed capability while it is unwinding.
pub struct AsyncOperation {
    state: AtomicU8,
    /// Linux ioprio captured at operation admission.  This belongs to the
    /// operation, not to the kernel worker which happens to execute it.
    io_priority: u16,
    waiters: PollSet,
}

impl AsyncOperation {
    pub(crate) fn new() -> Arc<Self> {
        Self::new_with_io_priority(0)
    }

    /// Creates an operation with its submitter-selected Linux I/O priority.
    /// Scheduling queues and providers consume this metadata directly; worker
    /// tasks intentionally do not pretend to be the submitting Linux thread.
    pub(crate) fn new_with_io_priority(io_priority: u16) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(ACTIVE),
            io_priority,
            waiters: PollSet::new(),
        })
    }

    pub(crate) const fn io_priority(&self) -> u16 {
        self.io_priority
    }

    /// Requests cancellation and wakes every readiness waiter.  `false`
    /// means a worker/canceller has already claimed the terminal transition.
    pub(crate) fn request_cancel(&self) -> bool {
        let changed = self
            .state
            .compare_exchange(
                ACTIVE,
                CANCEL_REQUESTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
        if changed {
            self.waiters.wake();
        }
        changed
    }

    /// Claims the provider issue boundary.  Once this succeeds a backend may
    /// mutate state; cancellation must report that it lost rather than invent
    /// an ECANCELED completion after an irreversible write has started.
    pub(crate) fn begin_issue(&self) -> bool {
        self.state
            .compare_exchange(ACTIVE, ISSUING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Returns whether a still-live operation has been cancelled.
    pub(crate) fn cancellation_requested(&self) -> bool {
        matches!(
            self.state.load(Ordering::Acquire),
            CANCEL_REQUESTED | TERMINAL_CANCELLED
        )
    }

    /// Atomically claims the only terminal transition.  The returned outcome
    /// preserves a cancellation racing a normal worker completion.
    pub(crate) fn claim_terminal(&self) -> Option<TerminalClaim> {
        loop {
            let state = self.state.load(Ordering::Acquire);
            match state {
                ACTIVE | CANCEL_REQUESTED | ISSUING => {
                    if self
                        .state
                        .compare_exchange(
                            state,
                            if state == CANCEL_REQUESTED {
                                TERMINAL_CANCELLED
                            } else {
                                TERMINAL
                            },
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return Some(if state == CANCEL_REQUESTED {
                            TerminalClaim::Cancelled
                        } else {
                            TerminalClaim::Completed
                        });
                    }
                }
                TERMINAL | TERMINAL_CANCELLED => return None,
                _ => unreachable!("invalid async operation state"),
            }
        }
    }

    /// Wait-set used by poll/readiness adapters.  It is intentionally exposed
    /// as a `PollSet`, allowing nested registration without a second wakeup
    /// protocol for each asynchronous subsystem.
    pub(crate) fn waiters(&self) -> &PollSet {
        &self.waiters
    }

    /// Wakes waiters after an external terminal claimant (for example classic
    /// `io_cancel`) has removed the request from its owner registry.
    pub(crate) fn wake_waiters(&self) {
        self.waiters.wake();
    }
}
