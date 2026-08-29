#[cfg(feature = "bpf")]
mod bpf;
mod dispatch;
mod fs;
mod io_mpx;
mod ipc;
mod mm;
mod net;
mod resources;
mod seccomp;
mod signal;
mod sync;
mod sys;
mod task;
mod time;

use core::time::Duration;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::uspace::UserContext;
use axnet::options::{Configurable, GetSocketOption};
use axtask::current;
use linux_raw_sys::general::{
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID, FUTEX_CMD_MASK, FUTEX_WAIT,
    FUTEX_WAIT_BITSET,
};
use syscalls::Sysno;
pub(crate) use thekernel_linux_usercopy::RawSigevent;

pub(crate) use self::sync::init_membarrier_ipi;
pub use self::{
    fs::*, io_mpx::*, ipc::*, mm::*, net::*, resources::*, seccomp::*, signal::*, sync::*, sys::*,
    task::*, time::*,
};
use crate::{
    file::{FileLike, Socket},
    mm::with_user_memory,
    task::{AsThread, RestartClass, Thread, has_pending_syscall_signal},
};

#[derive(Clone, Copy)]
enum SocketIoDirection {
    Read,
    Write,
}

fn socket_timeout_configured(fd: i32, direction: SocketIoDirection) -> bool {
    let Ok(socket) = Socket::from_fd(fd) else {
        return false;
    };

    let mut timeout = Duration::ZERO;
    let result = match direction {
        SocketIoDirection::Read => socket
            .inner
            .get_option(GetSocketOption::ReceiveTimeout(&mut timeout)),
        SocketIoDirection::Write => socket
            .inner
            .get_option(GetSocketOption::SendTimeout(&mut timeout)),
    };
    result.is_ok() && timeout != Duration::ZERO
}

fn restart_class_for_fd_io(fd: i32, direction: SocketIoDirection) -> Option<RestartClass> {
    (!socket_timeout_configured(fd, direction)).then_some(RestartClass::Sys)
}

fn restart_class_for_futex(uctx: &UserContext) -> Option<RestartClass> {
    let futex_op = uctx.arg1() as u32 & FUTEX_CMD_MASK as u32;
    if matches!(futex_op, FUTEX_WAIT | FUTEX_WAIT_BITSET) {
        Some(RestartClass::Sys)
    } else {
        None
    }
}

fn restart_class_for_syscall(sysno: Sysno, uctx: &UserContext) -> Option<RestartClass> {
    match sysno {
        Sysno::ioctl
        | Sysno::openat
        | Sysno::openat2
        | Sysno::wait4
        | Sysno::waitid
        | Sysno::flock => Some(RestartClass::Sys),
        #[cfg(target_arch = "x86_64")]
        Sysno::open | Sysno::creat => Some(RestartClass::Sys),
        Sysno::read | Sysno::readv => {
            restart_class_for_fd_io(uctx.arg0() as i32, SocketIoDirection::Read)
        }
        Sysno::readahead => Some(RestartClass::Sys),
        Sysno::write | Sysno::writev => {
            restart_class_for_fd_io(uctx.arg0() as i32, SocketIoDirection::Write)
        }
        Sysno::accept | Sysno::accept4 | Sysno::recvfrom | Sysno::recvmsg | Sysno::recvmmsg => {
            restart_class_for_fd_io(uctx.arg0() as i32, SocketIoDirection::Read)
        }
        Sysno::connect | Sysno::sendto | Sysno::sendmsg | Sysno::sendmmsg => {
            restart_class_for_fd_io(uctx.arg0() as i32, SocketIoDirection::Write)
        }
        Sysno::futex => restart_class_for_futex(uctx),
        Sysno::futex_waitv => Some(RestartClass::Sys),
        #[cfg(target_arch = "x86_64")]
        Sysno::futex_wait => Some(RestartClass::Sys),
        _ => None,
    }
}

fn maybe_request_syscall_restart(thr: &Thread, result: &Result<isize, AxError>) {
    if !matches!(result, Err(AxError::Interrupted)) || !has_pending_syscall_signal(thr) {
        return;
    }
    // A full bounded restart ledger intentionally leaves this interruption as
    // EINTR; signal delivery itself must still proceed.
    let _ = thr.request_syscall_restart();
}

