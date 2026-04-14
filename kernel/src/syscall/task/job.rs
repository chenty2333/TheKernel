use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use starry_process::Pid;

use crate::task::{AsThread, get_process_data, get_process_group, remember_process_group};

pub fn sys_getsid(pid: Pid) -> AxResult<isize> {
    Ok(get_process_data(pid)?.proc.group().session().sid() as _)
}

pub fn sys_setsid() -> AxResult<isize> {
    let curr = current();
    let proc = &curr.as_thread().proc_data.proc;
    if proc.group().pgid() == proc.pid() {
        return Err(AxError::OperationNotPermitted);
    }

    if let Some((session, group)) = proc.create_session() {
        remember_process_group(&group);
        Ok(session.sid() as _)
    } else {
        Ok(proc.pid() as _)
    }
}

pub fn sys_getpgid(pid: Pid) -> AxResult<isize> {
    Ok(get_process_data(pid)?.proc.group().pgid() as _)
}

pub fn sys_setpgid(pid: i32, pgid: i32) -> AxResult<isize> {
    let curr = current();
    let caller = &curr.as_thread().proc_data.proc;

    if pgid < 0 {
        return Err(AxError::InvalidInput);
    }

    let proc = if pid == 0 {
        caller.clone()
    } else if pid > 0 {
        get_process_data(pid as Pid)?.proc.clone()
    } else {
        return Err(AxError::from(LinuxError::ESRCH));
    };

    if proc.pid() != caller.pid() {
        let is_child = proc.parent().is_some_and(|parent| parent.pid() == caller.pid());
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
        if let Some(group) = proc.create_group() {
            remember_process_group(&group);
        }
        return Ok(0);
    }

    let group = get_process_group(pgid).map_err(|_| AxError::OperationNotPermitted)?;
    if !alloc::sync::Arc::ptr_eq(&group.session(), &caller_session) {
        return Err(AxError::OperationNotPermitted);
    }
    if !proc.move_to_group(&group) {
        return Err(AxError::OperationNotPermitted);
    }

    Ok(0)
}

// TODO: job control
