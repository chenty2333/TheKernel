//! Allocation-free terminal status for a ptraced nonleader. The reverse node
//! retains only namespace/credential/accounting identity, never a live task/mm.
use super::{
    super::{TaskUsage, Thread},
    *,
};

#[derive(Clone)]
pub(crate) struct PtraceTaskExitReport {
    pub(crate) session: PtraceSession,
    pub(crate) wait_status: i32,
    pub(crate) credential: Arc<Cred>,
    pub(crate) usage: TaskUsage,
    pub(crate) pgid: Pid,
    pub(crate) process: Pid,
}

pub(crate) struct PtraceTaskExit {
    pub(crate) pid_ns: Arc<PidNamespace>,
    pub(crate) tid: Pid,
    state: SpinNoIrq<(Option<PtraceTaskExitReport>, bool)>,
    retained: AtomicBool,
}

impl PtraceTaskExit {
    pub(crate) fn new(pid_ns: Arc<PidNamespace>, tid: Pid) -> Self {
        Self {
            pid_ns,
            tid,
            state: SpinNoIrq::new((None, false)),
            retained: AtomicBool::new(false),
        }
    }
    pub(crate) fn report(&self, session: PtraceSession) -> Option<PtraceTaskExitReport> {
        let state = self.state.lock();
        state
            .1
            .then(|| {
                state
                    .0
                    .as_ref()
                    .filter(|report| report.session == session)
                    .cloned()
            })
            .flatten()
    }
    pub(crate) fn identity(&self, session: PtraceSession) -> Option<PtraceTaskExitReport> {
        self.state
            .lock()
            .0
            .as_ref()
            .filter(|report| report.session == session)
            .cloned()
    }
    pub(super) fn release_tid(&self) {
        if self.retained.swap(false, Ordering::AcqRel) {
            self.pid_ns.release_exited_thread(self.tid);
        }
    }
}
impl Drop for PtraceTaskExit {
    fn drop(&mut self) {
        self.release_tid();
    }
}

impl PtraceReverseLinks {
    fn task_exit(&self, link: PtraceReverseLink) -> Option<Arc<PtraceTaskExit>> {
        let mut node = self.head.as_deref();
        while let Some(item) = node {
            if item.tracee == link.tracee && item.session == link.session {
                return item.task_exit.clone();
            }
            node = item.next.as_deref();
        }
        None
    }

    fn retain_task_exit(&mut self, link: PtraceReverseLink, exit: Arc<PtraceTaskExit>) -> bool {
        let mut node = self.head.as_deref_mut();
        while let Some(item) = node {
            if item.tracee == link.tracee && item.session == link.session {
                exit.retained.store(true, Ordering::Release);
                item.task_exit = Some(exit);
                return true;
            }
            node = item.next.as_deref_mut();
        }
        false
    }

    fn remove_if_unretained(
        &mut self,
        link: PtraceReverseLink,
    ) -> (
        Option<Arc<PtraceTaskExit>>,
        Option<Box<PtraceReverseLinkNode>>,
    ) {
        let mut cursor = &mut self.head;
        loop {
            let matches = cursor
                .as_ref()
                .is_some_and(|node| node.tracee == link.tracee && node.session == link.session);
            if matches {
                if let Some(exit) = cursor.as_ref().and_then(|node| node.task_exit.clone()) {
                    return (Some(exit), None);
                }
                let mut removed = cursor.take().expect("matched reverse node disappeared");
                *cursor = removed.next.take();
                self.len -= 1;
                return (None, Some(removed));
            }
            let Some(node) = cursor.as_mut() else {
                return (None, None);
            };
            cursor = &mut node.next;
        }
    }
}

