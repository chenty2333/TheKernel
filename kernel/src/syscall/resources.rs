use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use linux_raw_sys::general::{
    CAP_SYS_RESOURCE, RLIM_NLIMITS, RLIMIT_NOFILE, rlimit, rlimit64, rusage,
};
use starry_process::Pid;
use starry_vm::{VmMutPtr, VmPtr};

use crate::task::{AsThread, ProcessData, TaskUsage, get_process_data, nr_open_limit};

fn can_raise_hard_limit(proc_data: &ProcessData) -> bool {
    proc_data.has_effective_capability(CAP_SYS_RESOURCE) || proc_data.euid() == 0
}

fn set_resource_limit(proc_data: &ProcessData, resource: u32, new_limit: rlimit64) -> AxResult<()> {
    if new_limit.rlim_cur > new_limit.rlim_max {
        return Err(AxError::InvalidInput);
    }
    if resource == RLIMIT_NOFILE && new_limit.rlim_max > nr_open_limit() {
        return Err(AxError::from(LinuxError::EPERM));
    }

    let curr = current();
    let curr_proc = &curr.as_thread().proc_data;
    let limit = &mut proc_data.rlim.write()[resource];
    if new_limit.rlim_max <= limit.max {
        limit.max = new_limit.rlim_max;
    } else if can_raise_hard_limit(curr_proc) {
        limit.max = new_limit.rlim_max;
    } else {
        return Err(AxError::OperationNotPermitted);
    }

    limit.current = new_limit.rlim_cur;
    Ok(())
}

pub fn sys_prlimit64(
    pid: Pid,
    resource: u32,
    new_limit: *const rlimit64,
    old_limit: *mut rlimit64,
) -> AxResult<isize> {
    if resource >= RLIM_NLIMITS {
        return Err(AxError::InvalidInput);
    }

    let proc_data = get_process_data(pid)?;
    if let Some(old_limit) = old_limit.nullable() {
        let limit = &proc_data.rlim.read()[resource];
        old_limit.vm_write(rlimit64 {
            rlim_cur: limit.current,
            rlim_max: limit.max,
        })?;
    }

    if let Some(new_limit) = new_limit.nullable() {
        // FIXME: AnyBitPattern
        let new_limit = unsafe { new_limit.vm_read_uninit()?.assume_init() };
        set_resource_limit(&proc_data, resource, new_limit)?;
    }

    Ok(0)
}

pub fn sys_setrlimit(resource: u32, new_limit: *const rlimit) -> AxResult<isize> {
    if resource >= RLIM_NLIMITS {
        return Err(AxError::InvalidInput);
    }

    let new_limit = unsafe { new_limit.vm_read_uninit()?.assume_init() };
    let proc_data = current().as_thread().proc_data.clone();
    set_resource_limit(
        &proc_data,
        resource,
        rlimit64 {
            rlim_cur: new_limit.rlim_cur,
            rlim_max: new_limit.rlim_max,
        },
    )?;
    Ok(0)
}

pub fn sys_getrlimit(resource: u32, old_limit: *mut rlimit) -> AxResult<isize> {
    if resource >= RLIM_NLIMITS {
        return Err(AxError::InvalidInput);
    }

    let proc_data = current().as_thread().proc_data.clone();
    if let Some(old_limit) = old_limit.nullable() {
        let limit = &proc_data.rlim.read()[resource];
        old_limit.vm_write(rlimit {
            rlim_cur: limit.current as _,
            rlim_max: limit.max as _,
        })?;
    }

    Ok(0)
}

pub fn sys_getrusage(who: i32, usage: *mut rusage) -> AxResult<isize> {
    const RUSAGE_SELF: i32 = linux_raw_sys::general::RUSAGE_SELF as i32;
    const RUSAGE_CHILDREN: i32 = linux_raw_sys::general::RUSAGE_CHILDREN;
    const RUSAGE_THREAD: i32 = linux_raw_sys::general::RUSAGE_THREAD as i32;

    let curr = current();
    let thr = curr.as_thread();

    let result = match who {
        RUSAGE_SELF => thr.proc_data.self_usage(),
        RUSAGE_CHILDREN => thr.proc_data.children_usage(),
        RUSAGE_THREAD => TaskUsage::from_thread(thr),
        _ => return Err(AxError::InvalidInput),
    };
    usage.vm_write(result.into())?;

    Ok(0)
}
