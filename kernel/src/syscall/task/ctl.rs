use alloc::sync::Arc;
use core::ffi::c_char;

use axerrno::{AxError, AxResult};
use axhal::paging::MappingFlags;
use axtask::current;
use linux_raw_sys::{
    general::{
        __user_cap_data_struct, __user_cap_header_struct, _LINUX_CAPABILITY_VERSION_1,
        _LINUX_CAPABILITY_VERSION_2, _LINUX_CAPABILITY_VERSION_3, CAP_SETPCAP,
    },
    mempolicy::*,
};
use memory_addr::{MemoryAddr, VirtAddr};
use starry_vm::{VmMutPtr, VmPtr, vm_write_slice};

use crate::{
    mm::vm_load_string,
    task::{AsThread, CapabilityState, Mempolicy, ProcessData, get_process_data},
};

const NO_ID_CHANGE: u32 = u32::MAX;
const ALLOWED_NODEMASK: usize = 1;
const SUPPORTED_GET_MEMPOLICY_FLAGS: usize =
    (MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED) as usize;
const SUPPORTED_MODE_FLAGS: usize = MPOL_MODE_FLAGS as usize;
const SUPPORTED_MBIND_FLAGS: usize = MPOL_MF_VALID as usize;

fn mempolicy_mode(mode_with_flags: usize) -> u32 {
    (mode_with_flags & !SUPPORTED_MODE_FLAGS) as u32
}

fn mempolicy_mode_flags(mode_with_flags: usize) -> usize {
    mode_with_flags & SUPPORTED_MODE_FLAGS
}

fn read_nodemask(nodemask: *const usize, maxnode: usize) -> AxResult<usize> {
    if nodemask.is_null() || maxnode == 0 {
        return Ok(0);
    }

    let words = maxnode.div_ceil(usize::BITS as usize);
    let mut result = 0usize;
    for index in 0..words {
        let word = nodemask.wrapping_add(index).vm_read()?;
        if index == 0 {
            result = word;
        } else if word != 0 {
            return Err(AxError::InvalidInput);
        }
    }

    if maxnode < usize::BITS as usize {
        let mask = (1usize << maxnode) - 1;
        if result & !mask != 0 {
            return Err(AxError::InvalidInput);
        }
        result &= mask;
    }

    Ok(result)
}

fn write_nodemask(nodemask: *mut usize, maxnode: usize, value: usize) -> AxResult<()> {
    if nodemask.is_null() || maxnode == 0 {
        return Ok(());
    }

    let words = maxnode.div_ceil(usize::BITS as usize);
    for index in 0..words {
        let word = if index == 0 { value } else { 0 };
        nodemask.wrapping_add(index).vm_write(word)?;
    }
    Ok(())
}

fn validate_mempolicy(mode_with_flags: usize, nodemask: usize) -> AxResult<Mempolicy> {
    let mode = mempolicy_mode(mode_with_flags);
    if mempolicy_mode_flags(mode_with_flags) & MPOL_F_NUMA_BALANCING as usize != 0
        && mode != MPOL_BIND as u32
    {
        return Err(AxError::InvalidInput);
    }

    if nodemask & !ALLOWED_NODEMASK != 0 {
        return Err(AxError::InvalidInput);
    }

    let needs_nodes = matches!(mode, mode if mode == MPOL_BIND as u32 || mode == MPOL_INTERLEAVE as u32 || mode == MPOL_PREFERRED_MANY as u32);
    if needs_nodes && nodemask == 0 {
        return Err(AxError::InvalidInput);
    }

    match mode {
        mode if mode == MPOL_DEFAULT as u32 => {
            if nodemask != 0 {
                return Err(AxError::InvalidInput);
            }
            Ok(Mempolicy::new(mode, 0))
        }
        mode if mode == MPOL_PREFERRED as u32 => Ok(Mempolicy::new(mode, nodemask)),
        mode if mode == MPOL_BIND as u32
            || mode == MPOL_INTERLEAVE as u32
            || mode == MPOL_LOCAL as u32
            || mode == MPOL_PREFERRED_MANY as u32 =>
        {
            if mode == MPOL_LOCAL as u32 && nodemask != 0 {
                return Err(AxError::InvalidInput);
            }
            Ok(Mempolicy::new(mode, nodemask))
        }
        _ => Err(AxError::InvalidInput),
    }
}

