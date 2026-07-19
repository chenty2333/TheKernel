use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use linux_raw_sys::general::{
    CAP_SYS_RESOURCE, RLIM_INFINITY, RLIM_NLIMITS, RLIMIT_CPU, RLIMIT_NOFILE, rlimit, rlimit64,
    rusage,
};
use starry_process::Pid;
use starry_vm::{VmMutPtr, VmPtr};

use crate::task::{
    AsThread, ProcessData, TaskUsage, check_current_process_prlimit_access, get_process_data,
    nr_open_limit,
};

fn current_can_raise_hard_limit() -> bool {
    current()
        .as_thread()
        .has_effective_capability(CAP_SYS_RESOURCE)
}

fn update_resource_limit(
    proc_data: &alloc::sync::Arc<ProcessData>,
    resource: u32,
    new_limit: Option<rlimit64>,
) -> AxResult<rlimit64> {
    let Some(new_limit) = new_limit else {
        let limit = &proc_data.rlim.read()[resource];
        return Ok(rlimit64 {
            rlim_cur: limit.current,
            rlim_max: limit.max,
        });
    };
    if new_limit.rlim_cur > new_limit.rlim_max {
        return Err(AxError::InvalidInput);
    }
    if resource == RLIMIT_NOFILE && new_limit.rlim_max > nr_open_limit() {
        return Err(AxError::from(LinuxError::EPERM));
    }

    let can_raise_hard_limit = current_can_raise_hard_limit();
    let mut limits = proc_data.rlim.write();
    let limit = &mut limits[resource];
    let old = rlimit64 {
        rlim_cur: limit.current,
        rlim_max: limit.max,
    };
    if new_limit.rlim_max <= limit.max {
        limit.max = new_limit.rlim_max;
    } else if can_raise_hard_limit {
        limit.max = new_limit.rlim_max;
    } else {
        return Err(AxError::OperationNotPermitted);
    }

    limit.current = new_limit.rlim_cur;
    if resource == RLIMIT_CPU {
        proc_data.process_rlimit_cpu_active.store(
            new_limit.rlim_cur != RLIM_INFINITY as i64 as u64,
            core::sync::atomic::Ordering::Release,
        );
    }
    drop(limits);

    if resource == RLIMIT_CPU && crate::task::request_process_cpu_evaluation(proc_data) {
        crate::deferred_work::wake_process_timer_worker();
    }
    Ok(old)
}

pub fn sys_prlimit64(
    pid: Pid,
    resource: u32,
    new_limit: *const rlimit64,
    old_limit: *mut rlimit64,
) -> AxResult<isize> {
    // Linux faults `new_limit` before PID lookup, permission checks, and
    // resource validation. Keep that observable precedence for combinations
    // of bad pointers, dead PIDs, and out-of-range resource numbers.
    let new_limit = if let Some(new_limit) = new_limit.nullable() {
        // FIXME: AnyBitPattern
        Some(unsafe { new_limit.vm_read_uninit()?.assume_init() })
    } else {
        None
    };
    let proc_data = get_process_data(pid)?;
    check_current_process_prlimit_access(&proc_data)?;
    if resource >= RLIM_NLIMITS {
        return Err(AxError::InvalidInput);
    }
    // Snapshot and optional replacement share one owner critical section, so
    // prlimit64(old,new) cannot report a value from a different generation.
    let old = update_resource_limit(&proc_data, resource, new_limit)?;
    if let Some(old_limit) = old_limit.nullable() {
        old_limit.vm_write(old)?;
    }

    Ok(0)
}

pub fn sys_setrlimit(resource: u32, new_limit: *const rlimit) -> AxResult<isize> {
    // Linux copies the replacement before dispatching resource policy, so a
    // bad userspace pointer wins over an out-of-range resource number.
    let new_limit = unsafe { new_limit.vm_read_uninit()?.assume_init() };
    if resource >= RLIM_NLIMITS {
        return Err(AxError::InvalidInput);
    }

    let proc_data = current().as_thread().proc_data.clone();
    update_resource_limit(
        &proc_data,
        resource,
        Some(rlimit64 {
            rlim_cur: new_limit.rlim_cur,
            rlim_max: new_limit.rlim_max,
        }),
    )?;
    Ok(0)
}

pub fn sys_getrlimit(resource: u32, old_limit: *mut rlimit) -> AxResult<isize> {
    if resource >= RLIM_NLIMITS {
        return Err(AxError::InvalidInput);
    }

    let proc_data = current().as_thread().proc_data.clone();
    let limit = &proc_data.rlim.read()[resource];
    old_limit.vm_write(rlimit {
        rlim_cur: limit.current as _,
        rlim_max: limit.max as _,
    })?;

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