/// Fast path for trivial getter syscalls that only read from the current
/// thread and return a scalar: no user copy, no restart, no blocking, no
/// timer accounting. Handling them here skips the per-syscall
/// enter_syscall/set_timer_state/restart bookkeeping, reducing common getter
/// latency. Signal delivery and preemption run in the
/// user-mode loop after handle_syscall returns, so they remain correct.
fn fast_path_getter(sysno: Sysno) -> Option<AxResult<isize>> {
    Some(match sysno {
        Sysno::getpid => sys_getpid(),
        Sysno::getppid => sys_getppid(),
        Sysno::gettid => sys_gettid(),
        Sysno::getuid => sys_getuid(),
        Sysno::geteuid => sys_geteuid(),
        Sysno::getgid => sys_getgid(),
        Sysno::getegid => sys_getegid(),
        _ => return None,
    })
}

pub fn handle_syscall(uctx: &mut UserContext) {
    // Seccomp observes the raw syscall register before generic decoding and
    // before every scalar/time fast path. This ordering is ABI-significant:
    // filters may reject unknown numbers and otherwise-fast syscalls.
    if !seccomp::enforce_syscall_seccomp(uctx) {
        return;
    }

    let Some(sysno) = Sysno::new(uctx.sysno()) else {
        warn!("Invalid syscall number: {}", uctx.sysno());
        uctx.set_retval(-LinuxError::ENOSYS.code() as _);
        return;
    };

    trace!("Syscall {sysno:?}");

    if let Some(result) = fast_path_getter(sysno) {
        uctx.set_retval(result.unwrap_or_else(|err| -LinuxError::from(err).code() as _) as _);
        return;
    }

    // Fast path for time-read syscalls: they don't block and aren't
    // restartable, so skip the per-syscall set_timer_state/enter_syscall/restart
    // bookkeeping on the nonblocking time-read fast path. clock_gettime is
    // gated to the non-CPUTIME clocks; CPUTIME clocks fall through to the full
    // path for accurate CPU-time accounting. Signal delivery and preemption
    // still run in the user-mode loop after handle_syscall returns.
    match sysno {
        Sysno::gettimeofday => {
            let aspace = current().as_thread().proc_data.aspace();
            let r = with_user_memory(aspace, |memory| {
                sys_gettimeofday(memory, uctx.arg0() as _, uctx.arg1() as _)
            });
            uctx.set_retval(r.unwrap_or_else(|err| -LinuxError::from(err).code() as _) as _);
            return;
        }
        Sysno::clock_gettime => {
            let clockid = uctx.arg0() as u32;
            if !matches!(clockid, CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID) {
                let aspace = current().as_thread().proc_data.aspace();
                let r = with_user_memory(aspace, |memory| {
                    sys_clock_gettime(memory, uctx.arg0() as _, uctx.arg1() as _)
                });
                uctx.set_retval(r.unwrap_or_else(|err| -LinuxError::from(err).code() as _) as _);
                return;
            }
        }
        _ => {}
    }
    let curr = current();
    let thr = curr.as_thread();
    let signal_handler_depth = thr.signal_handler_depth();
    let restart_class = restart_class_for_syscall(sysno, uctx);
    let preserve_restart_state = matches!(sysno, Sysno::rt_sigreturn) || thr.in_signal_handler();
    thr.enter_syscall(uctx, preserve_restart_state, restart_class);

    let result = dispatch::dispatch_syscall(sysno, uctx, || thr.proc_data.aspace());
    // Syscalls such as close, munmap, execve, and umount may release the final
    // filesystem identity. All syscall-local handles have been dropped by this
    // point, and policy work is safe in the current task context.
    axtask::run_deferred_work();
    maybe_request_syscall_restart(thr, &result);
    debug!("Syscall {sysno} return {result:?}");

    if thr.take_resume_restored_context() {
        thr.clear_saved_syscall();
        return;
    }

    if thr.signal_handler_depth() == signal_handler_depth {
        uctx.set_retval(result.unwrap_or_else(|err| -LinuxError::from(err).code() as _) as _);
    }
    thr.clear_saved_syscall();
}
