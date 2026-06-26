use axerrno::{AxError, AxResult};
use axtask::current;
use bitflags::bitflags;
use linux_raw_sys::general::{CAP_SYS_PTRACE, SI_TKILL};
use starry_signal::SignalInfo;
use starry_vm::VmPtr;

use crate::{
    file::{Directory, FD_TABLE, FileLike, PidFd, add_file_description},
    pseudofs::{ProcDirProcess, process_data_from_proc_dir},
    syscall::signal::parse_signo,
    task::{
        AsThread, ProcessData, get_process_data, get_visible_task, send_signal_to_process_data,
    },
};

fn process_data_from_proc_dir_fd(fd: i32) -> AxResult<alloc::sync::Arc<crate::task::ProcessData>> {
    let dir = Directory::from_fd(fd).map_err(|err| {
        if err == AxError::InvalidInput {
            AxError::BadFileDescriptor
        } else {
            err
        }
    })?;
    match process_data_from_proc_dir(dir.inner()) {
        ProcDirProcess::Live(proc_data) => Ok(proc_data),
        ProcDirProcess::Stale => Err(AxError::NoSuchProcess),
        ProcDirProcess::NotProcDir => Err(AxError::BadFileDescriptor),
    }
}

fn process_data_from_signal_fd(fd: i32) -> AxResult<alloc::sync::Arc<crate::task::ProcessData>> {
    match PidFd::from_fd(fd) {
        Ok(pidfd) => pidfd.process_data(),
        Err(AxError::InvalidInput) => process_data_from_proc_dir_fd(fd),
        Err(err) => Err(err),
    }
}

fn check_pidfd_getfd_permission(target: &ProcessData) -> AxResult<()> {
    let curr = current();
    let actor = &curr.as_thread().proc_data;
    if actor.proc.pid() == target.proc.pid()
        || actor.euid() == 0
        || actor.has_effective_capability(CAP_SYS_PTRACE)
        || [actor.uid(), actor.euid()]
            .into_iter()
            .any(|id| id == target.uid() || id == target.euid() || id == target.suid())
    {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, Default)]
    pub struct PidFdFlags: u32 {
        const NONBLOCK = 2048;
        const THREAD = 128;
    }
}

pub fn sys_pidfd_open(pid: i32, flags: u32) -> AxResult<isize> {
    debug!("sys_pidfd_open <= pid: {pid}, flags: {flags}");

    let flags = PidFdFlags::from_bits(flags).ok_or(AxError::InvalidInput)?;
    if pid <= 0 {
        return Err(AxError::InvalidInput);
    }
    let pid = pid as u32;

    let fd = if flags.contains(PidFdFlags::THREAD) {
        PidFd::new_thread(get_visible_task(pid)?.as_thread())
    } else {
        PidFd::new_process(&get_process_data(pid)?)
    };
    if flags.contains(PidFdFlags::NONBLOCK) {
        fd.set_nonblocking(true)?;
    }

    fd.add_to_fd_table(true).map(|fd| fd as _)
}

pub fn sys_pidfd_getfd(pidfd: i32, target_fd: i32, flags: u32) -> AxResult<isize> {
    debug!("sys_pidfd_getfd <= pidfd: {pidfd}, target_fd: {target_fd}, flags: {flags}");

    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    let pidfd = PidFd::from_fd(pidfd)?;
    let proc_data = pidfd.process_data()?;
    check_pidfd_getfd_permission(&proc_data)?;
    FD_TABLE
        .scope(&proc_data.scope.read())
        .read()
        .get(target_fd as usize)
        .ok_or(AxError::BadFileDescriptor)
        .and_then(|fd| {
            let fd = add_file_description(fd.description.clone(), true)?;
            Ok(fd as isize)
        })
}

fn make_pidfd_signal_info(
    target: &ProcessData,
    signo: u32,
    sig: *const SignalInfo,
) -> AxResult<Option<SignalInfo>> {
    if signo == 0 {
        return Ok(None);
    }

    let signo = parse_signo(signo)?;
    let sig = unsafe { sig.vm_read_uninit()?.assume_init() };
    if sig.signo() != signo {
        return Err(AxError::InvalidInput);
    }
    if current().as_thread().proc_data.proc.pid() != target.proc.pid()
        && (sig.code() >= 0 || sig.code() == SI_TKILL)
    {
        return Err(AxError::OperationNotPermitted);
    }
    Ok(Some(sig))
}

pub fn sys_pidfd_send_signal(
    pidfd: i32,
    signo: u32,
    sig: *mut SignalInfo,
    flags: u32,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let proc_data = process_data_from_signal_fd(pidfd)?;

    let sig = if sig.is_null() {
        if signo == 0 {
            None
        } else {
            let signo = parse_signo(signo)?;
            Some(SignalInfo::new_user(
                signo,
                0,
                current().as_thread().proc_data.proc.pid(),
            ))
        }
    } else {
        make_pidfd_signal_info(&proc_data, signo, sig)?
    };
    send_signal_to_process_data(&proc_data, sig)?;
    Ok(0)
}
