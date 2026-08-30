use alloc::sync::Arc;
use core::ffi::c_char;

use axerrno::{AxError, AxResult};
use axhal::uspace::UserContext;
use axsync::Mutex;
use linux_raw_sys::general::AT_FDCWD;
use syscalls::Sysno;
use thekernel_linux_signal::SignalSet;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext};

#[cfg(feature = "bpf")]
use super::bpf;
use super::*;
use crate::{
    file::IoctlContext,
    mm::{AddrSpace, UserMemoryCapability, with_user_memory},
};

#[inline]
fn validate_legacy_epoll_create_size(size: i32) -> AxResult<()> {
    (size > 0).then_some(()).ok_or(AxError::InvalidInput)
}

#[inline]
fn compat_epoll_create(size: i32) -> AxResult<isize> {
    validate_legacy_epoll_create_size(size)?;
    sys_epoll_create1(0)
}

#[inline]
fn compat_eventfd(initval: u32) -> AxResult<isize> {
    sys_eventfd2(initval, 0)
}

#[inline]
fn compat_inotify_init() -> AxResult<isize> {
    sys_inotify_init1(0)
}

#[inline]
fn compat_signalfd<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    fd: i32,
    mask: *const SignalSet,
    sigsetsize: usize,
) -> AxResult<isize> {
    sys_signalfd4(memory, fd, mask, sigsetsize, 0)
}

#[inline]
fn legacy_mknod_dev(dev: u64) -> u64 {
    u64::from(dev as u32)
}

#[inline]
fn compat_mknod<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    path: *const c_char,
    mode: u32,
    dev: u64,
) -> AxResult<isize> {
    sys_mknodat(memory, AT_FDCWD, path, mode, legacy_mknod_dev(dev))
}

#[inline]
fn compat_getpgrp() -> AxResult<isize> {
    sys_getpgid(0)
}

#[inline]
fn sys_ni_syscall() -> AxResult<isize> {
    Err(AxError::Unsupported)
}