impl ProcessData {
    pub(crate) fn ptrace_task_exit(&self, link: PtraceReverseLink) -> Option<Arc<PtraceTaskExit>> {
        self.ptrace_tracees.lock().task_exit(link)
    }
    /// Removes a stale reverse link only if it does not own a terminal task
    /// report. Call with the exact tracee's ptrace publication guard held, or
    /// after the exact task is no longer in the task table. The terminal check
    /// and unlink share one reverse-list critical section with terminal
    /// retention, so a racing wait cannot erase a newly attached report. It
    /// returns the terminal owner if retained, and otherwise removes the link
    /// if present.
    /// This only recognizes task-scoped terminal reports; callers must retain
    /// the existing process-scoped group-leader exit fallback separately.
    pub(crate) fn remove_ptrace_tracee_if_unretained(
        &self,
        link: PtraceReverseLink,
    ) -> Option<Arc<PtraceTaskExit>> {
        let (terminal, removed) = {
            let mut links = self.ptrace_tracees.lock();
            links.remove_if_unretained(link)
        };
        drop(removed);
        terminal
    }
    fn retain_task_exit(&self, link: PtraceReverseLink, exit: Arc<PtraceTaskExit>) -> bool {
        self.ptrace_tracees.lock().retain_task_exit(link, exit)
    }
    pub(crate) fn claim_ptrace_task_exit(
        &self,
        link: PtraceReverseLink,
        exit: &Arc<PtraceTaskExit>,
    ) -> bool {
        if exit.report(link.session()).is_none() {
            return false;
        }
        if !self.remove_ptrace_tracee(link) {
            return false;
        }
        exit.release_tid();
        true
    }
}

/// Called with the exact task action gate held, after membership retirement.
/// Return true only if the reverse node took custody of the namespace binding.
pub(crate) fn retain_ptrace_task_exit(thread: &Thread, wait_status: i32) -> bool {
    let Some(session) = thread.ptrace_active_session() else {
        return false;
    };
    let Ok(tracer) = crate::task::get_process_data(session.tracer) else {
        return false;
    };
    let exit = &thread.ptrace_terminal;
    *exit.state.lock() = (
        Some(PtraceTaskExitReport {
            session,
            wait_status,
            credential: thread.current_cred(),
            usage: thread.usage_snapshot(),
            pgid: thread.proc_data.proc.group().pgid(),
            process: thread.proc_data.proc.pid(),
        }),
        false,
    );
    tracer.retain_task_exit(
        PtraceReverseLink::new(thread.kernel_tid(), session),
        exit.clone(),
    )
}

