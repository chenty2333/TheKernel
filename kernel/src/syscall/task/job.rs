use axerrno::{AxError, AxResult};
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

pub fn sys_setpgid(pid: Pid, pgid: Pid) -> AxResult<isize> {
    let proc = &get_process_data(pid)?.proc;

    if pgid == 0 {
        if let Some(group) = proc.create_group() {
            remember_process_group(&group);
        }
    } else if !proc.move_to_group(&get_process_group(pgid)?) {
        return Err(AxError::OperationNotPermitted);
    }

    Ok(0)
}

// TODO: job control
