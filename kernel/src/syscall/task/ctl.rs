use alloc::sync::Arc;
use core::ffi::c_char;

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::{
    __user_cap_data_struct, __user_cap_header_struct, _LINUX_CAPABILITY_VERSION_1,
    _LINUX_CAPABILITY_VERSION_2, _LINUX_CAPABILITY_VERSION_3, CAP_SETPCAP,
};
use starry_vm::{VmMutPtr, VmPtr, vm_write_slice};

use crate::{
    mm::vm_load_string,
    task::{AsThread, CapabilityState, ProcessData, get_process_data},
};

const NO_ID_CHANGE: u32 = u32::MAX;

fn cap_data_words(version: u32) -> usize {
    match version {
        _LINUX_CAPABILITY_VERSION_1 => 1,
        _ => 2,
    }
}

fn cap_set_subset(lhs: [u32; 2], rhs: [u32; 2]) -> bool {
    lhs.iter().zip(rhs.iter()).all(|(lhs, rhs)| lhs & !rhs == 0)
}

fn cap_set_union(lhs: [u32; 2], rhs: [u32; 2]) -> [u32; 2] {
    [lhs[0] | rhs[0], lhs[1] | rhs[1]]
}

fn validate_cap_header(
    header_ptr: *mut __user_cap_header_struct,
) -> AxResult<(__user_cap_header_struct, Arc<ProcessData>)> {
    // FIXME: AnyBitPattern
    let mut header = unsafe { header_ptr.vm_read_uninit()?.assume_init() };
    if !matches!(
        header.version,
        _LINUX_CAPABILITY_VERSION_1 | _LINUX_CAPABILITY_VERSION_2 | _LINUX_CAPABILITY_VERSION_3
    ) {
        header.version = _LINUX_CAPABILITY_VERSION_3;
        header_ptr.vm_write(header)?;
        return Err(AxError::InvalidInput);
    }
    if header.pid < 0 {
        return Err(AxError::InvalidInput);
    }
    let proc_data = get_process_data(header.pid as u32)?;
    Ok((header, proc_data))
}

fn write_cap_data(
    data: *mut __user_cap_data_struct,
    version: u32,
    state: CapabilityState,
) -> AxResult<()> {
    for index in 0..cap_data_words(version) {
        data.wrapping_add(index).vm_write(__user_cap_data_struct {
            effective: state.effective[index],
            permitted: state.permitted[index],
            inheritable: state.inheritable[index],
        })?;
    }
    Ok(())
}

fn read_cap_data(data: *mut __user_cap_data_struct, version: u32) -> AxResult<CapabilityState> {
    let mut state = CapabilityState {
        effective: [0; 2],
        permitted: [0; 2],
        inheritable: [0; 2],
        bounding: [0; 2],
        securebits: 0,
    };

    for index in 0..cap_data_words(version) {
        let entry: __user_cap_data_struct =
            unsafe { data.wrapping_add(index).vm_read_uninit()?.assume_init() };
        state.effective[index] = entry.effective;
        state.permitted[index] = entry.permitted;
        state.inheritable[index] = entry.inheritable;
    }
    Ok(state)
}

pub fn sys_capget(
    header: *mut __user_cap_header_struct,
    data: *mut __user_cap_data_struct,
) -> AxResult<isize> {
    let (header, proc_data) = validate_cap_header(header)?;
    write_cap_data(data, header.version, proc_data.capability_state())?;
    Ok(0)
}

pub fn sys_capset(
    header: *mut __user_cap_header_struct,
    data: *mut __user_cap_data_struct,
) -> AxResult<isize> {
    let curr = current();
    let current_pid = curr.as_thread().proc_data.proc.pid();
    let (header, proc_data) = validate_cap_header(header)?;
    if header.pid != 0 && header.pid as u32 != current_pid {
        return Err(AxError::OperationNotPermitted);
    }

    let new_state = read_cap_data(data, header.version)?;
    let mut old_state = proc_data.capability_state();

    if !cap_set_subset(new_state.effective, new_state.permitted) {
        return Err(AxError::OperationNotPermitted);
    }
    if !cap_set_subset(new_state.permitted, old_state.permitted) {
        return Err(AxError::OperationNotPermitted);
    }

    let allowed_inheritable = if old_state.has_effective(CAP_SETPCAP) {
        cap_set_union(old_state.inheritable, old_state.bounding)
    } else {
        cap_set_union(old_state.inheritable, old_state.permitted)
    };
    if !cap_set_subset(new_state.inheritable, allowed_inheritable) {
        return Err(AxError::OperationNotPermitted);
    }

    old_state.effective = new_state.effective;
    old_state.permitted = new_state.permitted;
    old_state.inheritable = new_state.inheritable;
    proc_data.set_capability_state(old_state);

    Ok(0)
}

