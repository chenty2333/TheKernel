use alloc::sync::Arc;
use core::{
    mem::size_of,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axtask::current;

use crate::{
    file::{PerfEvent, PerfEventFile, PerfGroup, SoftwareEvent, add_file_like, get_typed_file},
    mm::{UserMemoryCapability, map_usercopy_error},
    task::{
        AsThread, PtraceAccessMode, check_current_thread_ptrace_image_access, get_visible_task,
    },
};

const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_TYPE_SOFTWARE: u32 = 1;
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;
const PERF_COUNT_SW_TASK_CLOCK: u64 = 1;
const PERF_COUNT_SW_PAGE_FAULTS: u64 = 2;
const PERF_COUNT_SW_CONTEXT_SWITCHES: u64 = 3;
const PERF_FLAG_FD_NO_GROUP: u64 = 1;
const PERF_FLAG_PID_CGROUP: u64 = 4;
const PERF_FLAG_FD_CLOEXEC: u64 = 8;
const PERF_ATTR_SIZE_VER0: u32 = 64;
// Linux extends perf_event_attr by appending fields.  This implementation
// understands only v0, but it must not silently discard a requested newer
// field.  Keep the probe bounded so a malicious size cannot make perf open
// perform unbounded usercopy work.
const PERF_ATTR_MAX_SIZE: u32 = 4096;
const PERF_ATTR_EXTENSION_CHUNK_SIZE: usize = 64;
const ATTR_DISABLED: u64 = 1;
const ATTR_EXCLUDE_USER: u64 = 1 << 4;
const ATTR_EXCLUDE_KERNEL: u64 = 1 << 5;
#[cfg(feature = "perf-sampling")]
const PERF_SAMPLE_SUPPORTED: u64 = crate::file::perf_sampling::PERF_SAMPLE_SUPPORTED;

static NEXT_PERF_EVENT_ID: AtomicU64 = AtomicU64::new(1);

/// This implementation has no inheritance, filtering, pinning, sampling, or
/// PMU scheduling. Accepting any corresponding perf attribute bit would
/// advertise behavior the event cannot provide.
fn supported_attr_flags(flags: u64) -> AxResult<bool> {
    if flags & !ATTR_DISABLED != 0 {
        return Err(AxError::OperationNotSupported);
    }
    Ok(flags & ATTR_DISABLED != 0)
}

#[cfg(feature = "perf-sampling")]
fn open_sampling(
    attr: PerfEventAttrV0,
    pid: i32,
    cpu: i32,
    group_fd: i32,
    flags: u64,
) -> AxResult<isize> {
    if pid != 0
        || cpu != -1
        || group_fd != -1
        || flags & !(PERF_FLAG_FD_CLOEXEC | PERF_FLAG_FD_NO_GROUP) != 0
        || flags & PERF_FLAG_FD_NO_GROUP != 0
    {
        return Err(AxError::OperationNotSupported);
    }
    if attr.event_type != PERF_TYPE_HARDWARE {
        return Err(AxError::OperationNotSupported);
    }
    if attr.sample_period < 4096
        || attr.sample_type == 0
        || attr.sample_type & !PERF_SAMPLE_SUPPORTED != 0
        || attr.read_format & !crate::file::PERF_FORMAT_SUPPORTED != 0
        || attr.read_format & crate::file::PERF_FORMAT_GROUP != 0
        || attr.wakeup_events > 1
        || attr.bp_type != 0
        || attr.config1 != 0
    {
        return Err(AxError::InvalidInput);
    }
    if attr.flags & !(ATTR_DISABLED | ATTR_EXCLUDE_USER | ATTR_EXCLUDE_KERNEL) != 0
        || attr.flags & (ATTR_EXCLUDE_USER | ATTR_EXCLUDE_KERNEL)
            == (ATTR_EXCLUDE_USER | ATTR_EXCLUDE_KERNEL)
    {
        return Err(AxError::OperationNotSupported);
    }
    let caps = axhal::pmu::capabilities().map_err(|_| AxError::OperationNotSupported)?;
    if attr.sample_period > caps.programmable_mask() {
        return Err(AxError::InvalidInput);
    }
    let event = match attr.config {
        PERF_COUNT_HW_CPU_CYCLES => crate::file::perf_sampling::SamplingEvent::Cycles,
        PERF_COUNT_HW_INSTRUCTIONS => crate::file::perf_sampling::SamplingEvent::Instructions,
        _ => return Err(AxError::OperationNotSupported),
    };
    let id = NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    let file =
        crate::file::PerfSamplingFile::try_new(crate::file::perf_sampling::SamplingConfig {
            id,
            target_task_id: current().id().as_u64(),
            event,
            period: attr.sample_period,
            sample_type: attr.sample_type,
            count_user: attr.flags & ATTR_EXCLUDE_USER == 0,
            count_kernel: attr.flags & ATTR_EXCLUDE_KERNEL == 0,
            disabled: attr.flags & ATTR_DISABLED != 0,
            read_format: attr.read_format,
        })?;
    current().as_thread().attach_perf_sampling(&file)?;
    match add_file_like(
        file.clone() as Arc<dyn crate::file::FileLike>,
        flags & PERF_FLAG_FD_CLOEXEC != 0,
    ) {
        Ok(fd) => {
            file.enter_current();
            Ok(fd as isize)
        }
        Err(error) => {
            current().as_thread().detach_perf_sampling(&file);
            Err(error)
        }
    }
}

#[cfg(test)]
mod abi_tests {
    use super::{
        ATTR_DISABLED, PERF_COUNT_HW_CPU_CYCLES, PERF_COUNT_HW_INSTRUCTIONS, PERF_TYPE_HARDWARE,
        PERF_TYPE_SOFTWARE, supported_attr_flags,
    };

    #[test]
    fn perf_attr_accepts_only_disabled_bit() {
        assert!(!supported_attr_flags(0).unwrap());
        assert!(supported_attr_flags(ATTR_DISABLED).unwrap());
        assert!(supported_attr_flags(ATTR_DISABLED | (1 << 8)).is_err());
    }

    #[test]
    fn hardware_abi_configs_are_distinct_and_linux_numbered() {
        assert_eq!(PERF_TYPE_HARDWARE, 0);
        assert_eq!(PERF_TYPE_SOFTWARE, 1);
        assert_eq!(PERF_COUNT_HW_CPU_CYCLES, 0);
        assert_eq!(PERF_COUNT_HW_INSTRUCTIONS, 1);
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct PerfEventAttrV0 {
    event_type: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}

// All fields are integer words and Linux's original ABI is exactly 64 bytes.
const _: () = assert!(size_of::<PerfEventAttrV0>() == PERF_ATTR_SIZE_VER0 as usize);

fn read_attr(
    memory: &UserMemoryCapability,
    attr: *const PerfEventAttrV0,
) -> AxResult<PerfEventAttrV0> {
    if attr.is_null() {
        return Err(AxError::BadAddress);
    };
    memory
        .read_value_uninit(attr)
        .map_err(map_usercopy_error)
        .map(|value| unsafe { value.assume_init() })
}

/// Returns the number of bytes following the v0 prefix that have to be
/// verified.  An extension is compatible only when every byte is zero.
fn attr_extension_len(size: u32) -> AxResult<usize> {
    if size < PERF_ATTR_SIZE_VER0 {
        return Err(AxError::InvalidInput);
    }
    if size > PERF_ATTR_MAX_SIZE {
        return Err(AxError::ArgumentListTooLong);
    }
    Ok(size as usize - PERF_ATTR_SIZE_VER0 as usize)
}

fn validate_extension_bytes(bytes: &[u8]) -> AxResult<()> {
    if bytes.iter().any(|&byte| byte != 0) {
        Err(AxError::ArgumentListTooLong)
    } else {
        Ok(())
    }
}

/// Checks the appended portion in fixed-size reads, preserving usercopy fault
/// reporting while avoiding a large stack allocation or an attacker-controlled
/// iteration count.
fn validate_attr_extensions(
    memory: &UserMemoryCapability,
    attr: *const PerfEventAttrV0,
    size: u32,
) -> AxResult<()> {
    let extension_len = attr_extension_len(size)?;
    let extension_start = (attr as usize)
        .checked_add(PERF_ATTR_SIZE_VER0 as usize)
        .ok_or(AxError::BadAddress)?;
    let mut extension = [core::mem::MaybeUninit::<u8>::uninit(); PERF_ATTR_EXTENSION_CHUNK_SIZE];
    let mut offset = 0;
    while offset < extension_len {
        let chunk_len = (extension_len - offset).min(extension.len());
        let address = extension_start
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        memory
            .read_bytes(address, &mut extension[..chunk_len])
            .map_err(map_usercopy_error)?;
        // SAFETY: read_bytes initialized every byte in the requested prefix.
        let bytes =
            unsafe { core::slice::from_raw_parts(extension.as_ptr().cast::<u8>(), chunk_len) };
        validate_extension_bytes(bytes)?;
        offset += chunk_len;
    }
    Ok(())
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
    let attr_value = read_attr(&memory, attr)?;
    validate_attr_extensions(&memory, attr, attr_value.size)?;
    let attr = attr_value;
    #[cfg(feature = "perf-sampling")]
    if attr.sample_period != 0 {
        return open_sampling(attr, pid, cpu, group_fd, flags);
    }
    let disabled = supported_attr_flags(attr.flags)?;
    if flags & !(PERF_FLAG_FD_NO_GROUP | PERF_FLAG_PID_CGROUP | PERF_FLAG_FD_CLOEXEC) != 0 {
        return Err(AxError::InvalidInput);
    }
    // A cgroup file descriptor has no meaning until cgroup perf attachment is
    // implemented; reject it before interpreting pid.
    if flags & PERF_FLAG_PID_CGROUP != 0 {
        return Err(AxError::OperationNotSupported);
    }
    if cpu != -1 {
        return Err(AxError::OperationNotSupported);
    }
    let target_is_current = pid == 0;
    let target = if target_is_current {
        current().clone()
    } else {
        if pid < 0 {
            return Err(AxError::InvalidInput);
        }
        get_visible_task(pid as u32)?
    };
    // perf's task attachment has ptrace-style credential access semantics.
    check_current_thread_ptrace_image_access(target.as_thread(), PtraceAccessMode::ReadReal)?;
    let target_task_id = target.id().as_u64();
    let (id, group) = if group_fd == -1 {
        // Reserve the ID before group construction so the group's immutable
        // leader identity and the created event cannot diverge.
        let id = NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed);
        (id, PerfGroup::new(target_task_id, id)?)
    } else {
        if flags & PERF_FLAG_FD_NO_GROUP != 0 {
            return Err(AxError::InvalidInput);
        }
        let leader = get_typed_file::<PerfEventFile>(group_fd)?;
        let Some(group) = leader.group() else {
            return Err(AxError::BadFileDescriptor);
        };
        if !leader.is_group_leader() || !group.accepts_target(target_task_id) {
            return Err(AxError::InvalidInput);
        }
        (NEXT_PERF_EVENT_ID.fetch_add(1, Ordering::Relaxed), group)
    };
    let event = match attr.event_type {
        PERF_TYPE_SOFTWARE => PerfEvent::Software(match attr.config {
            PERF_COUNT_SW_CPU_CLOCK => SoftwareEvent::CpuClock,
            PERF_COUNT_SW_TASK_CLOCK => SoftwareEvent::TaskClock,
            PERF_COUNT_SW_PAGE_FAULTS => SoftwareEvent::PageFaults,
            PERF_COUNT_SW_CONTEXT_SWITCHES => SoftwareEvent::ContextSwitches,
            _ => return Err(AxError::OperationNotSupported),
        }),
        PERF_TYPE_HARDWARE => {
            // Hardware leases are strictly local: opening another task would
            // either need a remote MSR operation or expose a stale sample.
            if !target_is_current {
                return Err(AxError::OperationNotSupported);
            }
            #[cfg(not(feature = "pmu"))]
            return Err(AxError::OperationNotSupported);
            #[cfg(feature = "pmu")]
            {
                if axhal::pmu::capabilities().is_err() {
                    return Err(AxError::OperationNotSupported);
                }
                PerfEvent::Hardware(match attr.config {
                    PERF_COUNT_HW_CPU_CYCLES => crate::file::HardwareEvent::Cycles,
                    PERF_COUNT_HW_INSTRUCTIONS => crate::file::HardwareEvent::Instructions,
                    _ => return Err(AxError::OperationNotSupported),
                })
            }
        }
        _ => return Err(AxError::OperationNotSupported),
    };
    // Sampling, output routing and read-format extensions are not fabricated.
    if attr.sample_period != 0
        || attr.sample_type != 0
        || attr.read_format & !crate::file::PERF_FORMAT_SUPPORTED != 0
    {
        return Err(AxError::OperationNotSupported);
    }
    // The target retains the only long-lived group Arc before the file takes
    // its weak back-reference. Every failure below removes an empty group.
    target.as_thread().attach_perf_group(group.clone())?;
    let file = match PerfEventFile::new(id, event, disabled, &group, attr.read_format) {
        Ok(file) => file,
        Err(error) => {
            target.as_thread().detach_empty_perf_group(&group);
            return Err(error);
        }
    };
    let result = add_file_like(
        file as Arc<dyn crate::file::FileLike>,
        flags & PERF_FLAG_FD_CLOEXEC != 0,
    );
    match result {
        Ok(fd) => {
            if target_is_current {
                group.reconfigure_current();
            }
            Ok(fd as isize)
        }
        Err(error) => {
            target.as_thread().detach_empty_perf_group(&group);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use axerrno::AxError;

    use super::{
        ATTR_DISABLED, PERF_ATTR_MAX_SIZE, PERF_ATTR_SIZE_VER0, attr_extension_len,
        supported_attr_flags, validate_extension_bytes,
    };
    use crate::file::PerfGroup;

    #[test]
    fn software_perf_accepts_only_disabled_attr_flag() {
        assert!(!supported_attr_flags(0).unwrap());
        assert!(supported_attr_flags(ATTR_DISABLED).unwrap());
        for unsupported in [1_u64 << 1, 1 << 2, 1 << 3, 1 << 4, 1 << 5] {
            assert!(supported_attr_flags(unsupported).is_err());
            assert!(supported_attr_flags(ATTR_DISABLED | unsupported).is_err());
        }
    }

    #[test]
    fn perf_group_binds_leader_and_target_task() {
        let group = PerfGroup::new(41, 7).unwrap();
        assert!(group.is_group_leader_for_test(7));
        assert!(!group.is_group_leader_for_test(8));
        assert!(group.accepts_target(41));
        assert!(!group.accepts_target(42));
    }

    #[test]
    fn perf_attr_extension_validator_accepts_only_zero_tail_with_bounded_size() {
        assert_eq!(attr_extension_len(PERF_ATTR_SIZE_VER0).unwrap(), 0);
        assert_eq!(attr_extension_len(PERF_ATTR_SIZE_VER0 + 1).unwrap(), 1);
        assert_eq!(attr_extension_len(PERF_ATTR_MAX_SIZE).unwrap(), 4032);
        assert!(attr_extension_len(PERF_ATTR_SIZE_VER0 - 1).is_err());
        assert_eq!(
            attr_extension_len(PERF_ATTR_MAX_SIZE + 1),
            Err(AxError::ArgumentListTooLong)
        );

        assert_eq!(validate_extension_bytes(&[0; 3]), Ok(()));
        assert_eq!(
            validate_extension_bytes(&[0, 0, 1]),
            Err(AxError::ArgumentListTooLong)
        );
    }
}