/// Publish only after teardown and exact exit flag; a waiter cannot retire TID
/// identity while the still-running exit path uses it to clear task resources.
pub(crate) fn publish_ptrace_task_exit(thread: &Thread) {
    // do_exit ended its usage-transition epoch before reaching this edge.
    // Sampling group RUSAGE_BOTH earlier would spin on our own writer epoch.
    let usage = thread.proc_data.total_usage();
    let report = {
        let mut state = thread.ptrace_terminal.state.lock();
        if let Some(report) = state.0.as_mut() { report.usage = usage; }
        state.0.clone()
    };
    let Some(report) = report else {
        return;
    };
    if let Ok(tracer) = crate::task::get_process_data(report.session.tracer) {
        let pid = tracer
            .pid_ns()
            .visible_pid_for(&thread.ptrace_terminal.pid_ns, thread.ptrace_terminal.tid)
            .unwrap_or(0);
        let uid = tracer
            .user_ns()
            .from_kuid_munged(report.credential.ids().ruid);
        let (code, status) = if report.wait_status & 0x7f == 0 {
            (
                linux_raw_sys::general::CLD_EXITED,
                (report.wait_status >> 8) & 0xff,
            )
        } else if report.wait_status & 0x80 != 0 {
            (linux_raw_sys::general::CLD_DUMPED, report.wait_status & 0x7f)
        } else {
            (
                linux_raw_sys::general::CLD_KILLED,
                report.wait_status & 0x7f,
            )
        };
        let _ = crate::task::send_signal_to_process(
            report.session.tracer,
            Some(SignalInfo::new_child(
                Signo::SIGCHLD,
                code as i32,
                pid,
                uid,
                status,
            )),
        );
        thread.ptrace_terminal.state.lock().1 = true;
        tracer.child_exit_event.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(generation: u64) -> PtraceSession {
        PtraceSession { tracer: 7, tracer_kernel_tid: 70, generation }
    }
    #[test]
    fn task_terminal_identity_is_not_reportable_before_notification() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let ns = PidNamespace::try_new_root(user_ns.clone()).unwrap();
        let exit = PtraceTaskExit::new(ns, 11);
        assert!(exit.identity(session(1)).is_none());
        *exit.state.lock() = (Some(PtraceTaskExitReport {
            session: session(1), wait_status: 23 << 8,
            credential: Cred::try_root(user_ns).unwrap(),
            usage: TaskUsage::default(), pgid: 10, process: 10,
        }), false);
        assert!(exit.identity(session(1)).is_some());
        assert!(exit.report(session(1)).is_none());
        exit.state.lock().1 = true;
        assert_eq!(exit.report(session(1)).unwrap().wait_status, 23 << 8);
        assert!(exit.report(session(2)).is_none());
    }

    #[test]
    fn retained_nonleader_exit_stays_discoverable_until_report_ready() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let pid_ns = PidNamespace::try_new_root(user_ns.clone()).unwrap();
        pid_ns.reserve_process(10).unwrap().commit();
        pid_ns.reserve_process(11).unwrap().commit();

        let session = session(1);
        let link = PtraceReverseLink {
            tracee: 11,
            process: 10,
            session,
        };
        let exit = Arc::new(PtraceTaskExit::new(pid_ns.clone(), link.tracee));
        *exit.state.lock() = (
            Some(PtraceTaskExitReport {
                session,
                wait_status: 23 << 8,
                credential: Cred::try_root(user_ns).unwrap(),
                usage: TaskUsage::default(),
                pgid: 10,
                process: 10,
            }),
            false,
        );

        let mut links = PtraceReverseLinks {
            head: Some(Box::new(PtraceReverseLinkNode {
                tracee: link.tracee,
                process: link.process,
                session,
                retired_relationship: None,
                task_exit: None,
                next: None,
            })),
            len: 1,
            reservations: 0,
            closed: false,
        };

        // The first reverse-link snapshot can observe this node before exit
        // retention publishes its terminal owner.
        assert!(links.task_exit(link).is_none());
        // This is the early terminal-retention edge in do_exit: after core
        // task unlink a tracer wait can no longer resolve a live task, so the
        // reverse link itself must now identify the pending nonleader status.
        assert!(links.retain_task_exit(link, exit.clone()));
        let retained = links
            .task_exit(link)
            .expect("terminal reverse node vanished");
        assert!(Arc::ptr_eq(&retained, &exit));
        assert!(exit.retained.load(Ordering::Acquire));
        assert!(retained.identity(session).is_some());
        // matching_wait_candidates takes this branch before its stale-live-task
        // fallback. Identity admits a blocking candidate, while report() keeps
        // select_wait_event from claiming it before set_exit publishes readiness.
        assert!(retained.report(session).is_none());
        assert!(pid_ns.visible_pid_checked(link.tracee).is_some());

        // A stale-candidate cleanup may have observed no terminal while
        // building its first snapshot. Its final inspect-or-remove must
        // preserve the terminal node installed in the meantime and return the
        // same report owner so this wait iteration can add a TaskExited
        // candidate instead of concluding ECHILD from an empty candidate set.
        let (raced_terminal, removed) = links.remove_if_unretained(link);
        assert!(removed.is_none());
        let raced_terminal = raced_terminal.expect("stale cleanup erased terminal node");
        assert!(Arc::ptr_eq(&raced_terminal, &exit));
        assert!(Arc::ptr_eq(
            &links.task_exit(link).expect("raced terminal node vanished"),
            &exit
        ));

        exit.state.lock().1 = true;
        assert_eq!(retained.report(session).unwrap().wait_status, 23 << 8);
        assert!(Arc::ptr_eq(
            &links.task_exit(link).expect("ready terminal node vanished"),
            &exit
        ));
    }

    #[test]
    fn reverse_node_teardown_releases_tid_even_with_a_retained_snapshot() {
        let ns = PidNamespace::try_new_root(UserNamespace::try_new_root().unwrap()).unwrap();
        ns.reserve_process(10).unwrap().commit();
        ns.reserve_process(11).unwrap().commit();
        let exit = Arc::new(PtraceTaskExit::new(ns.clone(), 11));
        exit.retained.store(true, Ordering::Release);
        let node = Box::new(PtraceReverseLinkNode {
            tracee: 11, process: 10, session: session(1),
            retired_relationship: None, task_exit: Some(exit.clone()), next: None,
        });
        assert!(ns.visible_pid_checked(11).is_some());
        drop(node);
        assert!(ns.visible_pid_checked(11).is_none());
        // A stale report/drop must not erase a replacement numeric binding.
        ns.reserve_process(11).unwrap().commit();
        exit.release_tid();
        drop(exit);
        assert!(ns.visible_pid_checked(11).is_some());
    }
}
