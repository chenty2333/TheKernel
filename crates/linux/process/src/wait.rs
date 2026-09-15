//! Pure rules for the `wait4(2)` / `waitid(2)` event ABI.
//!
//! Everything here is a function of the arguments and of one already-sampled
//! child, so it is host-testable and contains no task-table lookups. The
//! kernel side samples the live state and calls these.

/// A `waitid(2)` id type, already translated from the raw `idtype` argument.
///
/// `clone(2)`/`waitpid(2)` spell a target as `P_ALL`, an exact PID, or a
/// process group. `waitid(2)` adds `P_PIDFD`, and Linux additionally accepts
/// `P_PID`/`P_PGID` only with the sign its own `find_child` implementation can
/// represent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitIdType {
    /// `P_ALL`: every child, the id argument is ignored.
    All,
    /// `P_PID`: exactly the child with the given PID.
    Pid(u32),
    /// `P_PGID`: children whose process-group ID matches.
    Pgid(u32),
    /// `P_PIDFD`: the child referenced by an already-validated pidfd.
    PidFd,
}

/// Which of the three Linux wait events a caller is willing to accept.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitEventSelection {
    /// `WEXITED`, or always for `wait4(2)`, which is documented to accept
    /// exited children and has no way to opt out.
    pub exited: bool,
    /// `WSTOPPED`; `wait4(2)` spells this `WUNTRACED`.
    pub stopped: bool,
    /// `WCONTINUED`.
    pub continued: bool,
}

/// One child's currently reportable wait events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitEventState {
    /// The child is a zombie with a published exit payload that this caller may
    /// reap.
    pub exited: bool,
    /// The child has an unreported stop.
    pub stopped: bool,
    /// `wait_consider_task()`'s per-child `ptrace` argument for `stopped`: this
    /// caller is the child's ptracer, or is treated as one because the tracee
    /// is traced from its own thread group (`if (!ptrace_reparented(p))
    /// ptrace = 1;`, `kernel/exit.c:1522-1523`). `wait_task_stopped()` reports
    /// such a stop regardless of `WUNTRACED`:
    ///
    /// ```c
    /// 	/* Traditionally we see ptrace'd stopped tasks regardless of options. */
    /// 	if (!ptrace && !(wo->wo_flags & WUNTRACED))
    /// 		return 0;
    /// ```
    pub ptrace: bool,
    /// The child has an unreported continue.
    pub continued: bool,
}

/// The event Linux's `wait_consider_task()` would report for this child.
///
/// This is deliberately *per child*. Linux walks the child list and, for each
/// child in order, tries exited then stopped then continued before moving on,
/// so the first child with any reportable event wins even when a later child
/// has a "higher priority" event. A global stopped-then-continued-then-exited
/// pass reports a different child, which is observable whenever two children
/// are stopped and exited at the same time.
///
/// The order comes from `wait_consider_task()`: the `EXIT_ZOMBIE` branch is
/// tried first, then `wait_task_stopped()`, then `wait_task_continued()`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitEventKind {
    /// `WEXITED` / a `wait4(2)` child exit: `wait_task_zombie()`.
    Exited,
    /// `WSTOPPED` (or `WUNTRACED` on `wait4(2)`): `wait_task_stopped()`.
    Stopped,
    /// `WCONTINUED`: `wait_task_continued()`.
    Continued,
}

/// Selects the event for one child, or `None` when it has nothing to report.
///
/// A child's stop state is checked before its continue state because that is
/// the order `wait_consider_task()` uses, and a task cannot be simultaneously
/// stopped and continued: `wait_task_continued()` only sets
/// `WCONTINUED` after the task has run again.
pub const fn select_child_event(
    state: WaitEventState,
    selection: WaitEventSelection,
) -> Option<WaitEventKind> {
    if state.exited && selection.exited {
        return Some(WaitEventKind::Exited);
    }
    if state.stopped && (state.ptrace || selection.stopped) {
        return Some(WaitEventKind::Stopped);
    }
    if state.continued && selection.continued {
        return Some(WaitEventKind::Continued);
    }
    None
}

/// Whether a zombie group leader must be held back because it still has live
/// threads.
///
/// Linux's `delay_group_leader()` returns true while any thread in the
/// thread-group is still alive: the leader's exit is only reported once the
/// whole group is dead, because ``the leader's ``exit_signal`` is delivered
/// when the *last* thread exits. Reporting it early would let a caller reap a
/// group that is still running and then lose the real exit notification.
pub const fn zombie_is_delayed(zombie: bool, live_threads: usize) -> bool {
    zombie && live_threads > 1
}

/// Validation verdict for a `waitid(2)` `idtype`/`id` pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitIdError {
    /// The `idtype` is not one of the four Linux accepts. `EINVAL`.
    InvalidIdType,
    /// The `id` cannot be represented for this `idtype`. `EINVAL`.
    InvalidId,
}

