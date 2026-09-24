use alloc::{sync::Arc, vec, vec::Vec};
use core::mem::{offset_of, size_of};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use bitflags::bitflags;
use linux_raw_sys::general::{
    __WALL, __WCLONE, __WNOTHREAD, CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED,
    CLD_TRAPPED, P_ALL, P_PGID, P_PID, P_PIDFD, SIGCHLD, SIGCONT, WCONTINUED, WEXITED, WNOHANG,
    WNOWAIT, WUNTRACED, rusage, siginfo,
};
use tk_linux_process_adapter::{Pid, ProcessError};

use crate::{
    file::{FileHandle, FileLike, PidFd},
    mm::{UserMemoryCapability, map_usercopy_error},
    pseudofs::cgroup,
    readiness::block_on_poll_set_interruptible_if,
    task::{
        AsThread, PidNamespace, Process, ProcessData, PtraceSession, StopFilter, StopReport,
        TaskParentNode, TaskUsage, Thread, ZombieSnapshot, get_process_data,
        has_pending_syscall_signal, is_exact_child_of_thread, process_domain, reap_process,
    },
};

// The accepted option masks live in `tk_linux_process::wait`, where they are
// host-tested against `kernel_wait4()` and `kernel_waitid_prepare()`; the bits
// themselves stay here as `WaitOptions`.
const POST_WAIT_RECLAIM_YIELDS: usize = 4;

bitflags! {
    #[derive(Debug)]
    pub(crate) struct WaitOptions: u32 {
        /// Do not block when there are no processes wishing to report status.
        const WNOHANG = WNOHANG;
        /// Report the status of selected processes which are stopped due to a
        /// `SIGTTIN`, `SIGTTOU`, `SIGTSTP`, or `SIGSTOP` signal.
        const WUNTRACED = WUNTRACED;
        /// Report the status of selected processes which have terminated.
        const WEXITED = WEXITED;
        /// Report the status of selected processes that have continued from a
        /// job control stop by receiving a `SIGCONT` signal.
        const WCONTINUED = WCONTINUED;
        /// Don't reap, just poll status.
        const WNOWAIT = WNOWAIT;

        /// Don't wait on children of other threads in this group.
        const WNOTHREAD = __WNOTHREAD;
        /// Wait on all children, regardless of type.
        const WALL = __WALL;
        /// Wait for "clone" children only.
        const WCLONE = __WCLONE;
    }
}

#[derive(Debug, Clone, Copy)]
enum WaitPid {
    /// Wait for any child process.
    Any,
    /// Wait for the child whose process ID is equal to the value.
    Pid(Pid),
    /// Wait for a child in this kernel-wide process group. A current group
    /// can be inherited from an outer PID namespace and have no visible ID.
    Pgid(Pid),
}

impl WaitPid {
    const fn matches(&self, child_pid: Pid, child_pgid: Pid) -> bool {
        match self {
            WaitPid::Any => true,
            WaitPid::Pid(pid) => child_pid == *pid,
            WaitPid::Pgid(pgid) => child_pgid == *pgid,
        }
    }
}

/// Renders a core process ID in the caller's PID namespace.  The process
/// registry deliberately uses kernel-wide IDs, but wait(2) arguments and
/// results are namespace-relative.
pub(crate) fn visible_process_pid(viewer_pid_ns: &PidNamespace, process: &Process) -> Option<Pid> {
    let target_pid_ns = crate::task::process_identity_pid_ns(process)?;
    viewer_pid_ns.visible_pid_for(target_pid_ns, process.pid())
}

fn wait_pid_applies(viewer_pid_ns: &PidNamespace, pid: WaitPid, child: &Process) -> Option<Pid> {
    let child_pid = visible_process_pid(viewer_pid_ns, child)?;
    pid.matches(child_pid, child.group().pgid())
        .then_some(child_pid)
}

#[derive(Clone)]
struct WaitCandidate {
    process: Arc<Process>,
    /// Snapshot before reaping: the namespace PID binding is released by reap.
    visible_pid: Pid,
    allow_exit: bool,
    expected_ptrace_session: Option<PtraceSession>,
}

#[derive(Clone)]
enum WaitEvent {
    Stopped {
        pid: Pid,
        stop: StopReport,
        proc_data: Arc<ProcessData>,
    },
    Continued {
        pid: Pid,
        proc_data: Arc<ProcessData>,
    },
    Exited {
        pid: Pid,
        child: Arc<Process>,
        snapshot: Arc<ZombieSnapshot>,
    },
}

impl WaitEvent {
    fn pid(&self) -> Pid {
        match self {
            WaitEvent::Stopped { pid, .. } | WaitEvent::Continued { pid, .. } => *pid,
            WaitEvent::Exited { pid, .. } => *pid,
        }
    }

    fn waitpid_status(&self) -> i32 {
        match self {
            WaitEvent::Stopped { stop, .. } => {
                (((stop.signal as i32) | ((stop.ptrace_event as i32) << 8)) << 8) | 0x7f
            }
            WaitEvent::Continued { .. } => 0xffff,
            WaitEvent::Exited { snapshot, .. } => snapshot.wait_status,
        }
    }

