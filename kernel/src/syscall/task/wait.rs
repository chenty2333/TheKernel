use alloc::vec::Vec;
use core::{future::poll_fn, task::Poll};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::{current, future::block_on};
use bitflags::bitflags;
use linux_raw_sys::general::{
    __WALL, __WCLONE, __WNOTHREAD, CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED,
    CLD_STOPPED, P_ALL, P_PGID, P_PID, SIGCHLD, SIGCONT, WCONTINUED, WEXITED, WNOHANG, WNOWAIT,
    WUNTRACED, rusage, siginfo,
};
use starry_process::{Pid, Process};
use starry_vm::{VmMutPtr, VmPtr};

use crate::task::{AsThread, get_cached_exit_signal, get_process_data, remove_cached_exit_signal};

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

        /// Don't wait on children of other threads in this group
        const WNOTHREAD = __WNOTHREAD;
        /// Wait on all children, regardless of type
        const WALL = __WALL;
        /// Wait for "clone" children only.
        const WCLONE = __WCLONE;
    }
}

#[derive(Debug, Clone, Copy)]
enum WaitPid {
    /// Wait for any child process
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

/// Determines whether a child should be included in wait based on WALL/WCLONE flags.
///
/// Without WALL: default behavior excludes clone children (those created with clone()
/// using a non-SIGCHLD or no exit signal).
/// With WCLONE: only wait for clone children.
/// With WALL: wait for all children regardless of type.
fn should_wait_for_child(child: &Process, options: &WaitOptions) -> bool {
    if options.contains(WaitOptions::WALL) {
        return true;
    }
    // Use the exit_signal cache which survives ProcessData drop for zombie children.
    let is_clone = get_cached_exit_signal(child.pid())
        .map(|sig| sig != Some(starry_signal::Signo::SIGCHLD))
        .unwrap_or(false);
    if options.contains(WaitOptions::WCLONE) {
        is_clone
    } else {
        !is_clone
    }
}

pub fn sys_waitpid(pid: i32, exit_code: *mut i32, options: u32, rusage_ptr: *mut rusage) -> AxResult<isize> {
    let options = WaitOptions::from_bits_truncate(options);
    info!("sys_waitpid <= pid: {pid:?}, options: {options:?}");

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let proc = &proc_data.proc;
    let nowait = options.contains(WaitOptions::WNOWAIT);

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
        let children = proc
            .children()
            .into_iter()
            .filter(|child| pid.apply(child) && should_wait_for_child(child, &options))
            .collect::<Vec<_>>();
        if children.is_empty() {
            return Err(AxError::from(LinuxError::ECHILD));
        }

        // Check for stopped children (WUNTRACED).
        if options.contains(WaitOptions::WUNTRACED) {
            for child in children.iter() {
                if let Ok(data) = get_process_data(child.pid()) {
                    let stop_signal = if nowait {
                        data.peek_stop_status()
                    } else {
                        data.take_stop_status()
                    };
                    if let Some(stop_signal) = stop_signal {
                        let status = ((stop_signal as i32) << 8) | 0x7f;
                        if let Some(exit_code) = exit_code.nullable() {
                            exit_code.vm_write(status)?;
                        }
                        return Ok(Some(child.pid() as _));
                    }
                }
            }
        }

        // Check for continued children (WCONTINUED).
        if options.contains(WaitOptions::WCONTINUED) {
            for child in children.iter() {
                if let Ok(data) = get_process_data(child.pid()) {
                    let continued = if nowait {
                        data.peek_continued()
                    } else {
                        data.take_continued()
                    };
                    if continued {
                        if let Some(exit_code) = exit_code.nullable() {
                            exit_code.vm_write(0xffffi32)?;
                        }
                        return Ok(Some(child.pid() as _));
                    }
                }
            }
        }

        // Check for exited (zombie) children.
        if let Some(child) = children.iter().find(|child| child.is_zombie()) {
            if !nowait {
                child.free();
                remove_cached_exit_signal(child.pid());
            }
            if let Some(exit_code) = exit_code.nullable() {
                exit_code.vm_write(child.exit_code())?;
            }
            // Write zeroed rusage for now — zombie thread data is already released.
            // TODO: accumulate rusage in ProcessData during do_exit for proper accounting.
            if let Some(rusage_ptr) = rusage_ptr.nullable() {
                rusage_ptr.vm_write(unsafe { core::mem::zeroed::<rusage>() })?;
            }
            Ok(Some(child.pid() as _))
        } else if options.contains(WaitOptions::WNOHANG) {
            Ok(Some(0))
        } else {
            Ok(None)
        }
    };

    block_on(poll_fn(|cx| {
        // 1. Always check children first — prioritize child status over signals.
        if let Some(res) = check_children().transpose() {
            return Poll::Ready(res);
        }

        // 2. Check for signal interruption — only after confirming no child status.
        //    poll_interrupt also registers a waker for future interrupts when returning
        //    Pending, so the task will be woken by either signals or child events.
        if curr.poll_interrupt(cx).is_ready() {
            // Re-check after consuming the signal flag (race between signal and status).
            if let Some(res) = check_children().transpose() {
                return Poll::Ready(res);
            }
            return Poll::Ready(Err(AxError::Interrupted));
        }

        // 3. Register waker for child events and re-check (prevent race between
        //    the check above and the waker registration).
        proc_data.child_exit_event.register(cx.waker());
        if let Some(res) = check_children().transpose() {
            Poll::Ready(res)
        } else {
            Poll::Pending
        }
    }))
}

