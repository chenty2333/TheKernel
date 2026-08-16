use core::mem::{align_of, offset_of, size_of};

use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use linux_raw_sys::general::{
    CAP_SYS_RESOURCE, RLIM_INFINITY, RLIM_NLIMITS, RLIMIT_CPU, RLIMIT_NOFILE, rlimit, rlimit64,
    rusage,
};
use thekernel_linux_process_adapter::Pid;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, VmMutPtr, VmPtr};

use crate::{
    mm::map_usercopy_error,
    task::{
        AsThread, ProcessData, TaskUsage, check_current_process_prlimit_access, get_process_data,
        nr_open_limit,
    },
};

// `linux_raw_sys` does not expose bytemuck's `AnyBitPattern`/`NoUninit`
// markers for these ABI structs.  The x86_64 Linux definitions are integer
// fields only; keep their complete object layouts checked before using the
// explicit usercopy unchecked path below.
const _: () = {
    assert!(align_of::<rlimit>() == 8);
    assert!(size_of::<rlimit>() == 16);
    assert!(offset_of!(rlimit, rlim_cur) == 0);
    assert!(offset_of!(rlimit, rlim_max) == 8);
    assert!(align_of::<rlimit64>() == 8);
    assert!(size_of::<rlimit64>() == 16);
    assert!(offset_of!(rlimit64, rlim_cur) == 0);
    assert!(offset_of!(rlimit64, rlim_max) == 8);
    assert!(align_of::<rusage>() == 8);
    assert!(size_of::<rusage>() == 144);
    assert!(offset_of!(rusage, ru_utime) == 0);
    assert!(offset_of!(rusage, ru_stime) == 16);
    assert!(offset_of!(rusage, ru_maxrss) == 32);
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
    // Lowering the hard limit is always permitted. Raising it requires the
    // capability the caller resolved before taking this lock.
    if new_limit.rlim_max > limit.max && !can_raise_hard_limit {
        return Err(AxError::OperationNotPermitted);
    }
    limit.max = new_limit.rlim_max;

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

pub fn sys_prlimit64<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    pid: Pid,
    resource: u32,
    new_limit: *const rlimit64,
    old_limit: *mut rlimit64,
) -> AxResult<isize> {
    // Linux faults `new_limit` before PID lookup, permission checks, and
    // resource validation. Keep that observable precedence for combinations
    // of bad pointers, dead PIDs, and out-of-range resource numbers.
    let new_limit = if let Some(new_limit) = VmPtr::nullable(new_limit) {
        let value = VmPtr::vm_read_uninit(new_limit, memory).map_err(map_usercopy_error)?;
        // SAFETY: the explicit provider initialized the complete value and
        // rlimit64 contains only integer fields on the x86_64 Linux ABI.
        Some(unsafe { value.assume_init() })
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
    if let Some(old_limit) = VmPtr::nullable(old_limit) {
        // SAFETY: rlimit64 has no padding on the checked x86_64 ABI, and all
        // fields in `old` are initialized before this copyout.
        unsafe { VmMutPtr::vm_write_unchecked(old_limit, memory, old) }
            .map_err(map_usercopy_error)?;
    }

    Ok(0)
}

pub fn sys_setrlimit<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    resource: u32,
    new_limit: *const rlimit,
) -> AxResult<isize> {
    // Linux copies the replacement before dispatching resource policy, so a
    // bad userspace pointer wins over an out-of-range resource number.
    // SAFETY: the explicit provider initializes every byte and rlimit contains
    // only integer fields on the checked x86_64 Linux ABI.
    let new_limit = unsafe {
        VmPtr::vm_read_uninit(new_limit, memory)
            .map_err(map_usercopy_error)?
            .assume_init()
    };
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

pub fn sys_getrlimit<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    resource: u32,
    old_limit: *mut rlimit,
) -> AxResult<isize> {
    if resource >= RLIM_NLIMITS {
        return Err(AxError::InvalidInput);
    }

    let proc_data = current().as_thread().proc_data.clone();
    let limit = &proc_data.rlim.read()[resource];
    let old = rlimit {
        rlim_cur: limit.current as _,
        rlim_max: limit.max as _,
    };
    // SAFETY: rlimit has no padding on the checked x86_64 ABI, and both
    // fields in `old` are initialized before this copyout.
    unsafe { VmMutPtr::vm_write_unchecked(old_limit, memory, old) }.map_err(map_usercopy_error)?;

    Ok(0)
}

pub fn sys_getrusage<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    who: i32,
    usage: *mut rusage,
) -> AxResult<isize> {
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
    let result: rusage = result.into();
    // SAFETY: TaskUsage conversion starts from a zeroed rusage and fills the
    // integer fields, so the full object representation is initialized.
    unsafe { VmMutPtr::vm_write_unchecked(usage, memory, result) }.map_err(map_usercopy_error)?;

    Ok(0)
}
