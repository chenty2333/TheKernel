#[cfg(feature = "bpf")]
mod bpf;
mod fs;
mod io_mpx;
mod ipc;
mod mm;
mod net;
mod resources;
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

pub use self::{
    fs::*, io_mpx::*, ipc::*, mm::*, net::*, resources::*, signal::*, sync::*, sys::*, task::*,
    time::*,
};
use crate::{
    file::{FileLike, Socket},
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
        Sysno::open => Some(RestartClass::Sys),
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
        #[cfg(target_arch = "loongarch64")]
        Sysno::recvmmsg_time64 => {
            restart_class_for_fd_io(uctx.arg0() as i32, SocketIoDirection::Read)
        }
        Sysno::connect | Sysno::sendto | Sysno::sendmsg | Sysno::sendmmsg => {
            restart_class_for_fd_io(uctx.arg0() as i32, SocketIoDirection::Write)
        }
        Sysno::futex => restart_class_for_futex(uctx),
        #[cfg(target_arch = "loongarch64")]
        Sysno::futex_time64 => restart_class_for_futex(uctx),
        Sysno::futex_waitv => Some(RestartClass::Sys),
        _ => None,
    }
}

fn maybe_request_syscall_restart(thr: &Thread, result: &Result<isize, AxError>) {
    if !matches!(result, Err(AxError::Interrupted)) || !has_pending_syscall_signal(thr) {
        return;
    }
    thr.request_syscall_restart();
}

