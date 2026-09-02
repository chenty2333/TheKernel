use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::{MappingFlags, PageSize};
use axtask::current;
use memory_addr::{PAGE_SIZE_4K, VirtAddr, VirtAddrRange};

use crate::{
    mm::{
        AddrSpace, Backend, UserMemoryCapability, check_rlimit_as_growth,
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
    let thread = curr.as_thread();
    Ok(thread.pid_ns().visible_pid(thread.proc_data.proc.pid()) as _)
}

pub fn sys_getppid() -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    let proc_data = &thread.proc_data;
    Ok(render_parent_pid(
        proc_data.proc.parent().as_deref(),
        &thread.pid_ns(),
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
    Ok(thread.pid_ns().visible_pid(thread.tid()) as _)
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
    UnlockShstk  = 0x5004,
    StatusShstk  = 0x5005,
}

#[cfg(target_arch = "x86_64")]
const ARCH_SHSTK_SHSTK: usize = 1;
#[cfg(target_arch = "x86_64")]
const ARCH_SHSTK_WRSS: usize = 2;
#[cfg(target_arch = "x86_64")]
const CET_SHSTK_EN: u64 = 1;
#[cfg(target_arch = "x86_64")]
const CET_WRSS_EN: u64 = 2;
#[cfg(target_arch = "x86_64")]
const CET_DEFAULT_SHSTK_MAX_SIZE: u64 = 4 * 1024 * 1024 * 1024;
#[cfg(target_arch = "x86_64")]
const CET_SHSTK_MIN_ADDR: usize = 1usize << 32;

/// Selects the size of one automatically allocated user shadow stack.
///
/// Linux uses `PAGE_ALIGN(min(rlimit(RLIMIT_STACK), 4GiB))` for a default
/// allocation and page-aligns a non-zero clone3 `stack_size` verbatim.  Keep
/// that decision pure so every fallible VMA operation happens only after the
/// size is known to fit in this kernel's `usize` address-space representation.
#[cfg(target_arch = "x86_64")]
pub(crate) fn cet_default_shadow_stack_size(
    clone3_stack_size: usize,
    rlimit_stack: u64,
) -> AxResult<usize> {
    let requested = if clone3_stack_size == 0 {
        rlimit_stack.min(CET_DEFAULT_SHSTK_MAX_SIZE)
    } else {
        clone3_stack_size as u64
    };
    let requested = usize::try_from(requested).map_err(|_| AxError::NoMemory)?;
    if requested == 0 {
        return Err(AxError::NoMemory);
    }
    requested
        .checked_add(PAGE_SIZE_4K - 1)
        .map(|size| size & !(PAGE_SIZE_4K - 1))
        .filter(|&size| size != 0)
        .ok_or(AxError::NoMemory)
}

/// Install the anonymous stack used by ARCH_SHSTK_ENABLE.  The lower page is
/// intentionally left unmapped: it is the overflow guard, not a VMA whose
/// lifetime can accidentally be detached from the task record.
#[cfg(target_arch = "x86_64")]
pub(crate) fn map_cet_default_shadow_stack(
    proc_data: &crate::task::ProcessData,
    aspace: &mut AddrSpace,
    task_id: u32,
    size: usize,
) -> AxResult<axhal::asm::UserCetState> {
    let total = size.checked_add(PAGE_SIZE_4K).ok_or(AxError::NoMemory)?;
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
        check_rlimit_as_growth(proc_data, aspace, size)?;
        aspace.map(
            start,
            size,
            MappingFlags::USER
                | MappingFlags::READ
                | MappingFlags::WRITE
                | MappingFlags::SHADOW_STACK,
            false,
            Backend::new_alloc(start, PageSize::Size4K),
        )?;
        aspace.register_cet_default_shadow_stack(task_id, start, size)?;
        Ok(axhal::asm::UserCetState {
            u_cet: CET_SHSTK_EN,
            // ARCH_SHSTK_ENABLE's default stack does not install a restore
            // token (Linux set_res_tok=false); the top itself is the SSP.
            pl3_ssp: (start.as_usize() + size) as u64,
            locked: 0,
        })
    })();
    if result.is_err()
        && let Ok(wake) = aspace.unmap(start, size)
    {
        wake.finish();
    }
    result
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn unmap_cet_default_shadow_stack(
    aspace: &mut AddrSpace,
    task_id: u32,
) -> crate::mm::DeferredUffdWake {
    aspace.retire_cet_default_shadow_stack(task_id)
}

/// To set the clear_child_tid field in the task extended data.
///
/// The set_tid_address() always succeeds
pub fn sys_set_tid_address(clear_child_tid: usize) -> AxResult<isize> {
    let curr = current();
    let thread = curr.as_thread();
    thread.set_clear_child_tid(clear_child_tid);
    Ok(thread.pid_ns().visible_pid(thread.tid()) as isize)
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
            if addr != ARCH_SHSTK_SHSTK && addr != ARCH_SHSTK_WRSS {
                return Err(AxError::InvalidInput);
            }
            if !axhal::asm::user_shadow_stack_enabled() {
                // CET is one all-online-CPU ABI.  Do not distinguish a
                // fleet rejection from map_shadow_stack(2) or ptrace's CET
                // regset path: none of them can expose a partially enabled
                // user-CET contract.
                return Err(AxError::OperationNotSupported);
            }
            let mut state = crate::task::current_user_live_cet_state();
            let feature = if addr == ARCH_SHSTK_SHSTK {
                CET_SHSTK_EN
            } else {
                CET_WRSS_EN
            };
            if state.locked & feature != 0 && state.u_cet & feature == 0 {
                return Err(LinuxError::EPERM.into());
            }
            if state.u_cet & feature != 0 {
                return Ok(0);
            }
            // Linux only enables WRSS when user shadow stacks are active.
            if feature == CET_WRSS_EN && state.u_cet & CET_SHSTK_EN == 0 {
                return Err(AxError::InvalidInput);
            }
            if feature == CET_WRSS_EN {
                state.u_cet |= CET_WRSS_EN;
                crate::task::set_current_user_cet_state(state);
                return Ok(0);
            }
            let curr = current();
            let thread = curr.as_thread();
            let aspace_handle = thread.proc_data.aspace();
            let locked = state.locked;
            let size = cet_default_shadow_stack_size(
                0,
                thread.proc_data.rlim.read()[linux_raw_sys::general::RLIMIT_STACK].current,
            )?;
            state = map_cet_default_shadow_stack(
                &thread.proc_data,
                &mut aspace_handle.lock(),
                thread.kernel_tid(),
                size,
            )?;
            // Mapping returns architectural enable/SSP state; ARCH_SHSTK_LOCK
            // belongs to the task and survives enabling another feature.
            state.locked = locked;
            crate::task::set_current_user_cet_state(state);
            Ok(0)
        }
        ArchPrctlCode::DisableShstk => {
            if addr != ARCH_SHSTK_SHSTK && addr != ARCH_SHSTK_WRSS {
                return Err(AxError::InvalidInput);
            }
            if !axhal::asm::user_shadow_stack_enabled() {
                return Err(AxError::OperationNotSupported);
            }
            let mut state = crate::task::current_user_live_cet_state();
            let feature = if addr == ARCH_SHSTK_SHSTK {
                CET_SHSTK_EN
            } else {
                CET_WRSS_EN
            };
            if state.locked & feature != 0 && state.u_cet & feature != 0 {
                return Err(LinuxError::EPERM.into());
            }
            if state.u_cet & feature == 0 {
                return Ok(0);
            }
            if feature == CET_WRSS_EN {
                state.u_cet &= !CET_WRSS_EN;
                crate::task::set_current_user_cet_state(state);
                return Ok(0);
            }
            // WRSS is dependent on SHSTK. Disabling SHSTK removes both bits,
            // unless the separately locked WRSS feature forbids that change.
            if state.u_cet & CET_WRSS_EN != 0 && state.locked & CET_WRSS_EN != 0 {
                return Err(LinuxError::EPERM.into());
            }
            let curr = current();
            let thread = curr.as_thread();
            let aspace_handle = thread.proc_data.aspace();
            let wake =
                { unmap_cet_default_shadow_stack(&mut aspace_handle.lock(), thread.kernel_tid()) };
            wake.finish();
            state.u_cet = 0;
            state.pl3_ssp = 0;
            crate::task::set_current_user_cet_state(state);
            Ok(0)
        }
        ArchPrctlCode::LockShstk => {
            if addr == 0 || addr & !(ARCH_SHSTK_SHSTK | ARCH_SHSTK_WRSS) != 0 {
                return Err(AxError::InvalidInput);
            }
            if !axhal::asm::user_shadow_stack_enabled() {
                return Err(AxError::OperationNotSupported);
            }
            let mut state = crate::task::current_user_live_cet_state();
            state.locked |= addr as u64;
            crate::task::set_current_user_cet_state(state);
            Ok(0)
        }
        // Linux reserves unlock for a tracer-mediated state change.  A task
        // cannot discard its own lock through arch_prctl; ptrace uses the
        // stopped-task CET state path instead.
        ArchPrctlCode::UnlockShstk => Err(LinuxError::EPERM.into()),
        ArchPrctlCode::StatusShstk => {
            if !axhal::asm::user_shadow_stack_enabled() {
                return Err(AxError::OperationNotSupported);
            }
            let state = crate::task::current_user_live_cet_state();
            memory
                .write_value(
                    addr as *mut usize,
                    (state.u_cet & (CET_SHSTK_EN | CET_WRSS_EN)) as usize,
                )
                .map_err(map_usercopy_error)?;
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_arch = "x86_64")]
    use axerrno::AxError;

    #[cfg(target_arch = "x86_64")]
    use super::cet_default_shadow_stack_size;
    use super::render_parent_pid;
    use crate::task::{PidNamespace, UserNamespace};

    #[test]
    fn getppid_parent_identity_obeys_pid_namespace_visibility() {
        let user_ns = UserNamespace::try_new_root().unwrap();
        let root_pid_ns = PidNamespace::try_new_root(user_ns.clone()).unwrap();
        let root_init_binding = root_pid_ns.reserve_process(1).unwrap();
        root_init_binding.commit();
        let child_pid_ns = root_pid_ns.try_fork(20, user_ns).unwrap();
        // The process adapter deliberately owns only process topology.  A
        // kernel process reaches PID visibility through the paired namespace
        // reservation, which binds it in every ancestor at publication.
        let child_pid_binding = child_pid_ns.reserve_process(20).unwrap();
        let domain = thekernel_linux_process_adapter::ProcessDomain::<()>::try_new().unwrap();
        let init = domain
            .try_new_init_with_identity(1, None, root_pid_ns.clone())
            .unwrap();
        let child = domain
            .prepare_fork_with_identity(&init, 20, None, child_pid_ns.clone())
            .unwrap();
        child.commit();
        child_pid_binding.commit();

        assert_eq!(render_parent_pid::<(), ()>(None, &root_pid_ns), 0);
        assert_eq!(render_parent_pid(Some(&init), &root_pid_ns), 1);
        assert_eq!(render_parent_pid(Some(&init), &child_pid_ns), 0);
        // The child is PID 1 in its new namespace and receives the next
        // namespace-local identity in the parent (PID 2), not its global ID.
        assert_eq!(root_pid_ns.visible_pid_for(&child_pid_ns, 20), Some(2));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn cet_default_shadow_stack_size_uses_rlimit_cap_and_page_alignment() {
        let page = 4096;
        assert_eq!(
            cet_default_shadow_stack_size(0, page as u64 + 1),
            Ok(page * 2)
        );
        assert_eq!(
            cet_default_shadow_stack_size(0, 8 * 1024 * 1024 * 1024),
            Ok(4 * 1024 * 1024 * 1024)
        );
        // A non-zero clone3 stack_size is intentional and is not capped by
        // the default-stack policy.
        assert_eq!(cet_default_shadow_stack_size(page + 1, 1), Ok(page * 2));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn cet_default_shadow_stack_size_rejects_empty_and_overflowing_requests() {
        assert_eq!(cet_default_shadow_stack_size(0, 0), Err(AxError::NoMemory));
        assert_eq!(
            cet_default_shadow_stack_size(usize::MAX, 1),
            Err(AxError::NoMemory)
        );
    }
}
