use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::{MappingFlags, PageSize};
use axtask::current;
use memory_addr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};

use crate::{
    mm::{
        AddrSpace, Backend, UserMemoryCapability,
        ldt::{BYTES as LDT_BYTES, UserDesc},
        map_usercopy_error,
    },
    task::AsThread,
};

pub fn sys_modify_ldt(
    memory: UserMemoryCapability,
    func: i32,
    ptr: *mut u8,
    bytes: usize,
) -> AxResult<isize> {
    const ZERO: [u8; 128] = [0; 128];

    match func {
        0 => {
            let Some(table) = memory.address_space().lock().ldt_snapshot() else {
                // Linux reports an uninitialized LDT as empty without
                // validating or touching the destination.
                return Ok(0);
            };
            let bytes = bytes.min(LDT_BYTES);
            let copied = table.bytes().len().min(bytes);
            if copied != 0 {
                memory
                    .write_bytes(ptr as usize, &table.bytes()[..copied])
                    .map_err(map_usercopy_error)?;
            }
            let mut offset = copied;
            while offset < bytes {
                let chunk = (bytes - offset).min(ZERO.len());
                memory
                    .write_bytes(ptr.wrapping_add(offset) as usize, &ZERO[..chunk])
                    .map_err(map_usercopy_error)?;
                offset += chunk;
            }
            Ok(bytes as isize)
        }
        2 => {
            let bytes = bytes.min(ZERO.len());
            let mut offset = 0;
            while offset < bytes {
                let chunk = (bytes - offset).min(ZERO.len());
                memory
                    .write_bytes(ptr.wrapping_add(offset) as usize, &ZERO[..chunk])
                    .map_err(map_usercopy_error)?;
                offset += chunk;
            }
            Ok(bytes as isize)
        }
        1 | 0x11 => {
            if bytes != core::mem::size_of::<UserDesc>() {
                return Err(AxError::InvalidInput);
            }
            let info = memory.read_value(ptr.cast()).map_err(map_usercopy_error)?;
            memory
                .address_space()
                .lock()
                .replace_ldt_entry(info, func == 1)?;
            Ok(0)
        }
        _ => Err(axerrno::LinuxError::ENOSYS.into()),
    }
}

pub fn sys_getpid() -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    Ok(proc_data.pid_ns().visible_pid(proc_data.proc.pid()) as _)
}

pub fn sys_getppid() -> AxResult<isize> {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    Ok(render_parent_pid(
        proc_data.proc.parent().as_deref(),
        &proc_data.pid_ns(),
    ))
}

fn render_parent_pid<C, R>(
    parent: Option<&thekernel_linux_process_adapter::Process<C, R>>,
    caller_pid_ns: &crate::task::PidNamespace,
) -> isize {
    let Some(parent) = parent else {
        return 0;
    };
    let Some(parent_pid_ns) = parent.identity::<alloc::sync::Arc<crate::task::PidNamespace>>()
    else {
        return 0;
    };
    caller_pid_ns
        .visible_pid_for(parent_pid_ns, parent.pid())
        .unwrap_or(0) as _
}

pub fn sys_gettid() -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    Ok(thread.proc_data.pid_ns().visible_pid(thread.tid()) as _)
}

/// ARCH_PRCTL codes
///
/// It is only avaliable on x86_64, and is not convenient
/// to generate automatically via c_to_rust binding.
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Eq, PartialEq, num_enum::TryFromPrimitive)]
#[repr(i32)]
enum ArchPrctlCode {
    /// Set the GS segment base
    SetGs        = 0x1001,
    /// Set the FS segment base
    SetFs        = 0x1002,
    /// Get the FS segment base
    GetFs        = 0x1003,
    /// Get the GS segment base
    GetGs        = 0x1004,
    /// The setting of the flag manipulated by ARCH_SET_CPUID
    GetCpuid     = 0x1011,
    /// Enable (addr != 0) or disable (addr == 0) the cpuid instruction for the
    /// calling thread.
    SetCpuid     = 0x1012,
    EnableShstk  = 0x5001,
    DisableShstk = 0x5002,
    LockShstk    = 0x5003,
    StatusShstk  = 0x5005,
}