/// Validates the `idtype`/`id` pair exactly as `kernel_waitid_prepare()` does.
///
/// Linux rejects the *combination*, not just the type: `P_PID` needs an id
/// above zero and `P_PGID` needs one below zero, because both funnel into
/// `find_child` through a single signed field. Accepting `id == 0` for `P_PID`
/// would silently wait on the caller's own process group instead.
///
/// `P_PIDFD` is accepted here without inspecting the descriptor; the caller
/// resolves it and reports `EBADF` for a descriptor that is not a pidfd.
pub const fn validate_id_type(idtype: i32, id: i32) -> Result<WaitIdType, WaitIdError> {
    match idtype {
        // `P_ALL`
        0 => Ok(WaitIdType::All),
        // `P_PID`: `upid <= 0` is EINVAL.
        1 => {
            if id <= 0 {
                Err(WaitIdError::InvalidId)
            } else {
                Ok(WaitIdType::Pid(id as u32))
            }
        }
        // `P_PGID`: `upid < 0` is EINVAL.
        2 => {
            if id < 0 {
                Err(WaitIdError::InvalidId)
            } else {
                Ok(WaitIdType::Pgid(id as u32))
            }
        }
        // `P_PIDFD`: also `upid < 0` is EINVAL, before the descriptor is
        // resolved, so a negative argument cannot reach `pidfd_get_pid()`.
        3 => {
            if id < 0 {
                Err(WaitIdError::InvalidId)
            } else {
                Ok(WaitIdType::PidFd)
            }
        }
        _ => Err(WaitIdError::InvalidIdType),
    }
}

/// The `options` bits `wait4(2)` forwards to `kernel_wait4()`.
///
/// `wait4(2)` has no `WEXITED`: every `wait4` call waits for exited children,
/// which is why `kernel_wait4()` ORs `WEXITED` into the flags it passes on.
/// `WSTOPPED` is the internal spelling of `WUNTRACED` and is accepted through
/// the same argument, so both bits must be tolerated even though the ABI
/// constant userspace passes is `WUNTRACED`.
pub const WAIT4_OPTIONS_ALLOWED: u32 = 0x0000_0001 // WNOHANG
    | 0x0000_0002 // WUNTRACED == WSTOPPED
    | 0x0000_0008 // WCONTINUED
    | 0x2000_0000 // __WNOTHREAD
    | 0x4000_0000 // __WALL
    | 0x8000_0000; // __WCLONE

/// The three event bits `waitid(2)` requires at least one of.
pub const WAITID_EVENT_FLAGS: u32 = 0x0000_0002 // WSTOPPED == WUNTRACED
    | 0x0000_0004 // WEXITED
    | 0x0000_0008; // WCONTINUED

