use alloc::{sync::Arc, vec, vec::Vec};
use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::{current, future::block_on};
use bitflags::bitflags;
use linux_raw_sys::general::{
    __WALL, __WCLONE, __WNOTHREAD, CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED,
    CLD_TRAPPED, P_ALL, P_PGID, P_PID, P_PIDFD, SIGCHLD, SIGCONT, WCONTINUED, WEXITED, WNOHANG,
    WNOWAIT, WUNTRACED, rusage, siginfo,
};
use starry_process::{Pid, Process, ZombieSnapshot};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    file::{FileHandle, FileLike, PidFd},
    pseudofs::cgroup,
    task::{
        AsThread, ProcessData, StopReport, TaskUsage, get_process_data, has_pending_syscall_signal,
    },
};

const WAITPID_ALLOWED_BITS: u32 =
    WNOHANG | WUNTRACED | WCONTINUED | __WNOTHREAD | __WALL | __WCLONE;
const WAITID_ALLOWED_BITS: u32 =
    WNOHANG | WEXITED | WUNTRACED | WCONTINUED | WNOWAIT | __WNOTHREAD | __WALL | __WCLONE;
const POST_WAIT_RECLAIM_YIELDS: usize = 4;

bitflags! {
    #[derive(Debug)]
    struct WaitOptions: u32 {
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
    /// Wait for any child process whose process group ID is equal to the value.
    Pgid(Pid),
}

impl WaitPid {
    fn apply(&self, child: &Process) -> bool {
        match self {
            WaitPid::Any => true,
            WaitPid::Pid(pid) => child.pid() == *pid,
            WaitPid::Pgid(pgid) => child.group().pgid() == *pgid,
        }
    }
}

#[derive(Clone)]
struct WaitCandidate {
    process: Arc<Process>,
    allow_exit: bool,
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
        child: Arc<Process>,
        snapshot: ZombieSnapshot,
    },
}

impl WaitEvent {
    fn pid(&self) -> Pid {
        match self {
            WaitEvent::Stopped { pid, .. } | WaitEvent::Continued { pid, .. } => *pid,
            WaitEvent::Exited { child, .. } => child.pid(),
        }
    }

    fn waitpid_status(&self) -> i32 {
        match self {
            WaitEvent::Stopped { stop, .. } => ((stop.signal as i32) << 8) | 0x7f,
            WaitEvent::Continued { .. } => 0xffff,
            WaitEvent::Exited { snapshot, .. } => snapshot.wait_status,
        }
    }

    fn waitid_siginfo(&self) -> siginfo {
        match self {
            WaitEvent::Stopped { pid, stop, .. } => fill_siginfo(
                *pid,
                0,
                if stop.traced {
                    CLD_TRAPPED
                } else {
                    CLD_STOPPED
                },
                stop.signal as i32,
            ),
            WaitEvent::Continued { pid, .. } => {
                fill_siginfo(*pid, 0, CLD_CONTINUED, SIGCONT as i32)
            }
            WaitEvent::Exited { child, snapshot } => {
                let (si_code, si_status) = decode_exit_code(snapshot.wait_status);
                fill_siginfo(child.pid(), snapshot.uid, si_code, si_status)
            }
        }
    }