/// Fast path for trivial getter syscalls that only read from the current
/// thread and return a scalar: no user copy, no restart, no blocking, no
/// timer accounting. Handling them here skips the per-syscall
/// enter_syscall/set_timer_state/restart bookkeeping, cutting the null-syscall
/// latency that lmbench measures. Signal delivery and preemption run in the
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
    // bookkeeping (the dominant cyclictest per-cycle cost). clock_gettime is
    // gated to the non-CPUTIME clocks; CPUTIME clocks fall through to the full
    // path for accurate CPU-time accounting. Signal delivery and preemption
    // still run in the user-mode loop after handle_syscall returns.
    match sysno {
        Sysno::gettimeofday => {
            let r = sys_gettimeofday(uctx.arg0() as _, uctx.arg1() as _);
            uctx.set_retval(r.unwrap_or_else(|err| -LinuxError::from(err).code() as _) as _);
            return;
        }
        Sysno::clock_gettime => {
            let clockid = uctx.arg0() as u32;
            if !matches!(clockid, CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID) {
                let r = sys_clock_gettime(uctx.arg0() as _, uctx.arg1() as _);
                uctx.set_retval(r.unwrap_or_else(|err| -LinuxError::from(err).code() as _) as _);
                return;
            }
        }
        #[cfg(target_arch = "loongarch64")]
        Sysno::clock_gettime64 => {
            let clockid = uctx.arg0() as u32;
            if !matches!(clockid, CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID) {
                let r = sys_clock_gettime(uctx.arg0() as _, uctx.arg1() as _);
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

    let result = match sysno {
        Sysno::restart_syscall => sys_restart_syscall(uctx),
        // fs ctl
        Sysno::ioctl => sys_ioctl(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::chdir => sys_chdir(uctx.arg0() as _),
        Sysno::fchdir => sys_fchdir(uctx.arg0() as _),
        Sysno::chroot => sys_chroot(uctx.arg0() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::mkdir => sys_mkdir(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::mkdirat => sys_mkdirat(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::getdents64 => sys_getdents64(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::link => sys_link(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::linkat => sys_linkat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        #[cfg(target_arch = "x86_64")]
        Sysno::rmdir => sys_rmdir(uctx.arg0() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::unlink => sys_unlink(uctx.arg0() as _),
        Sysno::unlinkat => sys_unlinkat(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::getcwd => sys_getcwd(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::symlink => sys_symlink(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::symlinkat => sys_symlinkat(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::rename => sys_rename(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(not(target_arch = "riscv64"))]
        Sysno::renameat => sys_renameat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::renameat2 => sys_renameat2(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::sync => sys_sync(),
        Sysno::syncfs => sys_syncfs(uctx.arg0() as _),
        Sysno::reboot => sys_reboot(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::vhangup => sys_vhangup(),
        Sysno::fsopen => sys_fsopen(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fsconfig => sys_fsconfig(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::fsmount => sys_fsmount(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::move_mount => sys_move_mount(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::mount_setattr => sys_mount_setattr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::open_tree => sys_open_tree(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::fspick => sys_fspick(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::quotactl => sys_quotactl(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::quotactl_fd => sys_quotactl_fd(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        // file ops
        #[cfg(target_arch = "x86_64")]
        Sysno::chown => sys_chown(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::lchown => sys_lchown(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::fchown => sys_fchown(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::fchownat => sys_fchownat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        #[cfg(target_arch = "x86_64")]
        Sysno::chmod => sys_chmod(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fchmod => sys_fchmod(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fchmodat => sys_fchmodat(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _, 0),
        Sysno::fchmodat2 => sys_fchmodat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::add_key => sys_add_key(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::request_key => sys_request_key(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::keyctl => sys_keyctl(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::openat2 => sys_openat2(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2().into(),
            uctx.arg3() as _,
        ),
        Sysno::setxattr => sys_setxattr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::lsetxattr => sys_lsetxattr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::fsetxattr => sys_fsetxattr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::getxattr => sys_getxattr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::lgetxattr => sys_lgetxattr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::fgetxattr => sys_fgetxattr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::listxattr => sys_listxattr(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::llistxattr => sys_llistxattr(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::flistxattr => sys_flistxattr(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::removexattr => sys_removexattr(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::lremovexattr => sys_lremovexattr(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fremovexattr => sys_fremovexattr(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::readlink => sys_readlink(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::readlinkat => sys_readlinkat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        #[cfg(target_arch = "x86_64")]
        Sysno::utime => sys_utime(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::utimes => sys_utimes(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::utimensat => sys_utimensat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::utimensat_time64 => sys_utimensat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::mknodat => sys_mknodat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        // fd ops
        #[cfg(target_arch = "x86_64")]
        Sysno::open => sys_open(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::openat => sys_openat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::name_to_handle_at => sys_name_to_handle_at(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4() as _,
        ),
        Sysno::open_by_handle_at => {
            sys_open_by_handle_at(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2() as _)
        }
        Sysno::close => sys_close(uctx.arg0() as _),
        Sysno::close_range => sys_close_range(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::dup => sys_dup(uctx.arg0() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::dup2 => sys_dup2(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::dup3 => sys_dup3(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::fcntl => sys_fcntl(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::flock => sys_flock(uctx.arg0() as _, uctx.arg1() as _),

        // io
        Sysno::read => sys_read(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::readv => sys_readv(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::readahead => sys_readahead(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::write => sys_write(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::writev => sys_writev(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::lseek => sys_lseek(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::truncate => sys_truncate(uctx.arg0().into(), uctx.arg1() as _),
        Sysno::ftruncate => sys_ftruncate(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fallocate => sys_fallocate(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::fsync => sys_fsync(uctx.arg0() as _),
        Sysno::fdatasync => sys_fdatasync(uctx.arg0() as _),
        Sysno::sync_file_range => sys_sync_file_range(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::fadvise64 => sys_fadvise64(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::pread64 => sys_pread64(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::pwrite64 => sys_pwrite64(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::preadv => sys_preadv(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::pwritev => sys_pwritev(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::preadv2 => sys_preadv2(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::pwritev2 => sys_pwritev2(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::io_setup => sys_io_setup(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::io_destroy => sys_io_destroy(uctx.arg0() as _),
        Sysno::io_submit => sys_io_submit(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::io_cancel => sys_io_cancel(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::io_getevents => sys_io_getevents(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::mq_open => sys_mq_open(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::mq_unlink => sys_mq_unlink(uctx.arg0() as _),
        Sysno::mq_timedsend => sys_mq_timedsend(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::mq_timedreceive => sys_mq_timedreceive(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::mq_notify => sys_mq_notify(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::mq_getsetattr => {
            sys_mq_getsetattr(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        #[cfg(target_arch = "loongarch64")]
        Sysno::mq_timedsend_time64 => sys_mq_timedsend(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::mq_timedreceive_time64 => sys_mq_timedreceive(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::sendfile => sys_sendfile(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::copy_file_range => sys_copy_file_range(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::splice => sys_splice(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::tee => sys_tee(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::vmsplice => sys_vmsplice(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        // io mpx
        #[cfg(target_arch = "x86_64")]
        Sysno::poll => sys_poll(uctx.arg0().into(), uctx.arg1() as _, uctx.arg2() as _),
        Sysno::ppoll => sys_ppoll(
            uctx,
            uctx.arg0().into(),
            uctx.arg1() as _,
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::ppoll_time64 => sys_ppoll(
            uctx,
            uctx.arg0().into(),
            uctx.arg1() as _,
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4() as _,
        ),
        #[cfg(target_arch = "x86_64")]
        Sysno::select => sys_select(
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4().into(),
        ),
        Sysno::pselect6 => sys_pselect6(
            uctx,
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4().into(),
            uctx.arg5().into(),
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::pselect6_time64 => sys_pselect6(
            uctx,
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4().into(),
            uctx.arg5().into(),
        ),
        Sysno::epoll_create1 => sys_epoll_create1(uctx.arg0() as _),
        Sysno::epoll_ctl => sys_epoll_ctl(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3().into(),
        ),
        Sysno::epoll_pwait => sys_epoll_pwait(
            uctx,
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4().into(),
            uctx.arg5() as _,
        ),
        Sysno::epoll_pwait2 => sys_epoll_pwait2(
            uctx,
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
            uctx.arg3().into(),
            uctx.arg4().into(),
            uctx.arg5() as _,
        ),

        // fs mount
        Sysno::mount => sys_mount(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ) as _,
        Sysno::umount2 => sys_umount2(uctx.arg0() as _, uctx.arg1() as _) as _,

        // pipe
        Sysno::pipe2 => sys_pipe2(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::pipe => sys_pipe2(uctx.arg0() as _, 0),

        // event
        Sysno::eventfd2 => sys_eventfd2(uctx.arg0() as _, uctx.arg1() as _),

        // pidfd
        Sysno::pidfd_open => sys_pidfd_open(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::pidfd_getfd => sys_pidfd_getfd(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::pidfd_send_signal => sys_pidfd_send_signal(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        // memfd
        Sysno::memfd_create => sys_memfd_create(uctx.arg0().into(), uctx.arg1() as _),

        // fs stat
        #[cfg(target_arch = "x86_64")]
        Sysno::stat => sys_stat(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fstat => sys_fstat(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::lstat => sys_lstat(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(any(target_arch = "x86_64", target_arch = "riscv64"))]
        Sysno::newfstatat => sys_fstatat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        #[cfg(not(any(target_arch = "x86_64", target_arch = "riscv64")))]
        Sysno::fstatat => sys_fstatat(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::statx => sys_statx(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        #[cfg(target_arch = "x86_64")]
        Sysno::access => sys_access(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::faccessat => sys_faccessat(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::faccessat2 => sys_faccessat2(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::statfs => sys_statfs(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fstatfs => sys_fstatfs(uctx.arg0() as _, uctx.arg1() as _),

        // mm
        Sysno::brk => sys_brk(uctx.arg0() as _),
        Sysno::mmap => sys_mmap(
            uctx.arg0(),
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::munmap => sys_munmap(uctx.arg0(), uctx.arg1() as _),
        Sysno::mprotect => sys_mprotect(uctx.arg0(), uctx.arg1() as _, uctx.arg2() as _),
        Sysno::mincore => sys_mincore(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::mremap => sys_mremap(
            uctx.arg0(),
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::process_vm_readv => sys_process_vm_readv(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::process_vm_writev => sys_process_vm_writev(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::process_madvise => sys_process_madvise(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::madvise => sys_madvise(uctx.arg0(), uctx.arg1() as _, uctx.arg2() as _),
        Sysno::msync => sys_msync(uctx.arg0(), uctx.arg1() as _, uctx.arg2() as _),
        Sysno::mlock => sys_mlock(uctx.arg0(), uctx.arg1() as _),
        Sysno::mlock2 => sys_mlock2(uctx.arg0(), uctx.arg1() as _, uctx.arg2() as _),
        Sysno::munlock => sys_munlock(uctx.arg0(), uctx.arg1() as _),
        Sysno::mlockall => sys_mlockall(uctx.arg0() as _),
        Sysno::munlockall => sys_munlockall(),
        Sysno::swapon => sys_swapon(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::swapoff => sys_swapoff(uctx.arg0() as _),

        // task info
        Sysno::getpid => sys_getpid(),
        Sysno::getppid => sys_getppid(),
        Sysno::gettid => sys_gettid(),
        Sysno::getcpu => sys_getcpu(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::getrusage => sys_getrusage(uctx.arg0() as _, uctx.arg1() as _),

        // task sched
        Sysno::sched_yield => sys_sched_yield(),
        Sysno::nanosleep => sys_nanosleep(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::clock_nanosleep => sys_clock_nanosleep(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::clock_nanosleep_time64 => sys_clock_nanosleep(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::sched_getaffinity => {
            sys_sched_getaffinity(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::sched_setaffinity => {
            sys_sched_setaffinity(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::sched_getscheduler => sys_sched_getscheduler(uctx.arg0() as _),
        Sysno::sched_setparam => sys_sched_setparam(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::sched_setscheduler => {
            sys_sched_setscheduler(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::sched_getparam => sys_sched_getparam(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::sched_get_priority_max => sys_sched_get_priority_max(uctx.arg0() as _),
        Sysno::sched_get_priority_min => sys_sched_get_priority_min(uctx.arg0() as _),
        Sysno::sched_rr_get_interval => {
            sys_sched_rr_get_interval(uctx.arg0() as _, uctx.arg1() as _)
        }
        #[cfg(target_arch = "loongarch64")]
        Sysno::sched_rr_get_interval_time64 => {
            sys_sched_rr_get_interval(uctx.arg0() as _, uctx.arg1() as _)
        }
        Sysno::sched_setattr => {
            sys_sched_setattr(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::sched_getattr => sys_sched_getattr(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::getpriority => sys_getpriority(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setpriority => sys_setpriority(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::ioprio_get => sys_ioprio_get(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::ioprio_set => sys_ioprio_set(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),

        // task ops
        Sysno::execve => sys_execve(uctx, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::execveat => sys_execveat(
            uctx,
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::init_module => sys_init_module(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::finit_module => {
            sys_finit_module(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::delete_module => sys_delete_module(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::set_tid_address => sys_set_tid_address(uctx.arg0()),
        #[cfg(target_arch = "x86_64")]
        Sysno::arch_prctl => sys_arch_prctl(uctx, uctx.arg0() as _, uctx.arg1() as _),
        Sysno::prctl => sys_prctl(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::prlimit64 => sys_prlimit64(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::getrlimit => sys_getrlimit(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setrlimit => sys_setrlimit(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::capget => sys_capget(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::capset => sys_capset(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::umask => sys_umask(uctx.arg0() as _),
        Sysno::setreuid => sys_setreuid(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setregid => sys_setregid(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setresuid => sys_setresuid(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::setresgid => sys_setresgid(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::get_mempolicy => sys_get_mempolicy(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::mbind => sys_mbind(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::migrate_pages => sys_migrate_pages(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::move_pages => sys_move_pages(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::kcmp => sys_kcmp(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::set_mempolicy => {
            sys_set_mempolicy(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }

        // task management
        Sysno::clone => sys_clone(
            uctx,
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2(),
            uctx.arg3(),
            uctx.arg4(),
        ),
        Sysno::clone3 => sys_clone3(
            uctx,
            uctx.arg0() as _, // args_ptr
            uctx.arg1() as _, // args_size
        ),
        Sysno::unshare => sys_unshare(uctx.arg0() as _),
        Sysno::setns => sys_setns(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "x86_64")]
        Sysno::fork => sys_fork(uctx),
        Sysno::exit => sys_exit(uctx.arg0() as _),
        Sysno::exit_group => sys_exit_group(uctx.arg0() as _),
        Sysno::wait4 => sys_waitpid(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::waitid => sys_waitid(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::ptrace => sys_ptrace(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::getsid => sys_getsid(uctx.arg0() as _),
        Sysno::setsid => sys_setsid(),
        Sysno::getpgid => sys_getpgid(uctx.arg0() as _),
        Sysno::setpgid => sys_setpgid(uctx.arg0() as i32, uctx.arg1() as i32),
        Sysno::acct => sys_acct(uctx.arg0() as _),

        // signal
        Sysno::rt_sigprocmask => sys_rt_sigprocmask(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::rt_sigaction => sys_rt_sigaction(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::rt_sigpending => sys_rt_sigpending(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::rt_sigreturn => sys_rt_sigreturn(uctx),
        Sysno::rt_sigtimedwait => sys_rt_sigtimedwait(
            uctx,
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::rt_sigtimedwait_time64 => sys_rt_sigtimedwait(
            uctx,
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::rt_sigsuspend => sys_rt_sigsuspend(uctx, uctx.arg0() as _, uctx.arg1() as _),
        Sysno::kill => sys_kill(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::tkill => sys_tkill(uctx.arg0() as i32, uctx.arg1() as _),
        Sysno::tgkill => sys_tgkill(uctx.arg0() as i32, uctx.arg1() as i32, uctx.arg2() as _),
        Sysno::rt_sigqueueinfo => {
            sys_rt_sigqueueinfo(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::rt_tgsigqueueinfo => sys_rt_tgsigqueueinfo(
            uctx.arg0() as i32,
            uctx.arg1() as i32,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::sigaltstack => sys_sigaltstack(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::futex => sys_futex(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::futex_time64 => sys_futex(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::futex_waitv => sys_futex_waitv(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::get_robust_list => {
            sys_get_robust_list(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::set_robust_list => sys_set_robust_list(uctx.arg0() as _, uctx.arg1() as _),

        // sys
        Sysno::getuid => sys_getuid(),
        Sysno::geteuid => sys_geteuid(),
        Sysno::getresuid => sys_getresuid(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::getgid => sys_getgid(),
        Sysno::getegid => sys_getegid(),
        Sysno::getresgid => sys_getresgid(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::setuid => sys_setuid(uctx.arg0() as _),
        Sysno::setgid => sys_setgid(uctx.arg0() as _),
        Sysno::setfsuid => sys_setfsuid(uctx.arg0() as _),
        Sysno::setfsgid => sys_setfsgid(uctx.arg0() as _),
        Sysno::getgroups => sys_getgroups(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setgroups => sys_setgroups(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::sethostname => sys_sethostname(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setdomainname => sys_setdomainname(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::uname => sys_uname(uctx.arg0() as _),
        Sysno::personality => sys_personality(uctx.arg0() as _),
        Sysno::sysinfo => sys_sysinfo(uctx.arg0() as _),
        Sysno::syslog => sys_syslog(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as isize),
        Sysno::getrandom => sys_getrandom(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::seccomp => sys_seccomp(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        #[cfg(target_arch = "riscv64")]
        Sysno::riscv_hwprobe => sys_riscv_hwprobe(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        #[cfg(target_arch = "riscv64")]
        Sysno::riscv_flush_icache => sys_riscv_flush_icache(),

        // sync
        Sysno::membarrier => sys_membarrier(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),

        // time
        Sysno::gettimeofday => sys_gettimeofday(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::clock_settime => sys_clock_settime(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::settimeofday => sys_settimeofday(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::adjtimex => sys_adjtimex(uctx.arg0() as _),
        Sysno::clock_adjtime => sys_clock_adjtime(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::times => sys_times(uctx.arg0() as _),
        Sysno::clock_gettime => sys_clock_gettime(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::clock_getres => sys_clock_getres(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "loongarch64")]
        Sysno::clock_settime64 => sys_clock_settime(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "loongarch64")]
        Sysno::clock_adjtime64 => sys_clock_adjtime(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "loongarch64")]
        Sysno::clock_gettime64 => sys_clock_gettime(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "loongarch64")]
        Sysno::clock_getres_time64 => sys_clock_getres(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::getitimer => sys_getitimer(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setitimer => sys_setitimer(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),

        // msg
        Sysno::msgget => sys_msgget(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::msgsnd => sys_msgsnd(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::msgrcv => sys_msgrcv(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::msgctl => sys_msgctl(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),

        // shm
        Sysno::shmget => sys_shmget(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::shmat => sys_shmat(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::shmctl => sys_shmctl(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2().into()),
        Sysno::shmdt => sys_shmdt(uctx.arg0() as _),

        // sem
        Sysno::semget => sys_semget(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::semctl => sys_semctl(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::semop => sys_semop(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::semtimedop => sys_semtimedop(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::semtimedop_time64 => sys_semtimedop(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        // net
        Sysno::socket => sys_socket(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::socketpair => sys_socketpair(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3().into(),
        ),
        Sysno::bind => sys_bind(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2() as _),
        Sysno::connect => sys_connect(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2() as _),
        Sysno::getsockname => {
            sys_getsockname(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2().into())
        }
        Sysno::getpeername => {
            sys_getpeername(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2().into())
        }
        Sysno::listen => sys_listen(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::accept => sys_accept(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2().into()),
        Sysno::accept4 => sys_accept4(
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
            uctx.arg3() as _,
        ),
        Sysno::shutdown => sys_shutdown(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::sendto => sys_sendto(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4().into(),
            uctx.arg5() as _,
        ),
        Sysno::recvfrom => sys_recvfrom(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4().into(),
            uctx.arg5().into(),
        ),
        Sysno::sendmsg => sys_sendmsg(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2() as _),
        Sysno::recvmsg => sys_recvmsg(uctx.arg0() as _, uctx.arg1().into(), uctx.arg2() as _),
        Sysno::sendmmsg => sys_sendmmsg(
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::recvmmsg => sys_recvmmsg(
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4().into(),
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::recvmmsg_time64 => sys_recvmmsg(
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4().into(),
        ),
        Sysno::getsockopt => sys_getsockopt(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3().into(),
            uctx.arg4().into(),
        ),
        Sysno::setsockopt => sys_setsockopt(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3().into(),
            uctx.arg4() as _,
        ),

        // signal file descriptors
        Sysno::signalfd4 => sys_signalfd4(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2(),
            uctx.arg3() as _,
        ),

        // timerfd
        Sysno::timerfd_create => sys_timerfd_create(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::timerfd_settime => sys_timerfd_settime(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::timerfd_gettime => sys_timerfd_gettime(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "loongarch64")]
        Sysno::timerfd_settime64 => sys_timerfd_settime(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::timerfd_gettime64 => sys_timerfd_gettime(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::userfaultfd => sys_userfaultfd(uctx.arg0() as _),
        Sysno::io_uring_setup => sys_io_uring_setup(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::io_uring_enter => sys_io_uring_enter(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::io_uring_register => sys_io_uring_register(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2(),
            uctx.arg3() as _,
        ),
        Sysno::io_pgetevents => sys_io_pgetevents(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::io_pgetevents_time64 => sys_io_pgetevents(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::inotify_init1 => sys_inotify_init1(uctx.arg0() as _),
        Sysno::inotify_add_watch => {
            sys_inotify_add_watch(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::inotify_rm_watch => sys_inotify_rm_watch(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fanotify_init => sys_fanotify_init(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fanotify_mark => sys_fanotify_mark(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),

        // bpf
        #[cfg(feature = "bpf")]
        Sysno::bpf => bpf::sys_bpf(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),

        // dummy fds
        Sysno::perf_event_open | Sysno::memfd_secret => sys_dummy_fd(sysno),

        Sysno::timer_create => {
            sys_timer_create(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::timer_gettime => sys_timer_gettime(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::timer_getoverrun => sys_timer_getoverrun(uctx.arg0() as _),
        Sysno::timer_settime => sys_timer_settime(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        #[cfg(target_arch = "loongarch64")]
        Sysno::timer_gettime64 => sys_timer_gettime(uctx.arg0() as _, uctx.arg1() as _),
        #[cfg(target_arch = "loongarch64")]
        Sysno::timer_settime64 => sys_timer_settime(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::timer_delete => sys_timer_delete(uctx.arg0() as _),

        _ => {
            debug!("Unimplemented syscall: {sysno}");
            Err(AxError::Unsupported)
        }
    };
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