/// The full `waitid(2)` option mask Linux accepts.
pub const WAITID_OPTIONS_ALLOWED: u32 = 0x0000_0001 // WNOHANG
    | WAITID_EVENT_FLAGS
    | 0x0100_0000 // WNOWAIT
    | 0x2000_0000 // __WNOTHREAD
    | 0x4000_0000 // __WALL
    | 0x8000_0000; // __WCLONE

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: WaitEventSelection = WaitEventSelection {
        exited: true,
        stopped: true,
        continued: true,
    };
    const EXITED_ONLY: WaitEventSelection = WaitEventSelection {
        exited: true,
        stopped: false,
        continued: false,
    };

    #[test]
    fn exited_wins_over_stopped_for_the_same_child() {
        let state = WaitEventState {
            exited: true,
            stopped: true,
            ptrace: false,
            continued: true,
        };
        assert_eq!(select_child_event(state, ALL), Some(WaitEventKind::Exited));
    }

    #[test]
    fn stopped_wins_over_continued_for_the_same_child() {
        let state = WaitEventState {
            exited: false,
            stopped: true,
            ptrace: false,
            continued: true,
        };
        assert_eq!(select_child_event(state, ALL), Some(WaitEventKind::Stopped));
    }

    #[test]
    fn a_child_with_no_selected_event_reports_nothing() {
        let state = WaitEventState {
            exited: true,
            stopped: true,
            ptrace: false,
            continued: true,
        };
        assert_eq!(select_child_event(state, EXITED_ONLY), {
            assert!(EXITED_ONLY.exited);
            Some(WaitEventKind::Exited)
        });
        let none = WaitEventSelection {
            exited: false,
            stopped: false,
            continued: false,
        };
        assert_eq!(select_child_event(state, none), None);
    }

    #[test]
    fn a_bare_stop_is_not_reported_without_wuntraced() {
        // `wait_task_stopped()` returns 0 for a non-ptrace child unless
        // WUNTRACED/WSTOPPED is set.
        let state = WaitEventState {
            exited: false,
            stopped: true,
            ptrace: false,
            continued: true,
        };
        assert_eq!(
            select_child_event(
                state,
                WaitEventSelection {
                    exited: true,
                    stopped: false,
                    continued: true,
                }
            ),
            Some(WaitEventKind::Continued)
        );
    }

    #[test]
    fn a_ptrace_stop_is_reported_without_wuntraced() {
        // `wait_task_stopped()`: "Traditionally we see ptrace'd stopped tasks
        // regardless of options." `wait4(2)` passes no option bits at all, so a
        // tracer waiting for a `PTRACE_TRACEME` child that stopped must still
        // be told about the stop.
        let state = WaitEventState {
            exited: false,
            stopped: true,
            ptrace: true,
            continued: false,
        };
        let bare = WaitEventSelection {
            exited: true,
            stopped: false,
            continued: false,
        };
        assert_eq!(select_child_event(state, bare), Some(WaitEventKind::Stopped));
        // A job-control stop of a non-ptrace child keeps the option gate, so
        // the same bare request reports nothing for it.
        assert_eq!(
            select_child_event(
                WaitEventState {
                    ptrace: false,
                    ..state
                },
                bare
            ),
            None
        );
    }

    #[test]
    fn a_stopped_child_beats_a_later_exited_sibling_in_per_child_order() {
        // The regression this guards: child A is stopped, child B has exited.
        // Linux reports A (first child, stopped); a global exited-first pass
        // would report B.
        let a = WaitEventState {
            exited: false,
            stopped: true,
            ptrace: false,
            continued: false,
        };
        let b = WaitEventState {
            exited: true,
            stopped: false,
            ptrace: false,
            continued: false,
        };
        let per_child = [a, b]
            .into_iter()
            .find_map(|state| select_child_event(state, ALL));
        assert_eq!(per_child, Some(WaitEventKind::Stopped));
    }

    #[test]
    fn a_zombie_leader_with_live_threads_is_delayed() {
        assert!(zombie_is_delayed(true, 2));
        assert!(zombie_is_delayed(true, 7));
        assert!(!zombie_is_delayed(true, 1));
        assert!(!zombie_is_delayed(false, 1));
        assert!(!zombie_is_delayed(false, 3));
    }

    #[test]
    fn id_type_validation_matches_linux_sign_rules() {
        assert_eq!(validate_id_type(0, 0), Ok(WaitIdType::All));
        assert_eq!(validate_id_type(0, -1), Ok(WaitIdType::All));
        assert_eq!(validate_id_type(1, 42), Ok(WaitIdType::Pid(42)));
        assert_eq!(validate_id_type(1, 0), Err(WaitIdError::InvalidId));
        assert_eq!(validate_id_type(1, -1), Err(WaitIdError::InvalidId));
        assert_eq!(validate_id_type(2, 0), Ok(WaitIdType::Pgid(0)));
        assert_eq!(validate_id_type(2, 7), Ok(WaitIdType::Pgid(7)));
        assert_eq!(validate_id_type(2, -1), Err(WaitIdError::InvalidId));
        assert_eq!(validate_id_type(3, 9), Ok(WaitIdType::PidFd));
        assert_eq!(validate_id_type(4, 0), Err(WaitIdError::InvalidIdType));
        assert_eq!(validate_id_type(-1, 0), Err(WaitIdError::InvalidIdType));
    }

    #[test]
    fn wait4_and_waitid_option_masks_are_the_linux_masks() {
        // The exact expression from `kernel_wait4()`:
        //   WNOHANG|WUNTRACED|WCONTINUED|__WNOTHREAD|__WCLONE|__WALL
        assert_eq!(WAIT4_OPTIONS_ALLOWED, 0xe000_000b);
        // `wait4` has no WEXITED and no WNOWAIT, so both are rejected.
        assert_eq!(WAIT4_OPTIONS_ALLOWED & 0x0000_0004, 0);
        assert_eq!(WAIT4_OPTIONS_ALLOWED & 0x0100_0000, 0);
        // WUNTRACED and WSTOPPED are the same bit.
        assert_eq!(WAIT4_OPTIONS_ALLOWED & 0x0000_0002, 0x0000_0002);

        // `kernel_waitid_prepare()`'s mask, plus WNOWAIT which it also allows.
        assert_eq!(WAITID_OPTIONS_ALLOWED, 0xe100_000f);
        assert_eq!(WAITID_EVENT_FLAGS, 0x0000_000e);
        assert_eq!(WAITID_OPTIONS_ALLOWED & WAITID_EVENT_FLAGS, WAITID_EVENT_FLAGS);
        assert_eq!(WAITID_OPTIONS_ALLOWED & 0x0100_0000, 0x0100_0000);
    }

    #[test]
    fn pidfd_id_type_rejects_a_negative_id() {
        assert_eq!(validate_id_type(3, -1), Err(WaitIdError::InvalidId));
    }
}
