use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use starry_process::{Pid, ProcessError};

use crate::task::{
    AsThread, get_process_data, get_process_group, get_process_including_zombie, process_domain,
};

/// Serializes process-group/session identity admission with the corresponding
/// Starry membership mutation. These syscalls are cold paths, and a sleepable
/// mutex avoids holding a spin lock across object construction.
static JOB_CONTROL_OPERATION: Mutex<()> = Mutex::new(());

fn process_error(error: ProcessError) -> AxError {
    match error {
        ProcessError::NoMemory | ProcessError::Capacity => AxError::NoMemory,
        ProcessError::AlreadyExists => AxError::AlreadyExists,
        ProcessError::NotPublished | ProcessError::NotLive | ProcessError::NotInitialized => {
            AxError::NoSuchProcess
        }
        ProcessError::WrongDomain => AxError::BadState,
        _ => AxError::BadState,
    }
}

pub fn sys_getsid(pid: Pid) -> AxResult<isize> {
    let pid = if pid == 0 {
        current().as_thread().proc_data.proc.pid()
    } else {
        pid
    };
    Ok(get_process_including_zombie(pid)?.group().session().sid() as _)
}

pub fn sys_setsid() -> AxResult<isize> {
    let _operation = JOB_CONTROL_OPERATION.lock();
    let curr = current();
    let proc = &curr.as_thread().proc_data.proc;
    if proc.group().pgid() == proc.pid() {
        return Err(AxError::OperationNotPermitted);
    }

    if let Some((session, _group)) = process_domain()?
        .try_create_session(proc)
        .map_err(process_error)?
    {
        Ok(session.sid() as _)
    } else {
        Ok(proc.pid() as _)
    }
}

pub fn sys_getpgid(pid: Pid) -> AxResult<isize> {
    let pid = if pid == 0 {
        current().as_thread().proc_data.proc.pid()
    } else {
        pid
    };
    Ok(get_process_including_zombie(pid)?.group().pgid() as _)
}

pub fn sys_setpgid(pid: i32, pgid: i32) -> AxResult<isize> {
    let _operation = JOB_CONTROL_OPERATION.lock();
    let curr = current();
    let caller = &curr.as_thread().proc_data.proc;

    if pgid < 0 {
        return Err(AxError::InvalidInput);
    }

    let proc = if pid == 0 {
        caller.clone()
    } else if pid > 0 {
        let target = get_process_data(pid as Pid)?;
        if !target.proc.is_live() {
            return Err(AxError::from(LinuxError::ESRCH));
        }
        target.proc.clone()
    } else {
        return Err(AxError::from(LinuxError::ESRCH));
    };

    if proc.pid() != caller.pid() {
        let is_child = proc
            .parent()
            .is_some_and(|parent| parent.pid() == caller.pid());
        if !is_child {
            return Err(AxError::from(LinuxError::ESRCH));
        }
    }

    if proc.group().session().sid() == proc.pid() {
        return Err(AxError::OperationNotPermitted);
    }

    let caller_session = caller.group().session();
    if !alloc::sync::Arc::ptr_eq(&proc.group().session(), &caller_session) {
        return Err(AxError::OperationNotPermitted);
    }

    let pgid = if pgid == 0 { proc.pid() } else { pgid as Pid };
    if pgid == proc.pid() {
        if proc.group().pgid() == pgid {
            return Ok(0);
        }
        process_domain()?
            .try_create_group(&proc)
            .map_err(process_error)?;
        return Ok(0);
    }

    let group = get_process_group(pgid).map_err(|_| AxError::OperationNotPermitted)?;
    if !alloc::sync::Arc::ptr_eq(&group.session(), &caller_session) {
        return Err(AxError::OperationNotPermitted);
    }
    if !process_domain()?
        .move_to_group(&proc, &group)
        .map_err(process_error)?
    {
        return Err(AxError::OperationNotPermitted);
    }

    Ok(0)
}

// TODO: job control