    fn waitid_siginfo_fields(
        &self,
        viewer_user_ns: &crate::task::UserNamespace,
    ) -> WaitIdSiginfoFields {
        match self {
            WaitEvent::Stopped {
                pid,
                stop,
                proc_data,
            } => {
                let uid = viewer_user_ns.from_kuid_munged(proc_data.group_leader_cred().ids().ruid);
                WaitIdSiginfoFields {
                    signo: SIGCHLD as i32,
                    code: if stop.traced() {
                        CLD_TRAPPED as i32
                    } else {
                        CLD_STOPPED as i32
                    },
                    pid: *pid as i32,
                    uid,
                    status: stop.signal as i32,
                }
            }
            WaitEvent::Continued { pid, proc_data } => {
                let uid = viewer_user_ns.from_kuid_munged(proc_data.group_leader_cred().ids().ruid);
                WaitIdSiginfoFields {
                    signo: SIGCHLD as i32,
                    code: CLD_CONTINUED as i32,
                    pid: *pid as i32,
                    uid,
                    status: SIGCONT as i32,
                }
            }
            WaitEvent::Exited { pid, snapshot, .. } => {
                let (si_code, si_status) = decode_exit_code(snapshot.wait_status);
                let uid = viewer_user_ns.from_kuid_munged(snapshot.credential.ids().ruid);
                WaitIdSiginfoFields {
                    signo: SIGCHLD as i32,
                    code: si_code as i32,
                    pid: *pid as i32,
                    uid,
                    status: si_status,
                }
            }
        }
    }

    fn usage(&self) -> TaskUsage {
        match self {
            WaitEvent::Exited { snapshot, .. } => snapshot.total_usage().into(),
            WaitEvent::Stopped { proc_data, .. } | WaitEvent::Continued { proc_data, .. } => {
                proc_data.total_usage()
            }
        }
    }
}

/// `kernel_wait4()`'s option check: unknown bits are `EINVAL`, and there is no
/// requirement that any event bit be set because `kernel_wait4()` ORs in
/// `WEXITED` itself.
fn validate_waitpid_options(options: u32) -> AxResult<WaitOptions> {
    if options & !tk_linux_process::WAIT4_OPTIONS_ALLOWED != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(WaitOptions::from_bits_truncate(options) | WaitOptions::WEXITED)
}

/// `kernel_waitid_prepare()`'s option-mask check.
///
/// The separate "at least one of `WEXITED|WSTOPPED|WCONTINUED`" requirement is
/// enforced by `sys_waitid()` itself, in Linux's order, so that both halves of
/// the check stay visible next to the call they guard.
fn validate_waitid_options(options: u32) -> AxResult<WaitOptions> {
    if options & !tk_linux_process::WAITID_OPTIONS_ALLOWED != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(WaitOptions::from_bits_truncate(options))
}

/// Determines whether a child should be included in wait based on WALL/WCLONE flags.
pub(crate) fn should_wait_for_child(child: &Process, options: &WaitOptions) -> bool {
    if options.contains(WaitOptions::WALL) {
        return true;
    }

    let is_clone = child.exit_signal() != Some(tk_linux_signal::Signo::SIGCHLD as u8);
    if options.contains(WaitOptions::WCLONE) {
        is_clone
    } else {
        !is_clone
    }
}

/// Linux's `__WNOTHREAD` rule, which restricts a wait to the exact calling
/// task's own relation instead of the whole thread group's.
///
/// `__do_wait()` walks `current->children` and `current->ptraced` and breaks
/// out of the thread-group loop as soon as the flag is set
/// (`kernel/exit.c:1725-1735`), while `do_wait_pid()` admits a named target only
/// through `is_effectively_child()`: `current == target->real_parent` for the
/// thread-group lookup and `current == target->parent` for the ptrace one
/// (`kernel/exit.c:1657-1664`, `:1672-1697`). Both are exact *task* identities,
/// so a sibling thread of the thread that forked never qualifies.
struct WaitThreadScope {
    caller_kernel_tid: Pid,
    caller_node: Arc<TaskParentNode>,
}

impl WaitThreadScope {
    fn new(caller: &Thread) -> Self {
        Self {
            caller_kernel_tid: caller.kernel_tid(),
            caller_node: caller.task_parent_node().clone(),
        }
    }

    /// `current == p->real_parent`.
    fn owns_child(&self, child: &Process) -> bool {
        is_exact_child_of_thread(child, &self.caller_node)
    }

    /// `current == p->parent` for a `p->ptrace` tracee, which Linux reaches
    /// through the tracer thread that `ptrace_link()` recorded.
    fn owns_tracee(&self, session: &PtraceSession) -> bool {
        session.tracer_kernel_tid == self.caller_kernel_tid
    }
}

fn process_error(error: ProcessError) -> AxError {
    match error {
        ProcessError::NoMemory | ProcessError::Capacity => AxError::NoMemory,
        ProcessError::AlreadyExists => AxError::AlreadyExists,
        ProcessError::NotPublished | ProcessError::NotLive => AxError::NoSuchProcess,
        ProcessError::WrongDomain | ProcessError::NotInitialized => AxError::BadState,
        _ => AxError::BadState,
    }
}