    fn exited_usage(&self) -> Option<TaskUsage> {
        match self {
            WaitEvent::Exited { snapshot, .. } => Some(snapshot.total_usage().into()),
            _ => None,
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
fn should_wait_for_child(child: &Process, options: &WaitOptions) -> bool {
    if options.contains(WaitOptions::WALL) {
        return true;
    }

    let is_clone = child.exit_signal() != Some(starry_signal::Signo::SIGCHLD as u8);
    if options.contains(WaitOptions::WCLONE) {
        is_clone
    } else {
        !is_clone
    }
}

fn matching_wait_candidates(
    proc_data: &ProcessData,
    pid: WaitPid,
    options: &WaitOptions,
) -> AxResult<Vec<WaitCandidate>> {
    let proc = &proc_data.proc;
    let mut candidates = proc
        .try_children()
        .map_err(|_| AxError::NoMemory)?
        .into_iter()
        .filter(|child| pid.apply(child) && should_wait_for_child(child, options))
        .map(|process| WaitCandidate {
            process,
            allow_exit: true,
        })
        .collect::<Vec<_>>();

    for tracee_pid in proc_data.ptrace_tracees() {
        let Ok(tracee_data) = get_process_data(tracee_pid) else {
            proc_data.remove_ptrace_tracee(tracee_pid);
            continue;
        };
        if !tracee_data.is_traced_by(proc.pid()) {
            proc_data.remove_ptrace_tracee(tracee_pid);
            continue;
        }
        let tracee = &tracee_data.proc;
        if !pid.apply(tracee) {
            continue;
        }
        if candidates
            .iter()
            .any(|candidate| candidate.process.pid() == tracee.pid())
        {
            continue;
        }
        candidates.push(WaitCandidate {
            process: tracee.clone(),
            allow_exit: false,
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
    pidfd: &PidFd,
    options: &WaitOptions,
) -> AxResult<WaitCandidate> {
    let target = pidfd.process()?;

    if target
        .parent()
        .is_none_or(|parent| parent.pid() != proc.pid())
        || !should_wait_for_child(&target, options)
    {
        return Err(AxError::from(LinuxError::ECHILD));
    }

    Ok(WaitCandidate {
        process: target,
        allow_exit: true,
    })
}

fn select_wait_event(
    candidates: &[WaitCandidate],
    options: &WaitOptions,
    wait_exited: bool,
) -> Option<WaitEvent> {
    for candidate in candidates {
        if let Ok(proc_data) = get_process_data(candidate.process.pid())
            && let Some(stop) = proc_data.peek_stop_status()
            && (stop.traced || options.contains(WaitOptions::WUNTRACED))
        {
            return Some(WaitEvent::Stopped {
                pid: candidate.process.pid(),
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
                    pid: candidate.process.pid(),
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
                && let Some(snapshot) = child.zombie_snapshot()
            {
                return Some(WaitEvent::Exited {
                    child: child.clone(),
                    snapshot,
                });
            }
        }
    }

    None
}

fn write_waitpid_event(
    event: &WaitEvent,
    exit_code: *mut i32,
    rusage_ptr: *mut rusage,
) -> AxResult<()> {
    if let Some(exit_code) = exit_code.nullable() {
        exit_code.vm_write(event.waitpid_status())?;
    }
    if let Some(usage) = event.exited_usage()
        && let Some(rusage_ptr) = rusage_ptr.nullable()
    {
        rusage_ptr.vm_write(usage.into())?;
    }
    Ok(())
}

fn write_waitid_event(
    event: &WaitEvent,
    infop: *mut siginfo,
    rusage_ptr: *mut rusage,
) -> AxResult<()> {
    if let Some(infop) = infop.nullable() {
        infop.vm_write(event.waitid_siginfo())?;
    }
    if let Some(usage) = event.exited_usage()
        && let Some(rusage_ptr) = rusage_ptr.nullable()
    {
        rusage_ptr.vm_write(usage.into())?;
    }
    Ok(())
}

fn restore_wait_event(event: &WaitEvent) {
    match event {
        WaitEvent::Stopped {
            stop, proc_data, ..
        } => proc_data.restore_stop_status(*stop),
        WaitEvent::Continued { proc_data, .. } => proc_data.restore_continued(),
        WaitEvent::Exited { .. } => {}
    }
}

pub fn sys_waitpid(
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

    let pid = if pid == -1 {
        WaitPid::Any
    } else if pid == 0 {
        WaitPid::Pgid(proc.group().pgid())
    } else if pid > 0 {
        WaitPid::Pid(pid as _)
    } else {
        WaitPid::Pgid(-pid as _)
    };
    let check_children = || {
        let _wait_guard = proc_data.wait_lock.lock();
        let candidates = match matching_wait_candidates(proc_data, pid, &options) {
            Ok(candidates) => candidates,
            Err(err) => return Err(err),
        };

        if let Some(event) = select_wait_event(&candidates, &options, true) {
            let claimed_event = match &event {
                WaitEvent::Stopped {
                    stop, proc_data, ..
                } => {
                    let Some(claimed_stop) = proc_data.claim_stop_status() else {
                        return Ok(None);
                    };
                    if claimed_stop != *stop {
                        proc_data.restore_stop_status(claimed_stop);
                        return Ok(None);
                    }
                    Some(event.clone())
                }
                WaitEvent::Continued { proc_data, .. } => {
                    if !proc_data.claim_continued() {
                        return Ok(None);
                    }
                    Some(event.clone())
                }
                WaitEvent::Exited { .. } => None,
            };

            if let Err(err) = write_waitpid_event(&event, exit_code, rusage_ptr) {
                if let Some(claimed_event) = &claimed_event {
                    restore_wait_event(claimed_event);
                }
                return Err(err);
            }

            match &event {
                WaitEvent::Exited { child, snapshot } => {
                    let reaped = child.reap();
                    if !reaped {
                        return Ok(None);
                    }
                    cgroup::detach_process(child.pid());
                    proc_data.account_waited_child(snapshot.total_usage().into());
                }
                _ => {}
            }

            return Ok(Some(event.pid() as isize));
        }

        if options.contains(WaitOptions::WNOHANG) {
            Ok(Some(0))
        } else {
            Ok(None)
        }
    };

    let result = block_on(poll_fn(|cx| {
        if let Some(res) = check_children().transpose() {
            return Poll::Ready(res);
        }

        if has_pending_syscall_signal(curr.as_thread()) {
            return Poll::Ready(Err(AxError::Interrupted));
        }

        if curr.poll_interrupt(cx).is_ready() {
            if let Some(res) = check_children().transpose() {
                return Poll::Ready(res);
            }
            if has_pending_syscall_signal(curr.as_thread()) {
                return Poll::Ready(Err(AxError::Interrupted));
            }
        }

        proc_data.child_exit_event.register(cx.waker());
        if let Some(res) = check_children().transpose() {
            Poll::Ready(res)
        } else {
            Poll::Pending
        }
    }));
    let result = result?;
    axtask::reclaim_exited_tasks_until_clear(POST_WAIT_RECLAIM_YIELDS);
    Ok(result)
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
                WaitPid::Pgid(pgid as _)
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
            pidfd_candidate = Some(pidfd_wait_candidate(proc, &pidfd, &wait_options)?);
            None
        }
        _ => return Err(AxError::InvalidInput),
    };

    let check_children = || -> AxResult<Option<isize>> {
        let _wait_guard = proc_data.wait_lock.lock();
        let candidates = if let Some(pid) = pid {
            matching_wait_candidates(proc_data, pid, &wait_options)?
        } else {
            vec![pidfd_candidate.clone().ok_or(AxError::InvalidInput)?]
        };

        if let Some(event) = select_wait_event(
            &candidates,
            &wait_options,
            wait_options.contains(WaitOptions::WEXITED),
        ) {
            let claimed_event = if nowait {
                None
            } else {
                match &event {
                    WaitEvent::Stopped {
                        stop, proc_data, ..
                    } => {
                        let Some(claimed_stop) = proc_data.claim_stop_status() else {
                            return Ok(None);
                        };
                        if claimed_stop != *stop {
                            proc_data.restore_stop_status(claimed_stop);
                            return Ok(None);
                        }
                        Some(event.clone())
                    }
                    WaitEvent::Continued { proc_data, .. } => {
                        if !proc_data.claim_continued() {
                            return Ok(None);
                        }
                        Some(event.clone())
                    }
                    WaitEvent::Exited { .. } => None,
                }
            };

            if let Err(err) = write_waitid_event(&event, infop, rusage_ptr) {
                if let Some(claimed_event) = &claimed_event {
                    restore_wait_event(claimed_event);
                }
                return Err(err);
            }

            match &event {
                WaitEvent::Exited { child, snapshot } if !nowait => {
                    if !child.reap() {
                        return Ok(None);
                    }
                    cgroup::detach_process(child.pid());
                    proc_data.account_waited_child(snapshot.total_usage().into());
                }
                _ => {}
            }

            return Ok(Some(0));
        }

        if wait_options.contains(WaitOptions::WNOHANG) {
            if pidfd_nonblocking && !explicit_nohang {
                return Err(AxError::from(LinuxError::EAGAIN));
            }
            if let Some(infop) = infop.nullable() {
                infop.vm_write(unsafe { core::mem::zeroed::<siginfo>() })?;
            }
            Ok(Some(0))
        } else {
            Ok(None)
        }
    };

    let result = block_on(poll_fn(|cx| {
        if let Some(res) = check_children().transpose() {
            return Poll::Ready(res);
        }

        if has_pending_syscall_signal(curr.as_thread()) {
            return Poll::Ready(Err(AxError::Interrupted));
        }

        if curr.poll_interrupt(cx).is_ready() {
            if let Some(res) = check_children().transpose() {
                return Poll::Ready(res);
            }
            if has_pending_syscall_signal(curr.as_thread()) {
                return Poll::Ready(Err(AxError::Interrupted));
            }
        }

        proc_data.child_exit_event.register(cx.waker());
        if let Some(res) = check_children().transpose() {
            Poll::Ready(res)
        } else {
            Poll::Pending
        }
    }))?;
    axtask::reclaim_exited_tasks_until_clear(POST_WAIT_RECLAIM_YIELDS);
    Ok(result)
}