fn validate_mapped_user_range(start: usize, size: usize) -> AxResult<(VirtAddr, usize)> {
    let start = VirtAddr::from(start);
    if size == 0 {
        return Ok((start, 0));
    }
    let end = start.checked_add(size).ok_or(AxError::InvalidInput)?;
    let aligned_start = start.align_down_4k();
    let aligned_end = end.align_up_4k();
    let aligned_size = aligned_end.sub_addr(aligned_start);
    let curr = current();
    let aspace_handle = curr.as_thread().proc_data.aspace();
    let aspace = aspace_handle.lock();
    if !aspace.can_access_range(aligned_start, aligned_size, MappingFlags::USER) {
        return Err(AxError::BadAddress);
    }
    Ok((aligned_start, aligned_size))
}

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
    policy: *mut i32,
    nodemask: *mut usize,
    maxnode: usize,
    addr: usize,
    flags: usize,
) -> AxResult<isize> {
    if flags & !SUPPORTED_GET_MEMPOLICY_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    if flags & MPOL_F_MEMS_ALLOWED as usize != 0 {
        if flags & (MPOL_F_ADDR | MPOL_F_NODE) as usize != 0 {
            return Err(AxError::InvalidInput);
        }
        write_nodemask(nodemask, maxnode, ALLOWED_NODEMASK)?;
        return Ok(0);
    }
    if addr != 0 && flags & MPOL_F_ADDR as usize == 0 {
        return Err(AxError::InvalidInput);
    }

    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    let selected = if flags & MPOL_F_ADDR as usize != 0 {
        let addr = VirtAddr::from(addr);
        let aspace_handle = proc_data.aspace();
        if aspace_handle.lock().find_area(addr).is_none() {
            return Err(AxError::BadAddress);
        }
        proc_data
            .mempolicy_for_addr(addr.as_usize())
            .unwrap_or_else(|| Mempolicy::new(MPOL_DEFAULT as u32, 0))
    } else {
        proc_data.mempolicy()
    };

    if flags & MPOL_F_NODE as usize != 0 {
        if !nodemask.is_null() {
            return Err(AxError::InvalidInput);
        }
        if !policy.is_null() {
            policy.vm_write(0)?;
        }
        return Ok(0);
    }

    if !policy.is_null() {
        policy.vm_write(selected.mode as i32)?;
    }
    let returned_nodemask = if flags & MPOL_F_ADDR as usize == 0 && selected.nodemask == 0 {
        ALLOWED_NODEMASK
    } else {
        selected.nodemask
    };
    write_nodemask(nodemask, maxnode, returned_nodemask)?;
    Ok(0)
}

pub fn sys_set_mempolicy(mode: usize, nodemask: *const usize, maxnode: usize) -> AxResult<isize> {
    if mempolicy_mode_flags(mode) & !SUPPORTED_MODE_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let nodemask = read_nodemask(nodemask, maxnode)?;
    let policy = validate_mempolicy(mode, nodemask)?;
    current().as_thread().proc_data.set_mempolicy(policy);
    Ok(0)
}

pub fn sys_mbind(
    start: usize,
    len: usize,
    mode: usize,
    nodemask: *const usize,
    maxnode: usize,
    flags: usize,
) -> AxResult<isize> {
    if flags & !SUPPORTED_MBIND_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }
    let (start, len) = validate_mapped_user_range(start, len)?;
    let nodemask = read_nodemask(nodemask, maxnode)?;
    let policy = validate_mempolicy(mode, nodemask)?;
    current()
        .as_thread()
        .proc_data
        .bind_mempolicy_range(start.as_usize(), len, policy);
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
        PR_SET_PDEATHSIG => {
            if arg2 > 64 {
                return Err(AxError::InvalidInput);
            }
            current()
                .as_thread()
                .proc_data
                .set_pdeath_signal(arg2 as u32);
        }
        PR_GET_PDEATHSIG => {
            (arg2 as *mut i32).vm_write(current().as_thread().proc_data.pdeath_signal() as i32)?;
        }
        PR_SET_TIMERSLACK => {
            current().as_thread().proc_data.set_timerslack_ns(arg2);
        }
        PR_GET_TIMERSLACK => {
            return Ok(current().as_thread().proc_data.timerslack_ns() as isize);
        }
        PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            current().as_thread().proc_data.set_no_new_privs();
        }
        PR_GET_NO_NEW_PRIVS => {
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(AxError::InvalidInput);
            }
            return Ok(current().as_thread().proc_data.no_new_privs() as isize);
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
