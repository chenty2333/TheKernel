//! Tracer-first terminal wait ownership, retained in the existing zombie owner.
//!
//! Linux 7.2.3 kernel/exit.c wait_task_zombie and kernel/ptrace.c
//! __ptrace_detach distinguish a tracer's acknowledgement from the real parent's
//! reap. Only a session value survives here; no live task or memory is retained.

use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PtraceExitState {
    #[default]
    Untraced,
    /// Core zombie exists before final notification runs; it is not reportable.
    Pending(PtraceSession),
    Held(PtraceSession),
    /// Parent notification/autoreap is in flight; neither side can steal reap.
    HandingOff(PtraceSession),
    /// A tracer consumed/released the terminal report. Final exit must not
    /// notify the natural parent again if handoff raced its notification edge.
    HandedOff,
}

impl PtraceExitState {
    pub(crate) fn session(self) -> Option<PtraceSession> {
        match self {
            Self::Pending(session) | Self::Held(session) | Self::HandingOff(session) => {
                Some(session)
            }
            _ => None,
        }
    }

    fn release(&mut self, expected: PtraceSession, published: bool) -> bool {
        if !matches!(*self, Self::Pending(session) | Self::Held(session) if session == expected) {
            return false;
        }
        *self = if published {
            Self::HandingOff(expected)
        } else {
            Self::Untraced
        };
        true
    }

    fn report_matches(self, expected: Option<PtraceSession>) -> bool {
        match self {
            Self::Held(session) => expected == Some(session),
            Self::Untraced | Self::HandedOff => expected.is_none(),
            Self::Pending(_) | Self::HandingOff(_) => false,
        }
    }

    fn notified(&mut self, expected: PtraceSession) -> bool {
        if *self != Self::Pending(expected) {
            return false;
        }
        *self = Self::Held(expected);
        true
    }

    fn handed_off(&mut self, expected: PtraceSession) {
        if *self == Self::HandingOff(expected) {
            *self = Self::HandedOff;
        }
    }
}

fn retained_exit_owner(process: &Process) -> Option<GroupLeaderSignalOwner> {
    if let Some(snapshot) = process.zombie_payload() {
        return Some(snapshot.reap_owner.clone());
    }
    if let Ok(data) = crate::task::get_process_data(process.pid()) {
        return Some(data.group_leader_signal_owner());
    }
    None
}

pub(crate) fn retained_ptrace_exit_session(process: &Process) -> Option<PtraceSession> {
    retained_exit_owner(process)?
        .lock()
        .as_ref()?
        .ptrace_exit
        .session()
}

/// Transfer before clearing the live relationship, under its action gate. The
/// tracer reverse link remains until terminal wait or tracer teardown.
pub(crate) fn retain_ptrace_exit(thread: &super::super::Thread) {
    let data = &thread.proc_data;
    let session = thread.ptrace_active_session();
    let owner = data.group_leader_signal_owner();
    let mut owner = owner.lock();
    let Some(identity) = owner.as_mut() else {
        return;
    };
    identity.exit_autoreap = data.autoreap();
    if thread.is_thread_group_leader()
        && let Some(session) = session
    {
        identity.ptrace_exit = PtraceExitState::Pending(session);
    }
}

/// Exact terminal report admission. Natural parents cannot steal a report
/// still held by a tracer, including WNOWAIT observations.
pub(crate) fn ptrace_exit_report_matches(
    process: &Process,
    expected: Option<PtraceSession>,
) -> bool {
    let Some(owner) = retained_exit_owner(process) else {
        return false;
    };
    let owner = owner.lock();
    owner
        .as_ref()
        .is_some_and(|identity| identity.ptrace_exit.report_matches(expected))
}

/// Returns whether the terminal relationship was retired. Notification/reap
/// runs after the owner lock drops. Before zombie publication final exit will
/// perform normal parent notification instead.
pub(crate) fn release_ptrace_exit(
    process: &Arc<Process>,
    expected: PtraceSession,
    parent_publication: Option<&TaskParentPublicationGuard<'_>>,
) -> bool {
    // Non-final tracer exit already owns this gate. Reuse it rather than
    // recursively acquiring the global graph lock; final exit supplies None.
    let _parent_publication = parent_publication
        .is_none()
        .then(lock_task_parent_publication);
    let Some(owner) = retained_exit_owner(process) else {
        return false;
    };
    let (released, published) = {
        let mut owner = owner.lock();
        let Some(identity) = owner.as_mut() else {
            return false;
        };
        let published = process.is_zombie();
        (identity.ptrace_exit.release(expected, published), published)
    };
    if released && published {
        crate::task::ops::notify_detached_ptrace_zombie(process, expected.tracer);
        finish_ptrace_exit_handoff(process, expected);
    }
    released
}