#[cfg(target_arch = "x86_64")]
const ARCH_SHSTK_SHSTK: usize = 1;
#[cfg(target_arch = "x86_64")]
const CET_SHSTK_EN: u64 = 1;
#[cfg(target_arch = "x86_64")]
const CET_DEFAULT_SHSTK_SIZE: usize = crate::config::USER_STACK_SIZE;
#[cfg(target_arch = "x86_64")]
const CET_SHSTK_MIN_ADDR: usize = 1usize << 32;

/// Install the anonymous stack used by ARCH_SHSTK_ENABLE.  The lower page is
/// intentionally left unmapped: it is the overflow guard, not a VMA whose
/// lifetime can accidentally be detached from the task record.
#[cfg(target_arch = "x86_64")]
pub(crate) fn map_cet_default_shadow_stack(
    aspace: &mut AddrSpace,
    task_id: u32,
) -> AxResult<axhal::asm::UserCetState> {
    let total = CET_DEFAULT_SHSTK_SIZE
        .checked_add(PAGE_SIZE_4K)
        .ok_or(AxError::NoMemory)?;
    let base = aspace
        .find_kernel_area(
            VirtAddr::from(CET_SHSTK_MIN_ADDR),
            total,
            VirtAddrRange::new(aspace.base(), aspace.end()),
            PAGE_SIZE_4K,
        )
        .ok_or(AxError::NoMemory)?;
    let start = base + PAGE_SIZE_4K;
    let result = (|| {
        aspace.map(
            start,
            CET_DEFAULT_SHSTK_SIZE,
            MappingFlags::USER | MappingFlags::READ | MappingFlags::SHADOW_STACK,
            false,
            Backend::new_alloc(start, PageSize::Size4K),
        )?;
        // A restore token at the architectural top is required before CET is
        // made visible; an enabled task must never enter userspace with only
        // U_CET set and no valid PL3_SSP landing stack.
        aspace.populate_area(start, CET_DEFAULT_SHSTK_SIZE, MappingFlags::READ)?;
        let token = start.as_usize() + CET_DEFAULT_SHSTK_SIZE - core::mem::size_of::<u64>();
        aspace.write(
            VirtAddr::from(token),
            &((token + 8) as u64 | 1).to_ne_bytes(),
        )?;
        aspace.register_cet_default_shadow_stack(task_id, start, CET_DEFAULT_SHSTK_SIZE)?;
        Ok(axhal::asm::UserCetState {
            u_cet: CET_SHSTK_EN,
            pl3_ssp: (token + 8) as u64,
            locked: false,
        })
    })();
    if result.is_err() {
        if let Ok(wake) = aspace.unmap(start, CET_DEFAULT_SHSTK_SIZE) {
            wake.finish();
        }
    }
    result
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn unmap_cet_default_shadow_stack(aspace: &mut AddrSpace, task_id: u32) {
    if let Some(owner) = aspace.take_cet_default_shadow_stack(task_id)
        && let Ok(wake) = aspace.unmap(owner.start, owner.size)
    {
        wake.finish();
    }
}

/// To set the clear_child_tid field in the task extended data.
///
/// The set_tid_address() always succeeds
pub fn sys_set_tid_address(clear_child_tid: usize) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    thread.set_clear_child_tid(clear_child_tid);
    Ok(thread.proc_data.pid_ns().visible_pid(thread.tid()) as isize)
}

#[cfg(test)]
mod tests {
    use super::render_parent_pid;
    use crate::task::{PidNamespace, UserNamespace};

    #[test]
    fn getppid_parent_identity_obeys_pid_namespace_visibility() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let root_pid_ns = PidNamespace::try_new_root(user_ns.clone()).unwrap();
        let child_pid_ns = root_pid_ns.try_fork(20, user_ns).unwrap();
        let domain = thekernel_linux_process_adapter::ProcessDomain::<()>::try_new().unwrap();
        let init = domain
            .try_new_init_with_identity(1, None, root_pid_ns.clone())
            .unwrap();
        let _child = domain
            .prepare_fork_with_identity(&init, 20, None, child_pid_ns.clone())
            .unwrap();