fn matching_wait_candidates(
    proc_data: &ProcessData,
    viewer_pid_ns: &PidNamespace,
    pid: WaitPid,
    options: &WaitOptions,
    caller: &Thread,
) -> AxResult<Vec<WaitCandidate>> {
    let proc = &proc_data.proc;
    let nothread = options.contains(WaitOptions::WNOTHREAD);
    let thread_scope = nothread.then(|| WaitThreadScope::new(caller));
    let children = proc
        .try_children(process_domain()?.registry())
        .map_err(process_error)?;
    let tracees = proc_data.try_ptrace_tracees()?;
    let capacity = children
        .len()
        .checked_add(tracees.len())
        .ok_or(AxError::NoMemory)?;
    let mut candidates = Vec::new();
    candidates
        .try_reserve_exact(capacity)
        .map_err(|_| AxError::NoMemory)?;
    // Diagnostic counters: how many children the registry returned, how many
    // the pid filter accepted, and how many survived the clone-type filter.
    // Published for the trace record that this syscall is about to write.
    let seen = children.len() as u32;
    let mut pid_ok = 0u32;
    let mut ok = 0u32;
    let mut invisible = 0u32;
    for process in children {
        // `__WNOTHREAD` narrows the thread-group walk to the caller itself, so
        // a child of a sibling thread is not a candidate at all.
        if thread_scope
            .as_ref()
            .is_some_and(|scope| !scope.owns_child(&process))
        {
            invisible += 1;
            continue;
        }
        let Some(visible_pid) = wait_pid_applies(viewer_pid_ns, pid, &process) else {
            invisible += 1;
            continue;
        };
        pid_ok += 1;
        if should_wait_for_child(&process, options) {
            ok += 1;
            candidates.push(WaitCandidate {
                process,
                visible_pid,
                allow_exit: true,
                expected_ptrace_session: None,
            });
        }
    }
    crate::task::exit_status_note_candidates(seen, pid_ok, ok, tracees.len() as u32, invisible);

    for reverse_link in tracees {
        // `ptrace_do_wait()` walks one task's `ptraced` list, so
        // `__WNOTHREAD` admits only tracees this exact thread attached.
        if thread_scope
            .as_ref()
            .is_some_and(|scope| !scope.owns_tracee(&reverse_link.session()))
        {
            continue;
        }
        let tracee_pid = reverse_link.tracee();
        let Ok(tracee_data) = get_process_data(tracee_pid) else {
            proc_data.remove_ptrace_tracee(reverse_link);
            continue;
        };
        if tracee_data.ptrace_session_if_traced_by_process(proc.pid())
            != Some(reverse_link.session())
        {
            proc_data.remove_ptrace_tracee(reverse_link);
            continue;
        }
        let tracee = &tracee_data.proc;
        let Some(visible_pid) = wait_pid_applies(viewer_pid_ns, pid, tracee) else {
            continue;
        };
        if let Some(candidate) = candidates
            .iter_mut()
            .find(|candidate| candidate.process.pid() == tracee.pid())
        {
            candidate.expected_ptrace_session = Some(reverse_link.session());
            continue;
        }
        candidates.push(WaitCandidate {
            process: tracee.clone(),
            visible_pid,
            allow_exit: false,
            expected_ptrace_session: Some(reverse_link.session()),
        });
    }

    if candidates.is_empty() {
        Err(AxError::from(LinuxError::ECHILD))
    } else {
        Ok(candidates)
    }
}

fn waitid_pidfd(fd: i32) -> AxResult<FileHandle<PidFd>> {
    PidFd::from_fd(fd).map_err(|err| {
        if err == AxError::InvalidInput {
            AxError::BadFileDescriptor
        } else {
            err
        }
    })
}

fn pidfd_wait_candidate(
    proc: &Process,
    viewer_pid_ns: &PidNamespace,
    pidfd: &PidFd,
    options: &WaitOptions,
    caller: &Thread,
) -> AxResult<WaitCandidate> {
    let target = pidfd.process()?;
    let visible_pid =
        visible_process_pid(viewer_pid_ns, &target).ok_or(AxError::from(LinuxError::ECHILD))?;

    let expected_ptrace_session = get_process_data(target.pid())
        .ok()
        .and_then(|target| target.ptrace_session_if_traced_by_process(proc.pid()));
    let ptrace = expected_ptrace_session.is_some();

    // `P_PIDFD` takes `do_wait_pid()`'s PIDTYPE_TGID branch, which admits the
    // target when it is the caller's `real_parent` child, and then the
    // PIDTYPE_PID branch, which admits a `p->ptrace` target under
    // `target->parent` instead. `ptrace_link()` reparents the tracee onto the
    // tracer, so a *non-parent* ptracer holding a pidfd for a tracee it attached
    // to must still be able to wait on it; requiring `real_parent` alone would
    // report ECHILD and strand the tracee.
    // `__WNOTHREAD` replaces both `same_thread_group()` arms of
    // `is_effectively_child()` with exact task identity: the calling task must
    // be the target's `real_parent`, or the tracer recorded by `ptrace_link()`
    // for the `p->ptrace` arm.
    let is_effectively_child = if options.contains(WaitOptions::WNOTHREAD) {
        let scope = WaitThreadScope::new(caller);
        scope.owns_child(&target)
            || expected_ptrace_session
                .as_ref()
                .is_some_and(|session| scope.owns_tracee(session))
    } else {
        ptrace
            || target
                .parent()
                .is_some_and(|parent| parent.pid() == proc.pid())
    };
    // `eligible_child()` returns 1 for a ptrace target regardless of
    // `__WCLONE`/`__WALL`, and otherwise filters on the exit signal.
    let eligible = ptrace || should_wait_for_child(&target, options);
    if !is_effectively_child || !eligible {
        return Err(AxError::from(LinuxError::ECHILD));
    }

    Ok(WaitCandidate {
        process: target,
        visible_pid,
        allow_exit: true,
        expected_ptrace_session,
    })
}

