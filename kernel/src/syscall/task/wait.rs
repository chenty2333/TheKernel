use alloc::{sync::Arc, vec::Vec};
use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::{current, future::block_on};
use bitflags::bitflags;
use linux_raw_sys::general::{
    __WALL, __WCLONE, __WNOTHREAD, CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED,
    P_ALL, P_PGID, P_PID, SIGCHLD, SIGCONT, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WUNTRACED,
    rusage, siginfo,
};
use starry_process::{Pid, Process, ZombieSnapshot};
use starry_vm::{VmMutPtr, VmPtr};

use crate::task::{
    AsThread, ProcessData, TaskUsage, cleanup_task_tables, get_process_data,
    has_pending_syscall_signal,
};

const WAITPID_ALLOWED_BITS: u32 =
    WNOHANG | WUNTRACED | WCONTINUED | __WNOTHREAD | __WALL | __WCLONE;
const WAITID_ALLOWED_BITS: u32 =
    WNOHANG | WEXITED | WUNTRACED | WCONTINUED | WNOWAIT | __WNOTHREAD | __WALL | __WCLONE;

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
enum WaitEvent {
    Stopped {
        pid: Pid,
        stop_signal: u8,
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
            WaitEvent::Stopped { stop_signal, .. } => ((*stop_signal as i32) << 8) | 0x7f,
            WaitEvent::Continued { .. } => 0xffff,
            WaitEvent::Exited { snapshot, .. } => snapshot.wait_status,
        }
    }

    fn waitid_siginfo(&self) -> siginfo {
        match self {
            WaitEvent::Stopped {
                pid, stop_signal, ..
            } => fill_siginfo(*pid, 0, CLD_STOPPED, *stop_signal as i32),
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

fn matching_children(
    proc: &Process,
    pid: WaitPid,
    options: &WaitOptions,
) -> AxResult<Vec<Arc<Process>>> {
    let children = proc
        .children()
        .into_iter()
        .filter(|child| pid.apply(child) && should_wait_for_child(child, options))
        .collect::<Vec<_>>();

    if children.is_empty() {
        Err(AxError::from(LinuxError::ECHILD))
    } else {
        Ok(children)
    }
}

fn select_wait_event(
    children: &[Arc<Process>],
    options: &WaitOptions,
    wait_exited: bool,
) -> Option<WaitEvent> {
    if options.contains(WaitOptions::WUNTRACED) {
        for child in children {
            if let Ok(proc_data) = get_process_data(child.pid())
                && let Some(stop_signal) = proc_data.peek_stop_status()
            {
                return Some(WaitEvent::Stopped {
                    pid: child.pid(),
                    stop_signal,
                    proc_data,
                });
            }
        }
    }

    if options.contains(WaitOptions::WCONTINUED) {
        for child in children {
            if let Ok(proc_data) = get_process_data(child.pid())
                && proc_data.peek_continued()
            {
                return Some(WaitEvent::Continued {
                    pid: child.pid(),
                    proc_data,
                });
            }
        }
    }

    if wait_exited {
        for child in children {
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
            stop_signal,
            proc_data,
            ..
        } => proc_data.restore_stop_status(*stop_signal),
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
    info!("sys_waitpid <= pid: {pid:?}, options: {options:?}");

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
        let children = matching_children(proc, pid, &options)?;

        if let Some(event) = select_wait_event(&children, &options, true) {
            let claimed_event = match &event {
                WaitEvent::Stopped {
                    stop_signal,
                    proc_data,
                    ..
                } => {
                    let Some(claimed_signal) = proc_data.claim_stop_status() else {
                        return Ok(None);
                    };
                    if claimed_signal != *stop_signal {
                        proc_data.restore_stop_status(claimed_signal);
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
                    if !child.reap() {
                        return Ok(None);
                    }
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
    axtask::reclaim_exited_tasks();
    cleanup_task_tables();
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
    info!("sys_waitid <= idtype: {idtype}, id: {id}, options: {options:?}");

    if !options.intersects(WaitOptions::WEXITED | WaitOptions::WUNTRACED | WaitOptions::WCONTINUED)
    {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let proc = &proc_data.proc;
    let nowait = options.contains(WaitOptions::WNOWAIT);

    let pid = match idtype {
        P_ALL => WaitPid::Any,
        P_PID => WaitPid::Pid(id as _),
        P_PGID => {
            if id == 0 {
                WaitPid::Pgid(proc.group().pgid())
            } else {
                WaitPid::Pgid(id as _)
            }
        }
        _ => return Err(AxError::InvalidInput),
    };

    let check_children = || -> AxResult<Option<isize>> {
        let _wait_guard = proc_data.wait_lock.lock();
        let children = matching_children(proc, pid, &options)?;

        if let Some(event) =
            select_wait_event(&children, &options, options.contains(WaitOptions::WEXITED))
        {
            let claimed_event = if nowait {
                None
            } else {
                match &event {
                    WaitEvent::Stopped {
                        stop_signal,
                        proc_data,
                        ..
                    } => {
                        let Some(claimed_signal) = proc_data.claim_stop_status() else {
                            return Ok(None);
                        };
                        if claimed_signal != *stop_signal {
                            proc_data.restore_stop_status(claimed_signal);
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
                    proc_data.account_waited_child(snapshot.total_usage().into());
                }
                _ => {}
            }

            return Ok(Some(0));
        }

        if options.contains(WaitOptions::WNOHANG) {
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
    axtask::reclaim_exited_tasks();
    cleanup_task_tables();
    Ok(result)
}