pub(super) fn dispatch_syscall(
    sysno: Sysno,
    uctx: &mut UserContext,
    aspace: impl FnOnce() -> Arc<Mutex<AddrSpace>>,
) -> AxResult<isize> {
    match sysno {
        Sysno::restart_syscall => sys_restart_syscall(uctx),
        // fs ctl
        Sysno::ioctl => {
            let ioctl_context = IoctlContext::new(aspace());
            sys_ioctl(
                &ioctl_context,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
            )
        }
        Sysno::sysfs => with_user_memory(aspace(), |memory| {
            sys_sysfs(memory, uctx.arg0(), uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::chdir => with_user_memory(aspace(), |memory| sys_chdir(memory, uctx.arg0() as _)),
        Sysno::fchdir => sys_fchdir(uctx.arg0() as _),
        Sysno::chroot => with_user_memory(aspace(), |memory| sys_chroot(memory, uctx.arg0() as _)),
        Sysno::mkdir => with_user_memory(aspace(), |memory| {
            sys_mkdir(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::mkdirat => with_user_memory(aspace(), |memory| {
            sys_mkdirat(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::getdents64 => with_user_memory(aspace(), |memory| {
            sys_getdents64(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::getdents => with_user_memory(aspace(), |memory| {
            sys_getdents(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::link => with_user_memory(aspace(), |memory| {
            sys_link(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::linkat => with_user_memory(aspace(), |memory| {
            sys_linkat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::rmdir => with_user_memory(aspace(), |memory| sys_rmdir(memory, uctx.arg0() as _)),
        Sysno::unlink => with_user_memory(aspace(), |memory| sys_unlink(memory, uctx.arg0() as _)),
        Sysno::unlinkat => with_user_memory(aspace(), |memory| {
            sys_unlinkat(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::getcwd => with_user_memory(aspace(), |memory| {
            sys_getcwd(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::symlink => with_user_memory(aspace(), |memory| {
            sys_symlink(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::symlinkat => with_user_memory(aspace(), |memory| {
            sys_symlinkat(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::rename => with_user_memory(aspace(), |memory| {
            sys_rename(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::renameat => with_user_memory(aspace(), |memory| {
            sys_renameat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::renameat2 => with_user_memory(aspace(), |memory| {
            sys_renameat2(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::sync => sys_sync(),
        Sysno::syncfs => sys_syncfs(uctx.arg0() as _),
        Sysno::reboot => sys_reboot(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::vhangup => sys_vhangup(),
        Sysno::fsopen => with_user_memory(aspace(), |memory| {
            sys_fsopen(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::fsconfig => with_user_memory(aspace(), |memory| {
            sys_fsconfig(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::fsmount => sys_fsmount(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::move_mount => with_user_memory(aspace(), |memory| {
            sys_move_mount(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::mount_setattr => with_user_memory(aspace(), |memory| {
            sys_mount_setattr(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::open_tree => with_user_memory(aspace(), |memory| {
            sys_open_tree(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
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
        Sysno::chown => with_user_memory(aspace(), |memory| {
            sys_chown(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::lchown => with_user_memory(aspace(), |memory| {
            sys_lchown(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::fchown => sys_fchown(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::fchownat => with_user_memory(aspace(), |memory| {
            sys_fchownat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::chmod => with_user_memory(aspace(), |memory| {
            sys_chmod(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::fchmod => sys_fchmod(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fchmodat => with_user_memory(aspace(), |memory| {
            sys_fchmodat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                0,
            )
        }),
        Sysno::fchmodat2 => with_user_memory(aspace(), |memory| {
            sys_fchmodat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::add_key => with_user_memory(aspace(), |memory| {
            sys_add_key(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::request_key => with_user_memory(aspace(), |memory| {
            sys_request_key(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::keyctl => with_user_memory(aspace(), |memory| {
            sys_keyctl(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::openat2 => sys_openat2(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::setxattr => with_user_memory(aspace(), |memory| {
            sys_setxattr(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::lsetxattr => with_user_memory(aspace(), |memory| {
            sys_lsetxattr(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::fsetxattr => with_user_memory(aspace(), |memory| {
            sys_fsetxattr(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::getxattr => with_user_memory(aspace(), |memory| {
            sys_getxattr(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::lgetxattr => with_user_memory(aspace(), |memory| {
            sys_lgetxattr(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::fgetxattr => with_user_memory(aspace(), |memory| {
            sys_fgetxattr(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::listxattr => with_user_memory(aspace(), |memory| {
            sys_listxattr(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::llistxattr => with_user_memory(aspace(), |memory| {
            sys_llistxattr(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::flistxattr => with_user_memory(aspace(), |memory| {
            sys_flistxattr(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::removexattr => with_user_memory(aspace(), |memory| {
            sys_removexattr(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::lremovexattr => with_user_memory(aspace(), |memory| {
            sys_lremovexattr(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::fremovexattr => with_user_memory(aspace(), |memory| {
            sys_fremovexattr(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::readlink => with_user_memory(aspace(), |memory| {
            sys_readlink(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::readlinkat => with_user_memory(aspace(), |memory| {
            sys_readlinkat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::utime => with_user_memory(aspace(), |memory| {
            sys_utime(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::utimes => with_user_memory(aspace(), |memory| {
            sys_utimes(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::futimesat => with_user_memory(aspace(), |memory| {
            sys_futimesat(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::utimensat => with_user_memory(aspace(), |memory| {
            sys_utimensat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::mknodat => with_user_memory(aspace(), |memory| {
            sys_mknodat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::mknod => with_user_memory(aspace(), |memory| {
            compat_mknod(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),

        // fd ops
        Sysno::open => sys_open(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::creat => sys_creat(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
        ),
        Sysno::openat => sys_openat(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::name_to_handle_at => sys_name_to_handle_at(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::open_by_handle_at => sys_open_by_handle_at(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::close => sys_close(uctx.arg0() as _),
        Sysno::close_range => sys_close_range(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::dup => sys_dup(uctx.arg0() as _),
        Sysno::dup2 => sys_dup2(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::dup3 => sys_dup3(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::fcntl => sys_fcntl(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::flock => sys_flock(uctx.arg0() as _, uctx.arg1() as _),

        // io
        Sysno::read => sys_read(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::readv => sys_readv(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::readahead => sys_readahead(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::write => sys_write(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::writev => sys_writev(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::lseek => sys_lseek(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::truncate => sys_truncate(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
        ),
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
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::pwrite64 => sys_pwrite64(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::preadv => sys_preadv(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::pwritev => sys_pwritev(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::preadv2 => sys_preadv2(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::pwritev2 => sys_pwritev2(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::io_setup => with_user_memory(aspace(), |memory| {
            sys_io_setup(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::io_destroy => sys_io_destroy(uctx.arg0() as _),
        Sysno::io_submit => {
            let capability = UserMemoryCapability::new(aspace());
            with_user_memory(capability.clone(), |memory| {
                sys_io_submit(
                    capability,
                    memory,
                    uctx.arg0() as _,
                    uctx.arg1() as _,
                    uctx.arg2() as _,
                )
            })
        }
        Sysno::io_cancel => with_user_memory(aspace(), |memory| {
            sys_io_cancel(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::io_getevents => with_user_memory(aspace(), |memory| {
            sys_io_getevents(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::mq_open => with_user_memory(aspace(), |memory| {
            sys_mq_open(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::mq_unlink => {
            with_user_memory(aspace(), |memory| sys_mq_unlink(memory, uctx.arg0() as _))
        }
        Sysno::mq_timedsend => with_user_memory(aspace(), |memory| {
            sys_mq_timedsend(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::mq_timedreceive => with_user_memory(aspace(), |memory| {
            sys_mq_timedreceive(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::mq_notify => with_user_memory(aspace(), |memory| {
            sys_mq_notify(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::mq_getsetattr => with_user_memory(aspace(), |memory| {
            sys_mq_getsetattr(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::sendfile => sys_sendfile(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::copy_file_range => sys_copy_file_range(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::splice => sys_splice(
            UserMemoryCapability::new(aspace()),
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
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        // io mpx
        Sysno::poll => sys_poll(
            UserMemoryCapability::new(aspace()),
            uctx.arg0().into(),
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::ppoll => sys_ppoll(
            UserMemoryCapability::new(aspace()),
            uctx,
            uctx.arg0().into(),
            uctx.arg1() as _,
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4() as _,
        ),
        Sysno::select => sys_select(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4().into(),
        ),
        Sysno::pselect6 => sys_pselect6(
            UserMemoryCapability::new(aspace()),
            uctx,
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
            uctx.arg3().into(),
            uctx.arg4().into(),
            uctx.arg5().into(),
        ),
        Sysno::epoll_create => compat_epoll_create(uctx.arg0() as _),
        Sysno::epoll_create1 => sys_epoll_create1(uctx.arg0() as _),
        Sysno::epoll_ctl => with_user_memory(aspace(), |memory| {
            sys_epoll_ctl(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::epoll_wait => with_user_memory(aspace(), |memory| {
            sys_epoll_wait(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::epoll_pwait => with_user_memory(aspace(), |memory| {
            sys_epoll_pwait(
                memory,
                uctx,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
                uctx.arg5() as _,
            )
        }),
        Sysno::epoll_pwait2 => with_user_memory(aspace(), |memory| {
            sys_epoll_pwait2(
                memory,
                uctx,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
                uctx.arg5() as _,
            )
        }),

        // fs mount
        Sysno::mount => with_user_memory(aspace(), |memory| {
            sys_mount(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::umount2 => with_user_memory(aspace(), |memory| {
            sys_umount2(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::pivot_root => with_user_memory(aspace(), |memory| {
            sys_pivot_root(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),

        // pipe
        Sysno::pipe2 => with_user_memory(aspace(), |memory| {
            sys_pipe2(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::pipe => with_user_memory(aspace(), |memory| sys_pipe2(memory, uctx.arg0() as _, 0)),

        // event
        Sysno::eventfd => compat_eventfd(uctx.arg0() as _),
        Sysno::eventfd2 => sys_eventfd2(uctx.arg0() as _, uctx.arg1() as _),

        // pidfd
        Sysno::pidfd_open => sys_pidfd_open(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::pidfd_getfd => sys_pidfd_getfd(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::pidfd_send_signal => with_user_memory(aspace(), |memory| {
            sys_pidfd_send_signal(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),

        // memfd
        Sysno::memfd_create => sys_memfd_create(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
        ),

        // fs stat
        Sysno::stat => with_user_memory(aspace(), |memory| {
            sys_stat(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::fstat => with_user_memory(aspace(), |memory| {
            sys_fstat(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::lstat => with_user_memory(aspace(), |memory| {
            sys_lstat(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::newfstatat => with_user_memory(aspace(), |memory| {
            sys_fstatat(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::statx => with_user_memory(aspace(), |memory| {
            sys_statx(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::access => with_user_memory(aspace(), |memory| {
            sys_access(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::faccessat => with_user_memory(aspace(), |memory| {
            sys_faccessat(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::faccessat2 => with_user_memory(aspace(), |memory| {
            sys_faccessat2(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::ustat => with_user_memory(aspace(), |memory| {
            sys_ustat(memory, uctx.arg0() as u64, uctx.arg1() as _)
        }),
        Sysno::statfs => with_user_memory(aspace(), |memory| {
            sys_statfs(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::fstatfs => with_user_memory(aspace(), |memory| {
            sys_fstatfs(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),

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
        Sysno::mseal => sys_mseal(uctx.arg0(), uctx.arg1() as _, uctx.arg2() as _),
        Sysno::mincore => {
            let aspace = aspace();
            with_user_memory(aspace.clone(), |memory| {
                sys_mincore(
                    memory,
                    aspace,
                    uctx.arg0() as _,
                    uctx.arg1() as _,
                    uctx.arg2() as _,
                )
            })
        }
        Sysno::mremap => sys_mremap(
            uctx.arg0(),
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::process_vm_readv => sys_process_vm_readv(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::process_vm_writev => sys_process_vm_writev(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::process_madvise => sys_process_madvise(
            aspace(),
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
        Sysno::getcpu => with_user_memory(aspace(), |memory| {
            sys_getcpu(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::getrusage => with_user_memory(aspace(), |memory| {
            sys_getrusage(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),

        // task sched
        Sysno::sched_yield => sys_sched_yield(),
        Sysno::nanosleep => with_user_memory(aspace(), |memory| {
            sys_nanosleep(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::clock_nanosleep => with_user_memory(aspace(), |memory| {
            sys_clock_nanosleep(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::sched_getaffinity => with_user_memory(aspace(), |memory| {
            sys_sched_getaffinity(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::sched_setaffinity => with_user_memory(aspace(), |memory| {
            sys_sched_setaffinity(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::sched_getscheduler => sys_sched_getscheduler(uctx.arg0() as _),
        Sysno::sched_setparam => with_user_memory(aspace(), |memory| {
            sys_sched_setparam(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::sched_setscheduler => with_user_memory(aspace(), |memory| {
            sys_sched_setscheduler(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::sched_getparam => with_user_memory(aspace(), |memory| {
            sys_sched_getparam(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::sched_get_priority_max => sys_sched_get_priority_max(uctx.arg0() as _),
        Sysno::sched_get_priority_min => sys_sched_get_priority_min(uctx.arg0() as _),
        Sysno::sched_rr_get_interval => with_user_memory(aspace(), |memory| {
            sys_sched_rr_get_interval(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::sched_setattr => with_user_memory(aspace(), |memory| {
            sys_sched_setattr(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::sched_getattr => with_user_memory(aspace(), |memory| {
            sys_sched_getattr(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::getpriority => sys_getpriority(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setpriority => sys_setpriority(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::ioprio_get => sys_ioprio_get(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::ioprio_set => sys_ioprio_set(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::iopl => sys_iopl(uctx.arg0() as u32),
        Sysno::ioperm => sys_ioperm(uctx.arg0(), uctx.arg1(), uctx.arg2() as i32),

        // task ops
        Sysno::execve => {
            // Keep the old image selected for the complete argument snapshot;
            // execve publishes a new image only after this context is dropped.
            let old_aspace = aspace();
            with_user_memory(old_aspace, |memory| {
                sys_execve(
                    memory,
                    uctx,
                    uctx.arg0() as _,
                    uctx.arg1() as _,
                    uctx.arg2() as _,
                )
            })
        }
        Sysno::execveat => {
            // The path, argv, and envp snapshot must all use the pre-exec
            // address space, even if preparation later commits a new image.
            let old_aspace = aspace();
            with_user_memory(old_aspace, |memory| {
                sys_execveat(
                    memory,
                    uctx,
                    uctx.arg0() as _,
                    uctx.arg1() as _,
                    uctx.arg2() as _,
                    uctx.arg3() as _,
                    uctx.arg4() as _,
                )
            })
        }
        Sysno::init_module => sys_init_module(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::finit_module => {
            sys_finit_module(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }
        Sysno::delete_module => sys_delete_module(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::set_tid_address => sys_set_tid_address(uctx.arg0()),
        Sysno::arch_prctl => sys_arch_prctl(
            UserMemoryCapability::new(aspace()),
            uctx,
            uctx.arg0() as _,
            uctx.arg1() as _,
        ),
        Sysno::modify_ldt => sys_modify_ldt(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::prctl => with_user_memory(aspace(), |memory| {
            sys_prctl(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::prlimit64 => with_user_memory(aspace(), |memory| {
            sys_prlimit64(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::getrlimit => with_user_memory(aspace(), |memory| {
            sys_getrlimit(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::setrlimit => with_user_memory(aspace(), |memory| {
            sys_setrlimit(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::capget => with_user_memory(aspace(), |memory| {
            sys_capget(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::capset => with_user_memory(aspace(), |memory| {
            sys_capset(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::umask => sys_umask(uctx.arg0() as _),
        Sysno::setreuid => sys_setreuid(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setregid => sys_setregid(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::setresuid => sys_setresuid(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::setresgid => sys_setresgid(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::get_mempolicy => with_user_memory(aspace(), |memory| {
            sys_get_mempolicy(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::mbind => with_user_memory(aspace(), |memory| {
            sys_mbind(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
                uctx.arg5() as _,
            )
        }),
        Sysno::migrate_pages => with_user_memory(aspace(), |memory| {
            sys_migrate_pages(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::move_pages => with_user_memory(aspace(), |memory| {
            sys_move_pages(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
                uctx.arg5() as _,
            )
        }),
        Sysno::kcmp => sys_kcmp(
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::set_mempolicy => with_user_memory(aspace(), |memory| {
            sys_set_mempolicy(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),

        // task management
        Sysno::clone => sys_clone(
            UserMemoryCapability::new(aspace()),
            uctx,
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2(),
            uctx.arg3(),
            uctx.arg4(),
        ),
        Sysno::clone3 => sys_clone3(
            UserMemoryCapability::new(aspace()),
            uctx,
            uctx.arg0() as _, // args_ptr
            uctx.arg1() as _, // args_size
        ),
        Sysno::unshare => sys_unshare(uctx.arg0() as _),
        Sysno::setns => sys_setns(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fork => sys_fork(UserMemoryCapability::new(aspace()), uctx),
        #[cfg(target_arch = "x86_64")]
        Sysno::vfork => sys_vfork(UserMemoryCapability::new(aspace()), uctx),
        Sysno::exit => sys_exit(uctx.arg0() as _),
        Sysno::exit_group => sys_exit_group(uctx.arg0() as _),
        Sysno::wait4 => sys_waitpid(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::waitid => sys_waitid(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::ptrace => sys_ptrace(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        // `pid_t` is a signed 32-bit ABI value even though syscall registers
        // are 64-bit.  Keep only its low word so negative PIDs reach getsid's
        // ESRCH path rather than becoming unrelated wide identifiers.
        Sysno::getsid => sys_getsid(uctx.arg0() as u32 as _),
        Sysno::setsid => sys_setsid(),
        Sysno::getpgrp => compat_getpgrp(),
        Sysno::getpgid => sys_getpgid(uctx.arg0() as _),
        Sysno::setpgid => sys_setpgid(uctx.arg0() as i32, uctx.arg1() as i32),
        Sysno::acct => sys_acct(UserMemoryCapability::new(aspace()), uctx.arg0() as _),

        // signal
        Sysno::rt_sigprocmask => with_user_memory(aspace(), |memory| {
            sys_rt_sigprocmask(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::rt_sigaction => with_user_memory(aspace(), |memory| {
            sys_rt_sigaction(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::rt_sigpending => with_user_memory(aspace(), |memory| {
            sys_rt_sigpending(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::rt_sigreturn => with_user_memory(aspace(), |memory| sys_rt_sigreturn(memory, uctx)),
        Sysno::rt_sigtimedwait => with_user_memory(aspace(), |memory| {
            sys_rt_sigtimedwait(
                memory,
                uctx,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::rt_sigsuspend => with_user_memory(aspace(), |memory| {
            sys_rt_sigsuspend(memory, uctx, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::pause => sys_pause(uctx),
        Sysno::kill => sys_kill(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::tkill => sys_tkill(uctx.arg0() as i32, uctx.arg1() as _),
        Sysno::tgkill => sys_tgkill(uctx.arg0() as i32, uctx.arg1() as i32, uctx.arg2() as _),
        Sysno::rt_sigqueueinfo => with_user_memory(aspace(), |memory| {
            sys_rt_sigqueueinfo(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::rt_tgsigqueueinfo => with_user_memory(aspace(), |memory| {
            sys_rt_tgsigqueueinfo(
                memory,
                uctx.arg0() as i32,
                uctx.arg1() as i32,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::sigaltstack => {
            let ss = uctx.arg0() as _;
            let old_ss = uctx.arg1() as _;
            with_user_memory(aspace(), |memory| sys_sigaltstack(memory, uctx, ss, old_ss))
        }
        Sysno::futex => sys_futex(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::futex_waitv => sys_futex_waitv(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),
        Sysno::futex_wake => sys_futex_wake(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::futex_wait => sys_futex_wait(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::futex_requeue => sys_futex_requeue(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::get_robust_list => sys_get_robust_list(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::set_robust_list => sys_set_robust_list(uctx.arg0() as _, uctx.arg1() as _),

        // sys
        Sysno::getuid => sys_getuid(),
        Sysno::geteuid => sys_geteuid(),
        Sysno::getresuid => with_user_memory(aspace(), |memory| {
            sys_getresuid(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::getgid => sys_getgid(),
        Sysno::getegid => sys_getegid(),
        Sysno::getresgid => with_user_memory(aspace(), |memory| {
            sys_getresgid(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::setuid => sys_setuid(uctx.arg0() as _),
        Sysno::setgid => sys_setgid(uctx.arg0() as _),
        Sysno::setfsuid => sys_setfsuid(uctx.arg0() as _),
        Sysno::setfsgid => sys_setfsgid(uctx.arg0() as _),
        Sysno::getgroups => with_user_memory(aspace(), |memory| {
            sys_getgroups(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::setgroups => with_user_memory(aspace(), |memory| {
            sys_setgroups(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::sethostname => with_user_memory(aspace(), |memory| {
            sys_sethostname(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::setdomainname => with_user_memory(aspace(), |memory| {
            sys_setdomainname(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::uname => with_user_memory(aspace(), |memory| sys_uname(memory, uctx.arg0() as _)),
        Sysno::personality => sys_personality(uctx.arg0() as _),
        Sysno::sysinfo => {
            with_user_memory(aspace(), |memory| sys_sysinfo(memory, uctx.arg0() as _))
        }
        Sysno::syslog => sys_syslog(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as isize),
        Sysno::getrandom => with_user_memory(aspace(), |memory| {
            sys_getrandom(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::seccomp => with_user_memory(aspace(), |memory| {
            sys_seccomp(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),

        // sync
        Sysno::membarrier => sys_membarrier(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::rseq => sys_rseq(
            aspace(),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),

        // time
        Sysno::time => with_user_memory(aspace(), |memory| sys_time(memory, uctx.arg0() as _)),
        Sysno::gettimeofday => with_user_memory(aspace(), |memory| {
            sys_gettimeofday(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::clock_settime => with_user_memory(aspace(), |memory| {
            sys_clock_settime(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::settimeofday => with_user_memory(aspace(), |memory| {
            sys_settimeofday(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::adjtimex => {
            with_user_memory(aspace(), |memory| sys_adjtimex(memory, uctx.arg0() as _))
        }
        Sysno::clock_adjtime => with_user_memory(aspace(), |memory| {
            sys_clock_adjtime(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::times => with_user_memory(aspace(), |memory| sys_times(memory, uctx.arg0() as _)),
        Sysno::clock_gettime => with_user_memory(aspace(), |memory| {
            sys_clock_gettime(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::clock_getres => with_user_memory(aspace(), |memory| {
            sys_clock_getres(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::alarm => sys_alarm(uctx.arg0() as u32),
        Sysno::getitimer => with_user_memory(aspace(), |memory| {
            sys_getitimer(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::setitimer => with_user_memory(aspace(), |memory| {
            sys_setitimer(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),

        // msg
        Sysno::msgget => sys_msgget(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::msgsnd => with_user_memory(aspace(), |memory| {
            sys_msgsnd(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::msgrcv => with_user_memory(aspace(), |memory| {
            sys_msgrcv(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
            )
        }),
        Sysno::msgctl => with_user_memory(aspace(), |memory| {
            sys_msgctl(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),

        // shm
        Sysno::shmget => sys_shmget(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::shmat => sys_shmat(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::shmctl => with_user_memory(aspace(), |memory| {
            sys_shmctl(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::shmdt => sys_shmdt(uctx.arg0() as _),

        // sem
        Sysno::semget => sys_semget(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::semctl => with_user_memory(aspace(), |memory| {
            sys_semctl(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::semop => with_user_memory(aspace(), |memory| {
            sys_semop(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::semtimedop => with_user_memory(aspace(), |memory| {
            sys_semtimedop(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),

        // net
        Sysno::socket => sys_socket(uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _),
        Sysno::socketpair => sys_socketpair(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3().into(),
        ),
        Sysno::bind => sys_bind(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
        ),
        Sysno::connect => sys_connect(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
        ),
        Sysno::getsockname => sys_getsockname(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
        ),
        Sysno::getpeername => sys_getpeername(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
        ),
        Sysno::listen => sys_listen(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::accept => sys_accept(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
        ),
        Sysno::accept4 => sys_accept4(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2().into(),
            uctx.arg3() as _,
        ),
        Sysno::shutdown => sys_shutdown(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::sendto => sys_sendto(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4().into(),
            uctx.arg5() as _,
        ),
        Sysno::recvfrom => sys_recvfrom(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4().into(),
            uctx.arg5().into(),
        ),
        Sysno::sendmsg => sys_sendmsg(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
        ),
        Sysno::recvmsg => sys_recvmsg(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
        ),
        Sysno::sendmmsg => sys_sendmmsg(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::recvmmsg => sys_recvmmsg(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1().into(),
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4().into(),
        ),
        Sysno::getsockopt => sys_getsockopt(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3().into(),
            uctx.arg4().into(),
        ),
        Sysno::setsockopt => sys_setsockopt(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3().into(),
            uctx.arg4() as _,
        ),

        // signal file descriptors
        Sysno::signalfd => with_user_memory(aspace(), |memory| {
            compat_signalfd(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::signalfd4 => with_user_memory(aspace(), |memory| {
            sys_signalfd4(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2(),
                uctx.arg3() as _,
            )
        }),

        // timerfd
        Sysno::timerfd_create => sys_timerfd_create(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::timerfd_settime => with_user_memory(aspace(), |memory| {
            sys_timerfd_settime(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::timerfd_gettime => with_user_memory(aspace(), |memory| {
            sys_timerfd_gettime(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::userfaultfd => sys_userfaultfd(uctx.arg0() as _),
        Sysno::io_uring_setup => sys_io_uring_setup(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
        ),
        Sysno::io_uring_enter => sys_io_uring_enter(
            UserMemoryCapability::new(aspace()),
            uctx,
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
            uctx.arg5() as _,
        ),
        Sysno::io_uring_register => sys_io_uring_register(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
        ),
        Sysno::io_pgetevents => with_user_memory(aspace(), |memory| {
            sys_io_pgetevents(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
                uctx.arg4() as _,
                uctx.arg5() as _,
            )
        }),
        Sysno::inotify_init => compat_inotify_init(),
        Sysno::inotify_init1 => sys_inotify_init1(uctx.arg0() as _),
        Sysno::inotify_add_watch => sys_inotify_add_watch(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
        ),
        Sysno::inotify_rm_watch => sys_inotify_rm_watch(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fanotify_init => sys_fanotify_init(uctx.arg0() as _, uctx.arg1() as _),
        Sysno::fanotify_mark => sys_fanotify_mark(
            UserMemoryCapability::new(aspace()),
            uctx.arg0() as _,
            uctx.arg1() as _,
            uctx.arg2() as _,
            uctx.arg3() as _,
            uctx.arg4() as _,
        ),

        // bpf
        #[cfg(feature = "bpf")]
        Sysno::bpf => with_user_memory(aspace(), |memory| {
            bpf::sys_bpf(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),

        // Unsupported fd-producing syscalls.
        Sysno::perf_event_open | Sysno::memfd_secret => sys_unsupported_fd(sysno),

        Sysno::timer_create => with_user_memory(aspace(), |memory| {
            sys_timer_create(memory, uctx.arg0() as _, uctx.arg1() as _, uctx.arg2() as _)
        }),
        Sysno::timer_gettime => with_user_memory(aspace(), |memory| {
            sys_timer_gettime(memory, uctx.arg0() as _, uctx.arg1() as _)
        }),
        Sysno::timer_getoverrun => sys_timer_getoverrun(uctx.arg0() as _),
        Sysno::timer_settime => with_user_memory(aspace(), |memory| {
            sys_timer_settime(
                memory,
                uctx.arg0() as _,
                uctx.arg1() as _,
                uctx.arg2() as _,
                uctx.arg3() as _,
            )
        }),
        Sysno::timer_delete => sys_timer_delete(uctx.arg0() as _),

        // Linux x86_64's native sys_ni_syscall table slots.
        Sysno::uselib
        | Sysno::_sysctl
        | Sysno::create_module
        | Sysno::get_kernel_syms
        | Sysno::query_module
        | Sysno::nfsservctl
        | Sysno::getpmsg
        | Sysno::putpmsg
        | Sysno::afs_syscall
        | Sysno::tuxcall
        | Sysno::security
        | Sysno::set_thread_area
        | Sysno::get_thread_area
        | Sysno::lookup_dcookie
        | Sysno::epoll_ctl_old
        | Sysno::epoll_wait_old
        | Sysno::vserver => sys_ni_syscall(),

        _ => {
            debug!("Unimplemented syscall: {sysno}");
            Err(AxError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn legacy_epoll_create_requires_a_positive_size() {
        assert!(matches!(
            validate_legacy_epoll_create_size(-1),
            Err(AxError::InvalidInput)
        ));
        assert!(matches!(
            validate_legacy_epoll_create_size(0),
            Err(AxError::InvalidInput)
        ));
        assert!(validate_legacy_epoll_create_size(1).is_ok());
    }

    #[test]
    fn legacy_mknod_device_argument_is_truncated_to_u32() {
        assert_eq!(legacy_mknod_dev(0xffff_ffff_1234_5678), 0x1234_5678);
    }

    #[test]
    fn unsupported_dispatch_does_not_snapshot_address_space() {
        let mut context =
            UserContext::new(0x1234_5678, axhal::mem::VirtAddr::from_usize(0x8000), 0);
        let snapshots = Cell::new(0);

        let result = dispatch_syscall(Sysno::uretprobe, &mut context, || {
            snapshots.set(snapshots.get() + 1);
            panic!("unsupported syscall must not acquire an address space");
        });

        assert_eq!(result, Err(AxError::Unsupported));
        assert_eq!(snapshots.get(), 0);
    }

    #[test]
    fn linux_native_ni_slots_return_enosys_without_an_address_space() {
        let native_ni = [
            Sysno::uselib,
            Sysno::_sysctl,
            Sysno::create_module,
            Sysno::get_kernel_syms,
            Sysno::query_module,
            Sysno::nfsservctl,
            Sysno::getpmsg,
            Sysno::putpmsg,
            Sysno::afs_syscall,
            Sysno::tuxcall,
            Sysno::security,
            Sysno::set_thread_area,
            Sysno::get_thread_area,
            Sysno::lookup_dcookie,
            Sysno::epoll_ctl_old,
            Sysno::epoll_wait_old,
            Sysno::vserver,
        ];

        for sysno in native_ni {
            let mut context =
                UserContext::new(0x1234_5678, axhal::mem::VirtAddr::from_usize(0x8000), 0);
            let snapshots = Cell::new(0);
            let result = dispatch_syscall(sysno, &mut context, || {
                snapshots.set(snapshots.get() + 1);
                panic!("native NI syscall must not acquire an address space");
            });

            assert_eq!(result, Err(AxError::Unsupported), "{sysno}");
            assert_eq!(snapshots.get(), 0, "{sysno}");
        }
    }
}
