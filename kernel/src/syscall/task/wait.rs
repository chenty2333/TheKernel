use alloc::{sync::Arc, vec, vec::Vec};

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
        AsThread, PidNamespace, Process, ProcessData, PtraceSession, StopReport, TaskUsage,
        ZombieSnapshot, get_process_data, has_pending_syscall_signal, process_domain, reap_process,
    },
};

const WAITPID_ALLOWED_BITS: u32 =
    WNOHANG | WUNTRACED | WCONTINUED | __WNOTHREAD | __WALL | __WCLONE;
const WAITID_ALLOWED_BITS: u32 =
    WNOHANG | WEXITED | WUNTRACED | WCONTINUED | WNOWAIT | __WNOTHREAD | __WALL | __WCLONE;
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
    let target_pid_ns = process.identity::<Arc<PidNamespace>>()?;
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

    fn waitid_siginfo(&self, viewer_user_ns: &crate::task::UserNamespace) -> siginfo {
        match self {
            WaitEvent::Stopped {
                pid,
                stop,
                proc_data,
            } => {
                let uid = viewer_user_ns.from_kuid_munged(proc_data.group_leader_cred().ids().ruid);
                fill_siginfo(
                    *pid,
                    uid,
                    if stop.traced() {
                        CLD_TRAPPED
                    } else {
                        CLD_STOPPED
                    },
                    stop.signal as i32,
                )
            }
            WaitEvent::Continued { pid, proc_data } => {
                let uid = viewer_user_ns.from_kuid_munged(proc_data.group_leader_cred().ids().ruid);
                fill_siginfo(*pid, uid, CLD_CONTINUED, SIGCONT as i32)
            }
            WaitEvent::Exited { pid, snapshot, .. } => {
                let (si_code, si_status) = decode_exit_code(snapshot.wait_status);
                let uid = viewer_user_ns.from_kuid_munged(snapshot.credential.ids().ruid);
                fill_siginfo(*pid, uid, si_code, si_status)
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

fn validate_waitpid_options(options: u32) -> AxResult<WaitOptions> {
    if options & !WAITPID_ALLOWED_BITS != 0 {
        return Err(AxError::InvalidInput);
    }
    Ok(WaitOptions::from_bits_truncate(options))
}

fn validate_waitid_options(options: u32) -> AxResult<WaitOptions> {
    if options & !WAITID_ALLOWED_BITS != 0 {
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
) -> AxResult<Vec<WaitCandidate>> {
    let proc = &proc_data.proc;
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
) -> AxResult<WaitCandidate> {
    let target = pidfd.process()?;
    let visible_pid =
        visible_process_pid(viewer_pid_ns, &target).ok_or(AxError::from(LinuxError::ECHILD))?;

    if target
        .parent()
        .is_none_or(|parent| parent.pid() != proc.pid())
        || !should_wait_for_child(&target, options)
    {
        return Err(AxError::from(LinuxError::ECHILD));
    }

    let expected_ptrace_session = get_process_data(target.pid())
        .ok()
        .and_then(|target| target.ptrace_session_if_traced_by_process(proc.pid()));

    Ok(WaitCandidate {
        process: target,
        visible_pid,
        allow_exit: true,
        expected_ptrace_session,
    })
}

fn select_wait_event(
    candidates: &[WaitCandidate],
    options: &WaitOptions,
    wait_exited: bool,
) -> Option<WaitEvent> {
    for candidate in candidates {
        if let Ok(proc_data) = get_process_data(candidate.process.pid())
            && let Some(stop) = proc_data.peek_stop_status(candidate.expected_ptrace_session)
            && wait_candidate_accepts_stop(candidate.expected_ptrace_session, stop)
            && (stop.traced() || options.contains(WaitOptions::WUNTRACED))
        {
            return Some(WaitEvent::Stopped {
                pid: candidate.visible_pid,
                stop,
                proc_data,
            });
        }
    }

    if options.contains(WaitOptions::WCONTINUED) {
        for candidate in candidates {
            if let Ok(proc_data) = get_process_data(candidate.process.pid())
                && proc_data.peek_continued()
            {
                return Some(WaitEvent::Continued {
                    pid: candidate.visible_pid,
                    proc_data,
                });
            }
        }
    }

    if wait_exited {
        for candidate in candidates {
            if !candidate.allow_exit {
                continue;
            }
            let child = &candidate.process;
            if child.is_zombie()
                && let Some(snapshot) = child.zombie_payload()
            {
                return Some(WaitEvent::Exited {
                    pid: candidate.visible_pid,
                    child: child.clone(),
                    snapshot,
                });
            }
        }
    }

    None
}

fn wait_candidate_accepts_stop(
    expected_ptrace_session: Option<PtraceSession>,
    stop: StopReport,
) -> bool {
    stop.ptrace_session
        .is_none_or(|session| Some(session) == expected_ptrace_session)
}

fn write_waitpid_event(
    memory: &UserMemoryCapability,
    event: &WaitEvent,
    exit_code: *mut i32,
    rusage_ptr: *mut rusage,
) -> AxResult<()> {
    if !exit_code.is_null() {
        memory
            .write_value(exit_code, event.waitpid_status())
            .map_err(map_usercopy_error)?;
    }
    if !rusage_ptr.is_null() {
        let usage = event.usage();
        // TaskUsage's conversion starts from a zeroed rusage and fills
        // every exposed field, so the complete ABI representation is
        // initialized for this unchecked copyout.
        unsafe {
            memory
                .write_value_unchecked(rusage_ptr, usage.into())
                .map_err(map_usercopy_error)?;
        }
    }
    Ok(())
}

fn write_waitid_event(
    memory: &UserMemoryCapability,
    event: &WaitEvent,
    viewer_user_ns: &crate::task::UserNamespace,
    infop: *mut siginfo,
    rusage_ptr: *mut rusage,
) -> AxResult<()> {
    if !infop.is_null() {
        // fill_siginfo starts from zero, including ABI padding/tail bytes.
        unsafe {
            memory
                .write_value_unchecked(infop, event.waitid_siginfo(viewer_user_ns))
                .map_err(map_usercopy_error)?;
        }
    }
    if !rusage_ptr.is_null() {
        let usage = event.usage();
        unsafe {
            memory
                .write_value_unchecked(rusage_ptr, usage.into())
                .map_err(map_usercopy_error)?;
        }
    }
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
            let Some(claimed) = proc_data.claim_stop_status(stop.ptrace_session) else {
                return Ok(false);
            };
            if claimed != *stop {
                proc_data.restore_stop_status(claimed);
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
        let candidates = match matching_wait_candidates(proc_data, &viewer_pid_ns, pid, &options) {
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

        if let Some(event) = select_wait_event(&candidates, &options, true) {
            // Diagnostic: record what this pid-targeted wait is about to
            // return, before the event is consumed. `event.pid()` is the
            // candidate's namespace-visible pid, which may differ from the
            // requested pid when the wait matched something else.
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
            if !claim_wait_event(&event, proc_data, false)? {
                return Ok(None);
            }
            // The immutable snapshot and consumed-event claim now suffice;
            // a userfaultfd stall must not hold the parent's wait mutex.
            drop(_wait_guard);
            write_waitpid_event(&memory, &event, exit_code, rusage_ptr)?;

            return Ok(Some(event.pid() as isize));
        }

        trace_wait(pid, -1, 0, &options, 4, 0, 0);
        if options.contains(WaitOptions::WNOHANG) {
            Ok(Some(0))
        } else {
            Ok(None)
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

/// Fills a siginfo_t struct for waitid.
fn fill_siginfo(pid: Pid, uid: u32, si_code: u32, si_status: i32) -> siginfo {
    let mut info: siginfo = unsafe { core::mem::zeroed() };
    unsafe {
        let inner = &mut info.__bindgen_anon_1.__bindgen_anon_1;
        inner.si_signo = SIGCHLD as i32;
        inner.si_code = si_code as i32;
        inner._sifields._sigchld._pid = pid as _;
        inner._sifields._sigchld._uid = uid;
        inner._sifields._sigchld._status = si_status;
    }
    info
}

pub fn sys_waitid(
    memory: UserMemoryCapability,
    idtype: u32,
    id: u32,
    infop: *mut siginfo,
    options: u32,
    rusage_ptr: *mut rusage,
) -> AxResult<isize> {
    let options = validate_waitid_options(options)?;

    if !options.intersects(WaitOptions::WEXITED | WaitOptions::WUNTRACED | WaitOptions::WCONTINUED)
    {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let viewer_user_ns = curr.as_thread().current_user_namespace();
    let viewer_pid_ns = curr.as_thread().pid_ns();
    let proc_data = &curr.as_thread().proc_data;
    let proc = &proc_data.proc;
    let nowait = options.contains(WaitOptions::WNOWAIT);

    let explicit_nohang = options.contains(WaitOptions::WNOHANG);
    let mut wait_options = options;
    let mut pidfd_nonblocking = false;
    let mut pidfd_candidate = None;
    let pid = match idtype {
        P_ALL => Some(WaitPid::Any),
        P_PID => {
            let pid = id as i32;
            if pid <= 0 {
                return Err(AxError::InvalidInput);
            }
            Some(WaitPid::Pid(pid as _))
        }
        P_PGID => {
            let pgid = id as i32;
            if pgid < 0 {
                return Err(AxError::InvalidInput);
            }
            Some(if pgid == 0 {
                WaitPid::Pgid(proc.group().pgid())
            } else {
                WaitPid::Pgid(viewer_pid_ns.resolve_visible_pid(pgid as _).unwrap_or(0))
            })
        }
        P_PIDFD => {
            if id > i32::MAX as u32 {
                return Err(AxError::InvalidInput);
            }
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
            )?);
            None
        }
        _ => return Err(AxError::InvalidInput),
    };

    let check_children = || -> AxResult<Option<isize>> {
        let _wait_guard = proc_data.wait_lock.lock();
        let candidates = if let Some(pid) = pid {
            matching_wait_candidates(proc_data, &viewer_pid_ns, pid, &wait_options)?
        } else {
            vec![pidfd_candidate.clone().ok_or(AxError::InvalidInput)?]
        };

        if let Some(event) = select_wait_event(
            &candidates,
            &wait_options,
            wait_options.contains(WaitOptions::WEXITED),
        ) {
            if !claim_wait_event(&event, proc_data, nowait)? {
                return Ok(None);
            }
            drop(_wait_guard);
            write_waitid_event(&memory, &event, &viewer_user_ns, infop, rusage_ptr)?;

            return Ok(Some(0));
        }

        if wait_options.contains(WaitOptions::WNOHANG) {
            if pidfd_nonblocking && !explicit_nohang {
                return Err(AxError::from(LinuxError::EAGAIN));
            }
            drop(_wait_guard);
            if !infop.is_null() {
                // The zeroed siginfo has a fully initialized ABI
                // representation, including padding bytes.
                unsafe {
                    memory
                        .write_value_unchecked(infop, core::mem::zeroed::<siginfo>())
                        .map_err(map_usercopy_error)?;
                }
            }
            Ok(Some(0))
        } else {
            Ok(None)
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
    use super::{WaitPid, wait_candidate_accepts_stop};
    use crate::task::{PtraceSession, StopReport};

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

        assert!(wait_candidate_accepts_stop(Some(old), old_stop));
        assert!(!wait_candidate_accepts_stop(Some(old), new_stop));
        assert!(!wait_candidate_accepts_stop(None, old_stop));
        assert!(wait_candidate_accepts_stop(None, job_control_stop));
        assert!(wait_candidate_accepts_stop(Some(new), job_control_stop));
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
            .try_new_init_with_identity(100, None, outer.clone())
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
                inner.clone(),
            )
            .unwrap();
        let init = admission
            .prepare_initial_thread(200)
            .unwrap()
            .commit()
            .unwrap();
        inner.reserve_process(201).unwrap().commit();
        let admission = domain
            .prepare_fork_in_reaper_scope_with_identity(&init, &scope, 201, Some(17), inner.clone())
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
