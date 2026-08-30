use alloc::sync::Arc;
use core::{
    mem::size_of,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axtask::current;

use crate::{
    file::{PerfEventFile, SoftwareEvent, add_file_like},
    mm::{UserMemoryCapability, map_usercopy_error},
    task::AsThread,
};

const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;
const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
const PERF_COUNT_SW_PAGE_FAULTS: u64 = 2;
const PERF_COUNT_SW_CONTEXT_SWITCHES: u64 = 3;
const PERF_FLAG_FD_NO_GROUP: u64 = 1;
const PERF_FLAG_PID_CGROUP: u64 = 4;
const PERF_FLAG_FD_CLOEXEC: u64 = 8;
const PERF_ATTR_SIZE_VER0: u32 = 64;
const ATTR_DISABLED: u64 = 1;

static NEXT_PERF_EVENT_ID: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
#[derive(Clone, Copy)]
struct PerfEventAttrV0 {
    event_type: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
}

// All fields are integer words and Linux's original ABI is exactly 64 bytes.
const _: () = assert!(size_of::<PerfEventAttrV0>() == PERF_ATTR_SIZE_VER0 as usize);

fn read_attr(
    memory: &UserMemoryCapability,
    attr: *const PerfEventAttrV0,
) -> AxResult<PerfEventAttrV0> {
    if attr.is_null() {
        return Err(AxError::BadAddress);
    }
    memory
        .read_value_uninit(attr)
        .map_err(map_usercopy_error)
        .map(|value| unsafe { value.assume_init() })
}

/// Implements the ABI-valid software-clock subset.  Unsupported perf types
/// fail at creation, rather than producing a descriptor whose samples lie.
pub(crate) fn sys_perf_event_open(
    memory: UserMemoryCapability,
    attr: *const PerfEventAttrV0,
    pid: i32,
    cpu: i32,
    group_fd: i32,
    flags: u64,
) -> AxResult<isize> {
    let attr = read_attr(&memory, attr)?;
    if attr.size < PERF_ATTR_SIZE_VER0 {
        return Err(AxError::InvalidInput);
    }
    if flags & !(PERF_FLAG_FD_NO_GROUP | PERF_FLAG_PID_CGROUP | PERF_FLAG_FD_CLOEXEC) != 0 {
        return Err(AxError::InvalidInput);
    }
    // A cgroup file descriptor has no meaning until cgroup perf attachment is
    // implemented; reject it before interpreting pid.
    if flags & PERF_FLAG_PID_CGROUP != 0 {
        return Err(AxError::OperationNotSupported);
    }
    if pid != 0 || cpu != -1 {
        return Err(AxError::OperationNotSupported);
    }
    if group_fd != -1 && flags & PERF_FLAG_FD_NO_GROUP == 0 {
        return Err(AxError::OperationNotSupported);
    }
    if attr.event_type != PERF_TYPE_SOFTWARE {
        return Err(AxError::OperationNotSupported);
    }
    let event = match attr.config {
        PERF_COUNT_SW_CPU_CLOCK => SoftwareEvent::CpuClock,
        PERF_COUNT_SW_TASK_CLOCK => SoftwareEvent::TaskClock,
        PERF_COUNT_SW_PAGE_FAULTS => SoftwareEvent::PageFaults,
        PERF_COUNT_SW_CONTEXT_SWITCHES => SoftwareEvent::ContextSwitches,
        _ => return Err(AxError::OperationNotSupported),
    };
    // Sampling, output routing and read-format extensions are not fabricated.
    if attr.sample_period != 0 || attr.sample_type != 0 || attr.read_format != 0 {
        return Err(AxError::OperationNotSupported);
    }
    let id = NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    let file = PerfEventFile::new(id, event, attr.flags & ATTR_DISABLED != 0);
    current().as_thread().attach_perf_event(&file)?;
    add_file_like(
        file as Arc<dyn crate::file::FileLike>,
        flags & PERF_FLAG_FD_CLOEXEC != 0,
    )
    .map(|fd| fd as isize)
}