        assert_eq!(render_parent_pid::<(), ()>(None, &root_pid_ns), 0);
        assert_eq!(render_parent_pid(Some(&init), &root_pid_ns), 1);
        assert_eq!(render_parent_pid(Some(&init), &child_pid_ns), 0);
        assert_eq!(root_pid_ns.visible_pid_for(&child_pid_ns, 20), Some(20));
    }
}

#[cfg(target_arch = "x86_64")]
pub fn sys_arch_prctl(
    memory: UserMemoryCapability,
    uctx: &mut axhal::uspace::UserContext,
    code: i32,
    addr: usize,
) -> AxResult<isize> {
    let code = ArchPrctlCode::try_from(code).map_err(|_| AxError::InvalidInput)?;
    debug!("sys_arch_prctl: code = {code:?}, addr = {addr:#x}");

    match code {
        // According to Linux implementation, SetFs & SetGs does not return
        // error at all
        ArchPrctlCode::GetFs => {
            memory
                .write_value(addr as *mut usize, uctx.tls())
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
        ArchPrctlCode::SetFs => {
            uctx.set_tls(addr);
            Ok(0)
        }
        ArchPrctlCode::GetGs => {
            memory
                .write_value(addr as *mut usize, uctx.gs_base as _)
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
        ArchPrctlCode::SetGs => {
            uctx.gs_base = addr as _;
            Ok(0)
        }
        ArchPrctlCode::GetCpuid => Ok(1),
        ArchPrctlCode::SetCpuid if addr != 0 => Ok(0),
        ArchPrctlCode::SetCpuid => Err(axerrno::AxError::NoSuchDevice),
        ArchPrctlCode::EnableShstk => {
            if addr != ARCH_SHSTK_SHSTK {
                return Err(AxError::InvalidInput);
            }
            if !axhal::asm::user_shadow_stack_enabled() {
                return Err(AxError::NoSuchDevice);
            }
            let mut state = crate::task::current_user_live_cet_state();
            if state.locked && state.u_cet & CET_SHSTK_EN == 0 {
                return Err(LinuxError::EPERM.into());
            }
            if state.u_cet & CET_SHSTK_EN != 0 {
                return Ok(0);
            }
            let curr = current();
            let thread = curr.as_thread();
            let aspace_handle = thread.proc_data.aspace();
            state = map_cet_default_shadow_stack(&mut aspace_handle.lock(), thread.kernel_tid())?;
            crate::task::set_current_user_cet_state(state);
            Ok(0)
        }
        ArchPrctlCode::DisableShstk => {
            if addr != ARCH_SHSTK_SHSTK {
                return Err(AxError::InvalidInput);
            }
            let mut state = crate::task::current_user_live_cet_state();
            if state.locked && state.u_cet & CET_SHSTK_EN != 0 {
                return Err(LinuxError::EPERM.into());
            }
            if state.u_cet & CET_SHSTK_EN == 0 {
                return Ok(0);
            }
            let curr = current();
            let thread = curr.as_thread();
            let aspace_handle = thread.proc_data.aspace();
            unmap_cet_default_shadow_stack(&mut aspace_handle.lock(), thread.kernel_tid());
            thread.clear_cet_signal_frames();
            state = axhal::asm::UserCetState::default();
            crate::task::set_current_user_cet_state(state);
            Ok(0)
        }
        ArchPrctlCode::LockShstk => {
            if addr == 0 || addr & !ARCH_SHSTK_SHSTK != 0 {
                return Err(AxError::InvalidInput);
            }
            let mut state = crate::task::current_user_live_cet_state();
            state.locked = true;
            crate::task::set_current_user_cet_state(state);
            Ok(0)
        }
        ArchPrctlCode::StatusShstk => {
            let state = crate::task::current_user_live_cet_state();
            memory
                .write_value(addr as *mut usize, (state.u_cet & CET_SHSTK_EN) as usize)
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
    }
}