/// Decodes a Linux-style wait status into (CLD_* code, si_status) for waitid.
fn decode_exit_code(exit_code: i32) -> (u32, i32) {
    if exit_code & 0x7f == 0 {
        // Normal exit: status in upper byte.
        (CLD_EXITED, (exit_code >> 8) & 0xff)
    } else if exit_code & 0x80 != 0 {
        // Core dump: signal in low 7 bits.
        (CLD_DUMPED, exit_code & 0x7f)
    } else {
        // Killed by signal: signal in low 7 bits.
        (CLD_KILLED, exit_code & 0x7f)
    }
}

/// Fills a siginfo_t struct for waitid.
fn fill_siginfo(pid: Pid, si_code: u32, si_status: i32) -> siginfo {
    let mut info: siginfo = unsafe { core::mem::zeroed() };
    unsafe {
        let inner = &mut info.__bindgen_anon_1.__bindgen_anon_1;
        inner.si_signo = SIGCHLD as i32;
        inner.si_code = si_code as i32;
        inner._sifields._sigchld._pid = pid as _;
        inner._sifields._sigchld._uid = 0;
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
    let options = WaitOptions::from_bits_truncate(options);
    info!("sys_waitid <= idtype: {idtype}, id: {id}, options: {options:?}");

    // waitid requires at least one of WEXITED, WSTOPPED (WUNTRACED), or WCONTINUED.
    if !options.intersects(
        WaitOptions::WEXITED | WaitOptions::WUNTRACED | WaitOptions::WCONTINUED,
    ) {
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
        _ => return Err(AxError::InvalidInput), // P_PIDFD not supported
    };

    let check_children = || -> AxResult<Option<isize>> {
        let children = proc
            .children()
            .into_iter()
            .filter(|child| pid.apply(child) && should_wait_for_child(child, &options))
            .collect::<Vec<_>>();
        if children.is_empty() {
            return Err(AxError::from(LinuxError::ECHILD));
        }

        // Check for stopped children (WUNTRACED / WSTOPPED).
        if options.contains(WaitOptions::WUNTRACED) {
            for child in children.iter() {
                if let Ok(data) = get_process_data(child.pid()) {
                    let stop_signal = if nowait {
                        data.peek_stop_status()
                    } else {
                        data.take_stop_status()
                    };
                    if let Some(stop_signal) = stop_signal {
                        let info = fill_siginfo(child.pid(), CLD_STOPPED, stop_signal as i32);
                        if let Some(infop) = infop.nullable() {
                            infop.vm_write(info)?;
                        }
                        return Ok(Some(0));
                    }
                }
            }
        }

        // Check for continued children (WCONTINUED).
        if options.contains(WaitOptions::WCONTINUED) {
            for child in children.iter() {
                if let Ok(data) = get_process_data(child.pid()) {
                    let continued = if nowait {
                        data.peek_continued()
                    } else {
                        data.take_continued()
                    };
                    if continued {
                        let info = fill_siginfo(child.pid(), CLD_CONTINUED, SIGCONT as i32);
                        if let Some(infop) = infop.nullable() {
                            infop.vm_write(info)?;
                        }
                        return Ok(Some(0));
                    }
                }
            }
        }

        // Check for exited (zombie) children (WEXITED).
        if options.contains(WaitOptions::WEXITED) {
            if let Some(child) = children.iter().find(|child| child.is_zombie()) {
                let (si_code, si_status) = decode_exit_code(child.exit_code());
                let info = fill_siginfo(child.pid(), si_code, si_status);
                if !nowait {
                    child.free();
                    remove_cached_exit_signal(child.pid());
                }
                if let Some(infop) = infop.nullable() {
                    infop.vm_write(info)?;
                }
                if let Some(rusage_ptr) = rusage_ptr.nullable() {
                    rusage_ptr.vm_write(unsafe { core::mem::zeroed::<rusage>() })?;
                }
                return Ok(Some(0));
            }
        }

        if options.contains(WaitOptions::WNOHANG) {
            // WNOHANG with no status: zero out siginfo and return 0.
            if let Some(infop) = infop.nullable() {
                infop.vm_write(unsafe { core::mem::zeroed::<siginfo>() })?;
            }
            Ok(Some(0))
        } else {
            Ok(None)
        }
    };

    block_on(poll_fn(|cx| {
        if let Some(res) = check_children().transpose() {
            return Poll::Ready(res);
        }

        if curr.poll_interrupt(cx).is_ready() {
            if let Some(res) = check_children().transpose() {
                return Poll::Ready(res);
            }
            return Poll::Ready(Err(AxError::Interrupted));
        }

        proc_data.child_exit_event.register(cx.waker());
        if let Some(res) = check_children().transpose() {
            Poll::Ready(res)
        } else {
            Poll::Pending
        }
    }))
}