/// Linux's `wait_consider_task()` event selection, applied per child.
///
/// The three events are not ranked globally: `wait_consider_task()` is called
/// once per child and returns as soon as that child has anything to report, so
/// the *first* child in the child list with any selected event wins. Collecting
/// all stopped children first would report a later stopped sibling ahead of an
/// earlier exited one, which is observable whenever two children have different
/// pending events.
///
/// `allow_exit` and `expected_ptrace_session` are carried by `WaitCandidate`,
/// and the stop gate below reproduces `wait_task_stopped()`'s rule that a
/// non-ptrace child's stop is only reportable with `WUNTRACED`.
/// `waiter_group` is the waiting thread group's identity, which
/// `wait_consider_task()` compares a tracee's tracer against before it lets a
/// plain child's waiter see a ptrace stop (`StopFilter`).
fn select_wait_event(
    candidates: &[WaitCandidate],
    options: &WaitOptions,
    wait_exited: bool,
    waiter_group: Pid,
) -> Option<WaitEvent> {
    let selection = selection_for(options, wait_exited);
    for candidate in candidates {
        // The exit path must not require process data. A process that has
        // exited but not yet been reaped is no longer registered, so
        // `get_process_data()` fails for exactly the candidates this loop is
        // most often called to report; skipping them strands the zombie and
        // hangs the parent in `wait4(2)` forever. Only the stop and continued
        // events need the registry entry, and a missing entry means neither is
        // reportable.
        let proc_data = get_process_data(candidate.process.pid()).ok();

        // A stop is reportable when it belongs to this waiter's ptrace session
        // (if any) and either it is a ptrace stop — which is always reported —
        // or the caller asked for job-control stops with `WUNTRACED`. This is
        // `wait_task_stopped()`'s `if (!ptrace && !(wo->wo_flags & WUNTRACED))
        // return 0;`, with `ptrace` derived from the same session identity.
        let filter = stop_filter_for(candidate.expected_ptrace_session, waiter_group);
        let stop = proc_data
            .as_ref()
            .and_then(|proc_data| proc_data.peek_stop_status(filter))
            .filter(|stop| stop.traced() || selection.stopped);

        // `delay_group_leader()`: a zombie group leader is held back while any
        // of its threads is still alive, so its exit is reported only once the
        // whole thread group is gone.
        let zombie = candidate
            .process
            .is_zombie()
            .then(|| candidate.process.zombie_payload())
            .flatten()
            .filter(|_| {
                !tk_linux_process::zombie_is_delayed(true, candidate.process.thread_count())
            });

        let event = tk_linux_process::select_child_event(
            tk_linux_process::WaitEventState {
                exited: candidate.allow_exit && zombie.is_some(),
                // `stop` is already the per-child gate above, so the selection
                // must not apply the `WUNTRACED` rule a second time: a ptrace
                // stop is reported to a bare `wait4(2)` because
                // `wait_consider_task()` forces `ptrace = 1` for it.
                stopped: stop.is_some(),
                ptrace: stop.is_some_and(StopReport::traced),
                continued: proc_data
                    .as_ref()
                    .is_some_and(|proc_data| proc_data.peek_continued())
                    && selection.continued,
            },
            selection,
        );

        match event {
            Some(tk_linux_process::WaitEventKind::Exited) => {
                return Some(WaitEvent::Exited {
                    pid: candidate.visible_pid,
                    child: candidate.process.clone(),
                    snapshot: zombie?,
                });
            }
            Some(tk_linux_process::WaitEventKind::Stopped) => {
                return Some(WaitEvent::Stopped {
                    pid: candidate.visible_pid,
                    stop: stop?,
                    proc_data: proc_data?,
                });
            }
            Some(tk_linux_process::WaitEventKind::Continued) => {
                return Some(WaitEvent::Continued {
                    pid: candidate.visible_pid,
                    proc_data: proc_data?,
                });
            }
            None => {}
        }
    }

    None
}

/// Maps the kernel's option bits onto the crate's event selection.
fn selection_for(options: &WaitOptions, wait_exited: bool) -> tk_linux_process::WaitEventSelection {
    tk_linux_process::WaitEventSelection {
        exited: wait_exited || options.contains(WaitOptions::WEXITED),
        // `wait4(2)` spells `WSTOPPED` as `WUNTRACED`; they are the same bit,
        // so one test covers both spellings.
        stopped: options.contains(WaitOptions::WUNTRACED),
        continued: options.contains(WaitOptions::WCONTINUED),
    }
}

/// The stop visibility a candidate's wait request implies.
///
/// A candidate that carries a session is a tracee this waiter explicitly waits
/// for, so only that exact session's stops are its to see. A candidate that
/// carries none is an ordinary child, and `wait_consider_task()` still forces
/// `ptrace = 1` for it when the tracee is traced from the waiter's own thread
/// group: `if (!ptrace_reparented(p)) ptrace = 1;` (`kernel/exit.c:1527-1528`).
/// That is the `PTRACE_TRACEME` case, where the tracer *is* the real parent, so
/// its `wait4(2)` must report the stop even though the request named no
/// session. A stop owned by any other thread group stays hidden, which is what
/// keeps a separate ptracer's stop from being reported to the real parent as
/// well, once as a job-control stop and once as a ptrace stop.
fn stop_filter_for(
    expected_ptrace_session: Option<PtraceSession>,
    waiter_group: Pid,
) -> StopFilter {
    match expected_ptrace_session {
        Some(session) => StopFilter::Session(session),
        None => StopFilter::Natural {
            group: waiter_group,
        },
    }
}

fn write_waitpid_event(
    memory: &UserMemoryCapability,
    event: &WaitEvent,
    exit_code: *mut i32,
    rusage_ptr: *mut rusage,
) -> AxResult<()> {
    // `wait_consider_task()` copies the rusage out before it returns, so the
    // `put_user(wo_stat, stat_addr)` in `kernel_wait4()` is the later write.
    if !rusage_ptr.is_null() {
        let usage = event.usage();
        memory
            .write_abi_value(rusage_ptr, usage.into())
            .map_err(map_usercopy_error)?;
    }
    if !exit_code.is_null() {
        memory
            .write_value(exit_code, event.waitpid_status())
            .map_err(map_usercopy_error)?;
    }
    Ok(())
}