pub fn sys_umask(mask: u32) -> AxResult<isize> {
    let curr = current();
    let old = curr.as_thread().proc_data.replace_umask(mask);
    Ok(old as isize)
}

pub fn sys_setreuid(ruid: u32, euid: u32) -> AxResult<isize> {
    current().as_thread().proc_data.setreuid(
        (ruid != NO_ID_CHANGE).then_some(ruid),
        (euid != NO_ID_CHANGE).then_some(euid),
    )?;
    Ok(0)
}

pub fn sys_setregid(rgid: u32, egid: u32) -> AxResult<isize> {
    current().as_thread().proc_data.setregid(
        (rgid != NO_ID_CHANGE).then_some(rgid),
        (egid != NO_ID_CHANGE).then_some(egid),
    )?;
    Ok(0)
}

pub fn sys_setresuid(ruid: u32, euid: u32, suid: u32) -> AxResult<isize> {
    current().as_thread().proc_data.setresuid(
        (ruid != NO_ID_CHANGE).then_some(ruid),
        (euid != NO_ID_CHANGE).then_some(euid),
        (suid != NO_ID_CHANGE).then_some(suid),
    )?;
    Ok(0)
}

pub fn sys_setresgid(rgid: u32, egid: u32, sgid: u32) -> AxResult<isize> {
    current().as_thread().proc_data.setresgid(
        (rgid != NO_ID_CHANGE).then_some(rgid),
        (egid != NO_ID_CHANGE).then_some(egid),
        (sgid != NO_ID_CHANGE).then_some(sgid),
    )?;
    Ok(0)
}

pub fn sys_get_mempolicy(
    _policy: *mut i32,
    _nodemask: *mut usize,
    _maxnode: usize,
    _addr: usize,
    _flags: usize,
) -> AxResult<isize> {
    warn!("Dummy get_mempolicy called");
    Ok(0)
}

/// prctl() is called with a first argument describing what to do, and further
/// arguments with a significance depending on the first one.
/// The first argument can be:
/// - PR_SET_NAME: set the name of the calling thread, using the value pointed to by `arg2`
/// - PR_GET_NAME: get the name of the calling
/// - PR_SET_SECCOMP: enable seccomp mode, with the mode specified in `arg2`
/// - PR_MCE_KILL: set the machine check exception policy
/// - PR_SET_MM options: set various memory management options (start/end code/data/brk/stack)
pub fn sys_prctl(
    option: u32,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> AxResult<isize> {
    use linux_raw_sys::prctl::*;

    debug!("sys_prctl <= option: {option}, args: {arg2}, {arg3}, {arg4}, {arg5}");

    match option {
        PR_SET_NAME => {
            let s = vm_load_string(arg2 as *const c_char)?;
            current().set_name(&s);
        }
        PR_GET_NAME => {
            let name = current().name();
            let len = name.len().min(15);
            let mut buf = [0; 16];
            buf[..len].copy_from_slice(&name.as_bytes()[..len]);
            vm_write_slice(arg2 as _, &buf)?;
        }
        PR_SET_SECCOMP => {}
        PR_MCE_KILL => {}
        PR_CAPBSET_DROP => {
            let curr = current();
            let proc_data = &curr.as_thread().proc_data;
            if !proc_data.has_effective_capability(CAP_SETPCAP) {
                return Err(AxError::OperationNotPermitted);
            }
            proc_data.drop_bounding_capability(arg2 as u32)?;
        }
        PR_CAPBSET_READ => {
            return Ok(current()
                .as_thread()
                .proc_data
                .bounding_capability_enabled(arg2 as u32) as isize);
        }
        PR_SET_SECUREBITS => {
            if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            let curr = current();
            let proc_data = &curr.as_thread().proc_data;
            if !proc_data.has_effective_capability(CAP_SETPCAP) {
                return Err(AxError::OperationNotPermitted);
            }
            proc_data.set_securebits(arg2 as u32);
        }
        PR_SET_MM => {
            // not implemented; but avoid annoying warnings
            return Err(AxError::InvalidInput);
        }
        _ => {
            warn!("sys_prctl: unsupported option {option}");
            return Err(AxError::InvalidInput);
        }
    }

    Ok(0)
}