/// Claim a tracer report once, without reaping a different real parent's child.
/// Returns None when another waiter/teardown already won, Some(true) when this
/// tracer is also the real parent and should perform the sole ordinary reap.
pub(crate) fn claim_ptrace_exit(
    process: &Arc<Process>,
    expected: PtraceSession,
    waiter: &ProcessData,
) -> Option<bool> {
    // Parent selection, notification and ready publication form one graph
    // transaction. Reparent callbacks cannot skip an in-flight handoff and
    // then miss the terminal SIGCHLD at its new parent.
    let _parent_publication = lock_task_parent_publication();
    let snapshot = process.zombie_payload()?;
    {
        let mut owner = snapshot.reap_owner.lock();
        let identity = owner.as_mut()?;
        if !identity.ptrace_exit.report_matches(Some(expected))
            || !identity.ptrace_exit.release(expected, true)
        {
            return None;
        }
    }
    let tracee_tid = snapshot.reap_owner.lock().as_ref()?.registration_tid;
    waiter.remove_ptrace_tracee(PtraceReverseLink::new(tracee_tid, expected));
    let real_parent = process
        .parent()
        .is_some_and(|parent| parent.pid() == waiter.proc.pid());
    if !real_parent {
        crate::task::ops::notify_detached_ptrace_zombie(process, expected.tracer);
        finish_ptrace_exit_handoff(process, expected);
    }
    Some(real_parent)
}

fn finish_ptrace_exit_handoff(process: &Process, expected: PtraceSession) {
    // Capture the waiter before publishing readiness: a winning wait may reap
    // and clear the child's parent link before the post-publication wake.
    let parent = process
        .parent()
        .and_then(|parent| crate::task::get_process_data(parent.pid()).ok());
    if let Some(owner) = retained_exit_owner(process)
        && let Some(identity) = owner.lock().as_mut()
    {
        identity.ptrace_exit.handed_off(expected);
    }
    // The notification wake may have raced a poll while HandingOff excluded
    // status consumption. Publish a new wake after making it reportable.
    if let Some(parent) = parent {
        parent.child_exit_event.wake();
    }
}

pub(crate) fn mark_ptrace_exit_notified(data: &ProcessData, expected: PtraceSession) {
    let owner = data.group_leader_signal_owner();
    if let Some(identity) = owner.lock().as_mut() {
        identity.ptrace_exit.notified(expected);
    }
}

pub(crate) fn retained_exit_autoreap(process: &Process) -> bool {
    retained_exit_owner(process).is_some_and(|owner| {
        owner
            .lock()
            .as_ref()
            .is_some_and(|identity| identity.exit_autoreap)
    })
}

/// A direct-parent tracer keeps the child's configured exit signal. A tracer
/// in a different process gets SIGCHLD, including clone children with no signal.
pub(crate) fn ptrace_exit_notification_signal(
    exit_signal: Option<u8>,
    tracer_is_real_parent: bool,
) -> Option<u8> {
    if tracer_is_real_parent {
        exit_signal
    } else {
        Some(linux_raw_sys::general::SIGCHLD as u8)
    }
}

/// Select exactly one notification destination after durable publication.
pub(crate) fn ptrace_exit_notification(data: &ProcessData) -> PtraceExitState {
    let owner = data.group_leader_signal_owner();
    let owner = owner.lock();
    owner
        .as_ref()
        .map_or(PtraceExitState::HandedOff, |identity| identity.ptrace_exit)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(generation: u64) -> PtraceSession {
        PtraceSession {
            tracer: 7,
            tracer_kernel_tid: 70,
            generation,
        }
    }
    #[test]
    fn terminal_handoff_requires_the_exact_session_and_is_once_only() {
        let mut state = PtraceExitState::Held(session(2));
        assert!(!state.release(session(1), true));
        assert_eq!(state.session(), Some(session(2)));
        assert!(state.release(session(2), true));
        assert_eq!(state, PtraceExitState::HandingOff(session(2)));
        assert!(!state.release(session(2), true));
        assert!(!state.report_matches(None));
        state.handed_off(session(2));
        assert_eq!(state, PtraceExitState::HandedOff);
        assert!(state.report_matches(None));
    }
    #[test]
    fn terminal_report_waits_for_notification_and_cannot_revive_after_teardown() {
        let mut state = PtraceExitState::Pending(session(1));
        assert!(!state.report_matches(Some(session(1))));
        assert!(!state.report_matches(None));
        assert!(!state.notified(session(2)));
        assert!(state.notified(session(1)));
        assert!(state.report_matches(Some(session(1))));
        assert!(state.release(session(1), true));
        assert!(!state.notified(session(1)));
        assert!(!state.report_matches(Some(session(1))));
    }
    #[test]
    fn terminal_notification_retains_direct_parent_clone_exit_signal() {
        assert_eq!(ptrace_exit_notification_signal(Some(10), true), Some(10));
        assert_eq!(ptrace_exit_notification_signal(None, true), None);
        assert_eq!(ptrace_exit_notification_signal(Some(10), false), Some(17));
        assert_eq!(ptrace_exit_notification_signal(None, false), Some(17));
    }
    #[test]
    fn teardown_before_publication_leaves_final_parent_notification_enabled() {
        let mut state = PtraceExitState::Held(session(1));
        assert!(state.release(session(1), false));
        assert_eq!(state, PtraceExitState::Untraced);
    }
}