/// `if (err > 0) { ...; if (ru && copy_to_user(ru, &r, ...)) return -EFAULT; }`.
///
/// The resource usage is the first thing `SYSCALL_DEFINE5(waitid)` copies, and
/// a fault there returns before the `siginfo_t` is touched at all.
fn write_waitid_rusage(
    memory: &UserMemoryCapability,
    rusage_ptr: *mut rusage,
    event: &WaitEvent,
) -> AxResult<()> {
    if rusage_ptr.is_null() {
        return Ok(());
    }
    let usage = event.usage();
    memory
        .write_abi_value(rusage_ptr, usage.into())
        .map_err(map_usercopy_error)?;
    Ok(())
}

/// Claim the event before any usercopy. As in Linux, EFAULT does not undo a
/// consumed wait event; WNOWAIT alone leaves it available to another waiter.
fn claim_wait_event(event: &WaitEvent, parent: &ProcessData, nowait: bool) -> AxResult<bool> {
    if nowait {
        return Ok(true);
    }
    match event {
        WaitEvent::Stopped {
            stop, proc_data, ..
        } => {
            // `claim_stop_status()` matches the report itself: a stop that a
            // later ptrace relationship replaced, or that another thread of the
            // group already reaped, must not be consumed by this waiter.
            if proc_data.claim_stop_status(*stop).is_none() {
                return Ok(false);
            }
        }
        WaitEvent::Continued { proc_data, .. } => {
            if !proc_data.claim_continued() {
                return Ok(false);
            }
        }
        WaitEvent::Exited {
            child, snapshot, ..
        } => {
            if !reap_child(child)? {
                return Ok(false);
            }
            cgroup::detach_process(child);
            parent.account_waited_child(snapshot.total_usage().into());
        }
    }
    Ok(true)
}

fn reap_child(child: &Process) -> AxResult<bool> {
    reap_process(child)
}

pub fn sys_waitpid(
    memory: UserMemoryCapability,
    pid: i32,
    exit_code: *mut i32,
    options: u32,
    rusage_ptr: *mut rusage,
) -> AxResult<isize> {
    let options = validate_waitpid_options(options)?;

    if pid == i32::MIN {
        return Err(AxError::from(LinuxError::ESRCH));
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let proc = &proc_data.proc;
    let viewer_pid_ns = curr.as_thread().pid_ns();

    let pid = if pid == -1 {
        WaitPid::Any
    } else if pid == 0 {
        WaitPid::Pgid(proc.group().pgid())
    } else if pid > 0 {
        WaitPid::Pid(pid as _)
    } else {
        WaitPid::Pgid(viewer_pid_ns.resolve_visible_pid(-pid as _).unwrap_or(0))
    };
    let check_children = || {
        let _wait_guard = proc_data.wait_lock.lock();
        loop {
            let candidates = match matching_wait_candidates(
                proc_data,
                &viewer_pid_ns,
                pid,
                &options,
                curr.as_thread(),
            ) {
                Ok(candidates) => candidates,
                Err(err) => {
                    // Error codes are recorded symbolically: `errno()` is not
                    // reachable from here, and the only distinction the report
                    // needs is which `wait4` failure the caller saw.
                    let code = if err == AxError::from(LinuxError::ECHILD) {
                        1
                    } else if err == AxError::from(LinuxError::ESRCH) {
                        2
                    } else {
                        3
                    };
                    trace_wait(pid, -1, 0, &options, 5, code, 0);
                    return Err(err);
                }
            };

            let Some(event) = select_wait_event(&candidates, &options, true, proc.pid()) else {
                trace_wait(pid, -1, 0, &options, 4, 0, 0);
                return if options.contains(WaitOptions::WNOHANG) {
                    Ok(Some(0))
                } else {
                    Ok(None)
                };
            };

            if !claim_wait_event(&event, proc_data, false)? {
                // A concurrent waiter consumed the event first. Linux's
                // do_wait rescans its child list in this case, so rebuild the
                // candidate set rather than blocking until the next child
                // event while a sibling may already have one pending.
                continue;
            }

            // Diagnostic: record what this pid-targeted wait is returning.
            // `event.pid()` is the candidate's namespace-visible pid, which may
            // differ from the requested pid when the wait matched something
            // else.
            let (event_kind, status) = match &event {
                WaitEvent::Exited { snapshot, .. } => (1u32, snapshot.wait_status),
                WaitEvent::Stopped { stop, .. } => (2, i32::from(stop.signal)),
                WaitEvent::Continued { .. } => (3, 0),
            };
            trace_wait(
                pid,
                i64::from(event.pid()),
                status,
                &options,
                event_kind,
                0,
                0,
            );
            // The immutable snapshot and consumed-event claim now suffice;
            // a userfaultfd stall must not hold the parent's wait mutex.
            drop(_wait_guard);
            write_waitpid_event(&memory, &event, exit_code, rusage_ptr)?;

            return Ok(Some(event.pid() as isize));
        }
    };

    let result = block_on_poll_set_interruptible_if(
        &proc_data.child_exit_event,
        || {
            if let Some(result) = check_children()? {
                Ok(result)
            } else if has_pending_syscall_signal(curr.as_thread()) {
                Err(AxError::Interrupted)
            } else {
                Err(AxError::WouldBlock)
            }
        },
        || has_pending_syscall_signal(curr.as_thread()),
    );
    // `result` is the blocking phase's outcome: an interrupt that did not
    // become a completed event, or a completed event.
    let (result, result_symbol) = match &result {
        Ok(_) => (result, 0u8),
        Err(error) if *error == AxError::Interrupted => (result, 1),
        Err(error) if *error == AxError::WouldBlock => (result, 2),
        Err(_) => (result, 3),
    };
    if result_symbol != 0 {
        trace_wait(pid, -1, 0, &options, 6, 0, result_symbol);
    }
    let result = result?;
    axtask::reclaim_exited_tasks_until_clear(POST_WAIT_RECLAIM_YIELDS);
    Ok(result)
}

/// Diagnostic: records one pid-targeted `wait4` observation.
///
/// Only positive, namespace-visible pid requests are recorded, because that is
/// the shape `tests/guest/portable/exit-status.c` and the acceptance loop use.
fn trace_wait(
    pid: WaitPid,
    got: i64,
    status: i32,
    options: &WaitOptions,
    event: u32,
    error: u8,
    result: u8,
) {
    let WaitPid::Pid(requested) = pid else {
        return;
    };
    crate::task::exit_status_trace_record(
        requested,
        got.max(-1) as Pid,
        status,
        options.bits(),
        event,
        error,
        result,
    );
}

/// Decodes a Linux-style wait status into (CLD_* code, si_status) for waitid.
fn decode_exit_code(exit_code: i32) -> (u32, i32) {
    if exit_code & 0x7f == 0 {
        (CLD_EXITED, (exit_code >> 8) & 0xff)
    } else if exit_code & 0x80 != 0 {
        (CLD_DUMPED, exit_code & 0x7f)
    } else {
        (CLD_KILLED, exit_code & 0x7f)
    }
}

/// The `siginfo_t` fields Linux's `waitid(2)` wrapper stores.
///
/// `do_wait()` fills a `struct waitid_info` that `{.status = 0}` initializes,
/// and `SYSCALL_DEFINE5(waitid)` then stores `si_signo`, `si_errno`, `si_code`,
/// `si_pid`, `si_uid` and `si_status` from it. `signo` is `SIGCHLD` only when
/// `kernel_waitid()` reported an event, so the zero value of this structure is
/// exactly what a failing `waitid` leaves in those six fields.
#[derive(Clone, Copy)]
struct WaitIdSiginfoFields {
    signo: i32,
    code: i32,
    pid: i32,
    uid: u32,
    status: i32,
}

impl WaitIdSiginfoFields {
    /// `int signo = 0` plus the zero-initialized `struct waitid_info`.
    const ZERO: Self = Self {
        signo: 0,
        code: 0,
        pid: 0,
        uid: 0,
        status: 0,
    };
}

/// Offset of the `_sifields` union inside `siginfo_t`.
///
/// Linux writes `si_pid`, `si_uid` and `si_status` at the head of the
/// `_sigchld` member, which every union member shares, so the three fields sit
/// at this offset and the two after it. Deriving the offset from the same type
/// `_sifields` is declared in keeps the write independent of the 4 bytes of
/// ABI padding that follow `si_code`.
const SIGINFO_SIFIELDS_OFFSET: usize =
    offset_of!(siginfo, __bindgen_anon_1.__bindgen_anon_1._sifields);

/// Stores the six `siginfo_t` fields `waitid(2)` writes.
///
/// Linux pins the whole `sizeof(struct siginfo)` extent with
/// `user_write_access_begin(infop, sizeof(*infop))` and then stores exactly
/// those six fields, so the padding after `si_code` and the rest of the union
/// keep whatever the caller had there, while an inaccessible structure is
/// `EFAULT` even when all six fields would have fit.
fn write_waitid_siginfo(
    memory: &UserMemoryCapability,
    infop: *mut siginfo,
    fields: WaitIdSiginfoFields,
) -> AxResult<()> {
    if infop.is_null() {
        return Ok(());
    }
    memory
        .with_memory(|memory| memory.validate_write_range(infop as usize, size_of::<siginfo>()))
        .map_err(map_usercopy_error)?;

    let mut head = [0u8; size_of::<[i32; 3]>()];
    head[0..4].copy_from_slice(&fields.signo.to_ne_bytes());
    // `si_errno` is the constant 0 the wrapper stores.
    head[8..12].copy_from_slice(&fields.code.to_ne_bytes());
    let mut tail = [0u8; size_of::<[i32; 3]>()];
    tail[0..4].copy_from_slice(&fields.pid.to_ne_bytes());
    tail[4..8].copy_from_slice(&fields.uid.to_ne_bytes());
    tail[8..12].copy_from_slice(&fields.status.to_ne_bytes());

    memory
        .write_bytes(infop as usize, &head)
        .map_err(map_usercopy_error)?;
    memory
        .write_bytes(infop as usize + SIGINFO_SIFIELDS_OFFSET, &tail)
        .map_err(map_usercopy_error)?;
    Ok(())
}

pub fn sys_waitid(
    memory: UserMemoryCapability,
    idtype: u32,
    id: u32,
    infop: *mut siginfo,
    options: u32,
    rusage_ptr: *mut rusage,
) -> AxResult<isize> {
    let outcome = waitid_claim(idtype, id, options);
    // `SYSCALL_DEFINE5(waitid)` stores `rusage` only for a reported event and
    // then stores the six `siginfo_t` fields on *every* path, including the
    // argument failures raised by `kernel_waitid_prepare()` before the wait
    // starts. A failed wait therefore leaves a zeroed event in the caller's
    // buffer instead of the caller's previous contents, and an `EFAULT` from
    // that mandatory store replaces the in-kernel error, exactly as Linux's
    // `Efault:` label does.
    match &outcome {
        Ok(WaitIdClaim::Event(event)) => {
            write_waitid_rusage(&memory, rusage_ptr, event)?;
            let viewer_user_ns = current().as_thread().current_user_namespace();
            write_waitid_siginfo(&memory, infop, event.waitid_siginfo_fields(&viewer_user_ns))?;
            Ok(0)
        }
        Ok(WaitIdClaim::Empty) => {
            write_waitid_siginfo(&memory, infop, WaitIdSiginfoFields::ZERO)?;
            Ok(0)
        }
        Err(error) => {
            write_waitid_siginfo(&memory, infop, WaitIdSiginfoFields::ZERO)?;
            Err(*error)
        }
    }
}

/// What one `waitid(2)` wait produced before any usercopy.
enum WaitIdClaim {
    /// A child event was claimed and must be reported.
    Event(WaitEvent),
    /// The wait completed without an event (`WNOHANG`, or `WNOWAIT` finding
    /// nothing): Linux reports success with the zeroed event.
    Empty,
}

/// Runs the wait half of `waitid(2)` and returns the claimed event.
fn waitid_claim(idtype: u32, id: u32, options: u32) -> AxResult<WaitIdClaim> {
    // `kernel_waitid_prepare()` checks the option mask, then that at least one
    // event bit is set, and only then the `which`/`upid` pair. The order
    // matters because a call failing both reports the *first* failure.
    let options = validate_waitid_options(options)?;
    if options.bits() & tk_linux_process::WAITID_EVENT_FLAGS == 0 {
        return Err(AxError::InvalidInput);
    }
    match tk_linux_process::validate_id_type(idtype as i32, id as i32) {
        Ok(tk_linux_process::WaitIdType::All) => debug_assert_eq!(idtype, P_ALL),
        Ok(tk_linux_process::WaitIdType::Pid(_)) => debug_assert_eq!(idtype, P_PID),
        Ok(tk_linux_process::WaitIdType::Pgid(_)) => debug_assert_eq!(idtype, P_PGID),
        Ok(tk_linux_process::WaitIdType::PidFd) => debug_assert_eq!(idtype, P_PIDFD),
        Err(_) => return Err(AxError::InvalidInput),
    }

    let curr = current();
    let viewer_pid_ns = curr.as_thread().pid_ns();
    // `kernel_waitid_prepare()` only resolves the `which`/`upid` pair into a
    // `struct pid`; it never errors when the number names no task.  The
    // ECHILD-vs-block decision happens in `__do_wait()`
    // (`kernel/exit.c:1706-1710`): `notask_error` starts at `-ECHILD` and is
    // only cleared by a matching child, so a pid that names nothing, or names
    // a task that is not a waitable child (including any live thread's tid,
    // which `pid_has_task(wo->pid, PIDTYPE_PID)` admits), reports ECHILD
    // without blocking, and only a real child parks the waiter.  ESRCH never
    // leaves waitid in this kernel -- the lone wait-path ESRCH is
    // `kernel_wait4(INT_MIN)` (`kernel/exit.c:1894`).  The candidate scan
    // below reproduces exactly that split: an empty candidate set is ECHILD
    // immediately, a non-child never matches the filter, and a live child
    // without a pending event blocks.
    let proc_data = &curr.as_thread().proc_data;
    let proc = &proc_data.proc;
    let nowait = options.contains(WaitOptions::WNOWAIT);

    let explicit_nohang = options.contains(WaitOptions::WNOHANG);
    let mut wait_options = options;
    let mut pidfd_nonblocking = false;
    let mut pidfd_candidate = None;
    let pid = match idtype {
        P_ALL => Some(WaitPid::Any),
        P_PID => Some(WaitPid::Pid(id as _)),
        P_PGID => {
            let pgid = id as i32;
            Some(if pgid == 0 {
                WaitPid::Pgid(proc.group().pgid())
            } else {
                WaitPid::Pgid(viewer_pid_ns.resolve_visible_pid(pgid as _).unwrap_or(0))
            })
        }
        P_PIDFD => {
            let pidfd = waitid_pidfd(id as i32)?;
            pidfd_nonblocking = pidfd.nonblocking();
            if pidfd_nonblocking {
                wait_options.insert(WaitOptions::WNOHANG);
            }
            pidfd_candidate = Some(pidfd_wait_candidate(
                proc,
                &viewer_pid_ns,
                &pidfd,
                &wait_options,
                curr.as_thread(),
            )?);
            None
        }
        _ => return Err(AxError::InvalidInput),
    };

    let check_children = || -> AxResult<Option<WaitIdClaim>> {
        let _wait_guard = proc_data.wait_lock.lock();
        loop {
            let candidates = if let Some(pid) = pid {
                matching_wait_candidates(
                    proc_data,
                    &viewer_pid_ns,
                    pid,
                    &wait_options,
                    curr.as_thread(),
                )?
            } else {
                vec![pidfd_candidate.clone().ok_or(AxError::InvalidInput)?]
            };

            let Some(event) = select_wait_event(
                &candidates,
                &wait_options,
                wait_options.contains(WaitOptions::WEXITED),
                proc.pid(),
            ) else {
                if wait_options.contains(WaitOptions::WNOHANG) {
                    if pidfd_nonblocking && !explicit_nohang {
                        return Err(AxError::from(LinuxError::EAGAIN));
                    }
                    return Ok(Some(WaitIdClaim::Empty));
                }
                return Ok(None);
            };

            if !claim_wait_event(&event, proc_data, nowait)? {
                // A concurrent waiter consumed the event first; rescan the
                // candidate set like Linux's do_wait loop instead of blocking
                // until the next child event.
                continue;
            }
            return Ok(Some(WaitIdClaim::Event(event)));
        }
    };

    let result = block_on_poll_set_interruptible_if(
        &proc_data.child_exit_event,
        || {
            if let Some(result) = check_children()? {
                Ok(result)
            } else if has_pending_syscall_signal(curr.as_thread()) {
                Err(AxError::Interrupted)
            } else {
                Err(AxError::WouldBlock)
            }
        },
        || has_pending_syscall_signal(curr.as_thread()),
    )?;
    axtask::reclaim_exited_tasks_until_clear(POST_WAIT_RECLAIM_YIELDS);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{WaitPid, stop_filter_for};
    use crate::task::{PtraceSession, StopFilter, StopReport};

    #[test]
    fn process_access_wait_candidate_does_not_cross_ptrace_generations() {
        let old = PtraceSession {
            tracer: 7,
            tracer_kernel_tid: 70,
            generation: 1,
        };
        let new = PtraceSession {
            tracer: 7,
            tracer_kernel_tid: 70,
            generation: 2,
        };
        let old_stop = StopReport {
            signal: 19,
            ptrace_event: 0,
            ptrace_session: Some(old),
        };
        let new_stop = StopReport {
            signal: 19,
            ptrace_event: 0,
            ptrace_session: Some(new),
        };
        let job_control_stop = StopReport {
            signal: 19,
            ptrace_event: 0,
            ptrace_session: None,
        };

        // A waiter that named a session sees exactly that session's stop.
        assert!(StopFilter::Session(old).accepts(old_stop.ptrace_session));
        assert!(!StopFilter::Session(old).accepts(new_stop.ptrace_session));
        // A waiter that named none is the real parent. `wait_consider_task()`
        // still reports the ptrace stop to it when the tracer shares its thread
        // group (`PTRACE_TRACEME`), and hides a separate ptracer's stop.
        assert!(stop_filter_for(None, 7).accepts(old_stop.ptrace_session));
        assert!(stop_filter_for(None, 7).accepts(new_stop.ptrace_session));
        assert!(!stop_filter_for(None, 9).accepts(old_stop.ptrace_session));
        // Job-control stops are sessionless and visible to every waiter.
        assert!(stop_filter_for(None, 9).accepts(job_control_stop.ptrace_session));
        assert!(StopFilter::Session(old).accepts(job_control_stop.ptrace_session));
        assert!(StopFilter::Session(new).accepts(job_control_stop.ptrace_session));
    }

    #[test]
    fn wait_pid_matches_the_caller_visible_pid_and_pgid() {
        // Process IDs are caller-visible; group selectors have already been
        // resolved to the stable kernel identity (12 here).
        assert!(WaitPid::Pid(46).matches(46, 12));
        assert!(!WaitPid::Pid(79).matches(46, 12));
        assert!(WaitPid::Pgid(12).matches(46, 12));
        assert!(!WaitPid::Pgid(78).matches(46, 12));
        assert!(WaitPid::Any.matches(46, 12));
    }

    #[test]
    fn namespace_wait_keeps_children_whose_group_is_outside_the_namespace() {
        use alloc::sync::Arc;

        use crate::task::{PidNamespace, UserNamespace};

        let user = UserNamespace::try_new_root().unwrap();
        let outer = PidNamespace::try_new_root(user.clone()).unwrap();
        outer.reserve_process(100).unwrap().commit();
        let domain = tk_linux_process_adapter::ProcessDomain::try_new().unwrap();
        let root = domain
            .try_new_init_with_identity(
                100,
                None,
                crate::task::ProcessIdentity::try_new(outer.clone(), None).unwrap(),
            )
            .unwrap();
        domain.prepare_thread(&root, 100).unwrap().commit().unwrap();
        let scope = domain.try_new_reaper_scope().unwrap();
        let inner = outer
            .try_fork_with_reaper_scope(200, user, scope.clone())
            .unwrap();
        inner.reserve_process(200).unwrap().commit();
        let admission = domain
            .prepare_fork_as_reaper_scope_init_with_identity(
                &root,
                &scope,
                200,
                None,
                crate::task::ProcessIdentity::try_new(inner.clone(), None).unwrap(),
            )
            .unwrap();
        let init = admission
            .prepare_initial_thread(200)
            .unwrap()
            .commit()
            .unwrap();
        inner.reserve_process(201).unwrap().commit();
        let admission = domain
            .prepare_fork_in_reaper_scope_with_identity(
                &init,
                &scope,
                201,
                Some(17),
                crate::task::ProcessIdentity::try_new(inner.clone(), None).unwrap(),
            )
            .unwrap();
        let child = admission.process().clone();
        admission.commit();
        assert!(Arc::ptr_eq(&init.group(), &root.group()));
        assert_eq!(inner.visible_pid_checked(child.group().pgid()), None);
        for selector in [
            WaitPid::Any,
            WaitPid::Pid(2),
            WaitPid::Pgid(init.group().pgid()),
        ] {
            assert_eq!(super::wait_pid_applies(&inner, selector, &child), Some(2));
        }
        assert_eq!(
            super::wait_pid_applies(&inner, WaitPid::Pgid(200), &child),
            None
        );
    }
}
