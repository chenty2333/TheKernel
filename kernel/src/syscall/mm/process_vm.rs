use alloc::{sync::Arc, vec::Vec};
use core::mem::MaybeUninit;

use axerrno::{AxError, AxResult, LinuxError};
use axhal::paging::MappingFlags;
use axsync::Mutex;
use axtask::current;
use linux_raw_sys::general::{CAP_SYS_NICE, MADV_COLD, MADV_COLLAPSE, MADV_PAGEOUT, MADV_WILLNEED};
use memory_addr::{MemoryAddr, PAGE_SIZE_4K, VirtAddr};

use crate::{
    file::{FileLike, PidFd},
    mm::{AddrSpace, IoVec, UserMemoryCapability, checked_align_up_4k, map_usercopy_error},
    task::{
        AsThread, PtraceAccessMode, check_current_ptrace_image_snapshot,
        check_current_thread_ptrace_image_access, get_process_data, get_visible_task,
        has_pending_syscall_signal, process_domain, process_error,
    },
};

const PROCESS_VM_MAX_IOV: usize = 1024;
const PROCESS_VM_COPY_CHUNK: usize = 16 * 1024;

#[derive(Clone, Copy)]
struct UserIoVec {
    base: usize,
    len: usize,
}

#[derive(Clone, Copy)]
enum ProcessVmOp {
    ReadRemote,
    WriteRemote,
}

fn read_iovecs(
    caller: &UserMemoryCapability,
    iovs: *const IoVec,
    iovcnt: usize,
) -> AxResult<(Vec<UserIoVec>, usize)> {
    if iovcnt > PROCESS_VM_MAX_IOV {
        return Err(AxError::InvalidInput);
    }
    if iovcnt == 0 {
        return Ok((Vec::new(), 0));
    }
    if iovs.is_null() {
        return Err(AxError::BadAddress);
    }

    let mut result = Vec::new();
    result
        .try_reserve_exact(iovcnt)
        .map_err(|_| AxError::NoMemory)?;
    let mut total = 0usize;
    for index in 0..iovcnt {
        let offset = index
            .checked_mul(core::mem::size_of::<IoVec>())
            .ok_or(AxError::BadAddress)?;
        let address = (iovs as usize)
            .checked_add(offset)
            .ok_or(AxError::BadAddress)?;
        let iov = caller
            .read_value(address as *const IoVec)
            .map_err(map_usercopy_error)?;
        if iov.iov_len < 0 {
            return Err(AxError::InvalidInput);
        }
        let len = iov.iov_len as usize;
        total = total.checked_add(len).ok_or(AxError::InvalidInput)?;
        result.push(UserIoVec {
            base: iov.iov_base as usize,
            len,
        });
    }
    Ok((result, total))
}

fn check_process_madvise_capability() -> AxResult<()> {
    let curr = current();
    if curr.as_thread().has_effective_capability(CAP_SYS_NICE) {
        Ok(())
    } else {
        Err(AxError::OperationNotPermitted)
    }
}

fn check_process_madvise_capability_if_remote(
    caller_aspace: &Arc<Mutex<AddrSpace>>,
    target_aspace: &Arc<Mutex<AddrSpace>>,
) -> AxResult<()> {
    if !process_madvise_is_remote(caller_aspace, target_aspace) {
        Ok(())
    } else {
        check_process_madvise_capability()
    }
}

#[inline]
fn process_madvise_is_remote(
    caller_aspace: &Arc<Mutex<AddrSpace>>,
    target_aspace: &Arc<Mutex<AddrSpace>>,
) -> bool {
    !Arc::ptr_eq(caller_aspace, target_aspace)
}

fn validate_process_madvise_behavior(behavior: u32) -> AxResult<()> {
    match behavior {
        MADV_WILLNEED => Ok(()),
        MADV_COLD | MADV_PAGEOUT | MADV_COLLAPSE => Err(AxError::OperationNotSupported),
        _ => Err(AxError::InvalidInput),
    }
}

fn validate_address_range(
    aspace: &mut AddrSpace,
    base: usize,
    len: usize,
    access_flags: MappingFlags,
) -> AxResult<()> {
    if len == 0 {
        return Ok(());
    }
    let start = VirtAddr::from(base);
    let end = start.checked_add(len).ok_or(AxError::BadAddress)?;
    if !aspace.contains_range(start, len) {
        return Err(AxError::BadAddress);
    }
    if !aspace.can_access_range(start, len, access_flags) {
        return Err(AxError::BadAddress);
    }
    let page_start = start.align_down_4k();
    let page_end = VirtAddr::from(checked_align_up_4k(end.as_usize()).ok_or(AxError::BadAddress)?);
    match aspace.populate_area(page_start, page_end.sub_addr(page_start), access_flags) {
        Ok(()) => {}
        // The VMA and permissions were checked above. Preserve an allocation
        // failure for a valid range instead of collapsing it into EFAULT.
        Err(AxError::NoMemory) => return Err(AxError::NoMemory),
        // Address/permission failures are user-range faults; only an
        // allocation failure for an otherwise valid VMA remains ENOMEM.
        Err(AxError::BadAddress | AxError::InvalidInput | AxError::PermissionDenied) => {
            return Err(AxError::BadAddress);
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

#[inline]
fn page_copy_len(address: usize, len: usize) -> usize {
    let page_offset = address & (PAGE_SIZE_4K - 1);
    len.min(PAGE_SIZE_4K - page_offset)
}

fn validate_remote_iovecs(
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    remote: &[UserIoVec],
) -> AxResult<()> {
    let mut aspace = aspace_handle.lock();
    for iov in remote {
        validate_address_range(&mut aspace, iov.base, iov.len, MappingFlags::READ)?;
    }
    Ok(())
}

fn copy_from_remote(
    caller: &UserMemoryCapability,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    remote: usize,
    local: usize,
    len: usize,
    scratch: &mut [u8],
) -> AxResult<()> {
    let mut copied = 0usize;
    while copied < len {
        let remote_addr = remote.checked_add(copied).ok_or(AxError::BadAddress)?;
        let local_addr = local.checked_add(copied).ok_or(AxError::BadAddress)?;
        let chunk = (len - copied)
            .min(scratch.len())
            .min(page_copy_len(remote_addr, len - copied))
            .min(page_copy_len(local_addr, len - copied));
        debug_assert!(chunk != 0);
        {
            let mut aspace = aspace_handle.lock();
            validate_address_range(&mut aspace, remote_addr, chunk, MappingFlags::READ)?;
            aspace.read(VirtAddr::from(remote_addr), &mut scratch[..chunk])?;
        }
        caller
            .write_bytes(local_addr, &scratch[..chunk])
            .map_err(map_usercopy_error)?;
        copied += chunk;
    }
    Ok(())
}

fn copy_to_remote(
    caller: &UserMemoryCapability,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    local: usize,
    remote: usize,
    len: usize,
    scratch: &mut [u8],
) -> AxResult<()> {
    let mut copied = 0usize;
    while copied < len {
        let local_addr = local.checked_add(copied).ok_or(AxError::BadAddress)?;
        let remote_addr = remote.checked_add(copied).ok_or(AxError::BadAddress)?;
        let chunk = (len - copied)
            .min(scratch.len())
            .min(page_copy_len(local_addr, len - copied))
            .min(page_copy_len(remote_addr, len - copied));
        debug_assert!(chunk != 0);
        let buf = unsafe {
            core::slice::from_raw_parts_mut(scratch.as_mut_ptr().cast::<MaybeUninit<u8>>(), chunk)
        };
        caller
            .read_bytes(local_addr, buf)
            .map_err(map_usercopy_error)?;
        {
            let mut aspace = aspace_handle.lock();
            validate_address_range(&mut aspace, remote_addr, chunk, MappingFlags::WRITE)?;
            aspace.write(VirtAddr::from(remote_addr), &scratch[..chunk])?;
        }
        copied += chunk;
    }
    Ok(())
}

fn process_vm_copy(
    caller: &UserMemoryCapability,
    aspace_handle: &Arc<Mutex<AddrSpace>>,
    local: &[UserIoVec],
    remote: &[UserIoVec],
    max_len: usize,
    op: ProcessVmOp,
) -> AxResult<isize> {
    if max_len == 0 {
        return Ok(0);
    }

    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(PROCESS_VM_COPY_CHUNK.min(max_len))
        .map_err(|_| AxError::NoMemory)?;
    scratch.resize(PROCESS_VM_COPY_CHUNK.min(max_len), 0);
    let mut local_index = 0usize;
    let mut remote_index = 0usize;
    let mut local_offset = 0usize;
    let mut remote_offset = 0usize;
    let mut copied_total = 0usize;

    while copied_total < max_len && local_index < local.len() && remote_index < remote.len() {
        while local_index < local.len() && local[local_index].len == local_offset {
            local_index += 1;
            local_offset = 0;
        }
        while remote_index < remote.len() && remote[remote_index].len == remote_offset {
            remote_index += 1;
            remote_offset = 0;
        }
        if local_index >= local.len() || remote_index >= remote.len() {
            break;
        }

        let copy_len = (max_len - copied_total)
            .min(local[local_index].len - local_offset)
            .min(remote[remote_index].len - remote_offset)
            .min(scratch.len());
        let Some(local_addr) = local[local_index].base.checked_add(local_offset) else {
            return if copied_total == 0 {
                Err(AxError::BadAddress)
            } else {
                Ok(copied_total as isize)
            };
        };
        let Some(remote_addr) = remote[remote_index].base.checked_add(remote_offset) else {
            return if copied_total == 0 {
                Err(AxError::BadAddress)
            } else {
                Ok(copied_total as isize)
            };
        };
        let copy_len = copy_len
            .min(page_copy_len(local_addr, copy_len))
            .min(page_copy_len(remote_addr, copy_len));
        debug_assert!(copy_len != 0);

        // Validate the caller-owned destination/source before touching the
        // target image. This keeps a local fault from causing a remote access
        // and preserves Linux's positive-prefix result after prior chunks.
        let local_result = {
            let mut caller_aspace = caller.address_space().lock();
            let access_flags = match op {
                ProcessVmOp::ReadRemote => MappingFlags::WRITE,
                ProcessVmOp::WriteRemote => MappingFlags::READ,
            };
            validate_address_range(&mut caller_aspace, local_addr, copy_len, access_flags)
        };
        if let Err(err) = local_result {
            return if copied_total == 0 {
                Err(err)
            } else {
                Ok(copied_total as isize)
            };
        }

        let copy_result = match op {
            ProcessVmOp::ReadRemote => copy_from_remote(
                caller,
                aspace_handle,
                remote_addr,
                local_addr,
                copy_len,
                &mut scratch,
            ),
            ProcessVmOp::WriteRemote => copy_to_remote(
                caller,
                aspace_handle,
                local_addr,
                remote_addr,
                copy_len,
                &mut scratch,
            ),
        };
        if let Err(err) = copy_result {
            return if copied_total == 0 {
                Err(err)
            } else {
                Ok(copied_total as isize)
            };
        }

        copied_total += copy_len;
        local_offset += copy_len;
        remote_offset += copy_len;
    }

    Ok(copied_total as isize)
}

fn sys_process_vm_rw(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    pid: i32,
    local_iov: *const IoVec,
    local_iovcnt: usize,
    remote_iov: *const IoVec,
    remote_iovcnt: usize,
    flags: usize,
    op: ProcessVmOp,
) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    let caller = UserMemoryCapability::new(caller_aspace);
    let (local, local_len) = read_iovecs(&caller, local_iov, local_iovcnt)?;
    if local_len == 0 {
        return Ok(0);
    }
    let (remote, remote_len) = read_iovecs(&caller, remote_iov, remote_iovcnt)?;
    let copy_len = local_len.min(remote_len);
    if copy_len == 0 {
        return Ok(0);
    }
    if pid < 0 {
        return Err(AxError::NoSuchProcess);
    }

    let target_task = get_visible_task(pid as u32)?;
    let target_thread = target_task.as_thread();
    let target_image =
        check_current_thread_ptrace_image_access(target_thread, PtraceAccessMode::AttachReal)?;
    let target_aspace = target_image.into_aspace();
    process_vm_copy(
        &caller,
        &target_aspace,
        &local,
        &remote,
        copy_len,
        op,
    )
}

pub fn sys_process_vm_readv(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    pid: i32,
    local_iov: *const IoVec,
    local_iovcnt: usize,
    remote_iov: *const IoVec,
    remote_iovcnt: usize,
    flags: usize,
) -> AxResult<isize> {
    sys_process_vm_rw(
        caller_aspace,
        pid,
        local_iov,
        local_iovcnt,
        remote_iov,
        remote_iovcnt,
        flags,
        ProcessVmOp::ReadRemote,
    )
}

pub fn sys_process_vm_writev(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    pid: i32,
    local_iov: *const IoVec,
    local_iovcnt: usize,
    remote_iov: *const IoVec,
    remote_iovcnt: usize,
    flags: usize,
) -> AxResult<isize> {
    sys_process_vm_rw(
        caller_aspace,
        pid,
        local_iov,
        local_iovcnt,
        remote_iov,
        remote_iovcnt,
        flags,
        ProcessVmOp::WriteRemote,
    )
}

pub fn sys_process_madvise(
    caller_aspace: Arc<Mutex<AddrSpace>>,
    pidfd: i32,
    iovs: *const IoVec,
    iovcnt: usize,
    behavior: u32,
    flags: u32,
) -> AxResult<isize> {
    debug!(
        "sys_process_madvise <= pidfd: {pidfd}, iovs: {iovs:?}, iovcnt: {iovcnt}, behavior: \
         {behavior:#x}, flags: {flags:#x}"
    );

    if flags != 0 {
        return Err(AxError::InvalidInput);
    }
    validate_process_madvise_behavior(behavior)?;

    let caller = UserMemoryCapability::new(caller_aspace);
    let (remote, total_len) = read_iovecs(&caller, iovs, iovcnt)?;

    let pidfd = PidFd::from_fd(pidfd)?;
    let target = pidfd.process_data()?;
    let target_image = pidfd.image_access_snapshot()?;
    check_current_ptrace_image_snapshot(&target, &target_image, PtraceAccessMode::ReadFs)?;
    check_process_madvise_capability_if_remote(caller.address_space(), target_image.aspace())?;
    let target_aspace = target_image.into_aspace();
    validate_remote_iovecs(&target_aspace, &remote)?;
    Ok(total_len as isize)
}

/// Linux 6.12 `process_mrelease(2)`: synchronously run the OOM-reaper portion
/// for an exiting pidfd target.  This never tears down VMAs themselves; it
/// drains only reclaimable private COW PTEs/backing and preserves the mm for
/// the normal exit/reap lifecycle.
pub fn sys_process_mrelease(pidfd: i32, flags: u32) -> AxResult<isize> {
    if flags != 0 {
        return Err(AxError::InvalidInput);
    }

    // Preserve pidfd_get_task-style descriptor/type/liveness errors exactly.
    let pidfd = PidFd::from_fd(pidfd)?;
    let target = pidfd.process_data()?;
    if !target.oom_reap_eligible() {
        return Err(AxError::InvalidInput);
    }

    // `mmap_read_lock_killable()` observes an already pending deliverable
    // signal before sleeping.  axsync exposes no interruptible mutex wait, so
    // contention is reported as the Linux retry result below.
    if has_pending_syscall_signal(current().as_thread()) {
        return Err(AxError::Interrupted);
    }

    let aspace = target.aspace();
    // `mm_users` in Linux includes other CLONE_VM process groups.  Snapshot
    // the process domain before claiming teardown and refuse a live sharer;
    // every remaining sharer has already crossed its permanent exit gate, so
    // none can publish a new CLONE_VM child after this point.
    for process in process_domain()?
        .registry()
        .try_processes()
        .map_err(process_error)?
    {
        if core::ptr::eq(&*process, &*target.proc) {
            continue;
        }
        let Ok(sharer) = get_process_data(process.pid()) else {
            continue;
        };
        if Arc::ptr_eq(&aspace, &sharer.aspace()) && !sharer.oom_reap_eligible() {
            return Err(AxError::InvalidInput);
        }
    }

    let claimed = aspace.lock().begin_oom_reap().map_err(|error| match error {
        AxError::ResourceBusy => LinuxError::EAGAIN.into(),
        _ => error,
    })?;
    if !claimed {
        return Ok(0);
    }

    let Some(mut aspace) = aspace.try_lock() else {
        target.aspace().lock().finish_oom_reap(false);
        return Err(LinuxError::EAGAIN.into());
    };
    if has_pending_syscall_signal(current().as_thread()) {
        drop(aspace);
        target.aspace().lock().finish_oom_reap(false);
        return Err(AxError::Interrupted);
    }
    let completed = aspace.oom_reap_private_pages();
    aspace.finish_oom_reap(completed);
    drop(aspace);
    if completed {
        Ok(0)
    } else {
        Err(LinuxError::EAGAIN.into())
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use axerrno::AxError;
    use axhal::paging::{MappingFlags, PageSize};
    use axsync::Mutex;
    use linux_raw_sys::general::{MADV_COLD, MADV_COLLAPSE, MADV_PAGEOUT, MADV_WILLNEED};
    use memory_addr::{PAGE_SIZE_4K, VirtAddr};

    use super::{
        IoVec, ProcessVmOp, UserIoVec, UserMemoryCapability, page_copy_len,
        process_madvise_is_remote, process_vm_copy, read_iovecs, validate_process_madvise_behavior,
    };
    use crate::mm::{AddrSpace, Backend};

    fn mapped_aspace(base: usize, mapped_pages: usize) -> Arc<Mutex<AddrSpace>> {
        let mut aspace = AddrSpace::new_empty(VirtAddr::from(base), PAGE_SIZE_4K * 2).unwrap();
        for page in 0..mapped_pages {
            let address = base + page * PAGE_SIZE_4K;
            aspace
                .map(
                    VirtAddr::from(address),
                    PAGE_SIZE_4K,
                    MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE,
                    false,
                    Backend::new_alloc(VirtAddr::from(address), PageSize::Size4K),
                )
                .unwrap();
        }
        Arc::new(Mutex::new(aspace))
    }

    fn mapped_capability() -> UserMemoryCapability {
        UserMemoryCapability::new(mapped_aspace(0x1000, 1))
    }

    #[test]
    fn process_vm_iov_descriptors_are_imported_from_explicit_caller_space() {
        let capability = mapped_capability();
        let descriptor = IoVec {
            iov_base: 0x1000 as *mut u8,
            iov_len: 37,
        };
        // SAFETY: IoVec is a complete initialized descriptor and the selected
        // capability owns the mapped destination range.
        unsafe {
            capability
                .write_value_unchecked(0x1000 as *mut IoVec, descriptor)
                .unwrap();
        }

        let (iovecs, total) = read_iovecs(&capability, 0x1000 as *const IoVec, 1).unwrap();
        assert_eq!(total, 37);
        assert_eq!(iovecs[0].base, 0x1000);
        assert_eq!(iovecs[0].len, 37);
    }

    #[test]
    fn process_vm_descriptor_range_is_faulted_before_element_copyin() {
        let capability = mapped_capability();
        assert!(matches!(
            read_iovecs(&capability, 0x1ff8 as *const IoVec, 1),
            Err(AxError::BadAddress)
        ));
    }

    #[test]
    fn process_vm_copy_batches_stop_at_page_boundaries() {
        assert_eq!(page_copy_len(0x1000, PAGE_SIZE_4K * 4), PAGE_SIZE_4K);
        assert_eq!(page_copy_len(0x1fff, PAGE_SIZE_4K), 1);
        assert_eq!(page_copy_len(0x2000, 37), 37);
    }

    #[test]
    fn process_vm_copy_returns_prefix_when_remote_second_page_faults() {
        let caller_aspace = mapped_aspace(0x1000, 2);
        let target_aspace = mapped_aspace(0x4000, 1);
        let caller = UserMemoryCapability::new(caller_aspace);
        let local = [UserIoVec {
            base: 0x1000,
            len: PAGE_SIZE_4K * 2,
        }];
        let remote = [UserIoVec {
            base: 0x4000,
            len: PAGE_SIZE_4K * 2,
        }];

        assert_eq!(
            process_vm_copy(
                &caller,
                &target_aspace,
                &local,
                &remote,
                PAGE_SIZE_4K * 2,
                ProcessVmOp::ReadRemote,
            )
            .unwrap(),
            PAGE_SIZE_4K as isize
        );

        assert_eq!(
            process_vm_copy(
                &caller,
                &target_aspace,
                &local,
                &remote,
                PAGE_SIZE_4K * 2,
                ProcessVmOp::WriteRemote,
            )
            .unwrap(),
            PAGE_SIZE_4K as isize
        );
    }

    #[test]
    fn process_vm_copy_reports_efault_before_the_first_byte() {
        let caller = UserMemoryCapability::new(mapped_aspace(0x1000, 0));
        let target = mapped_aspace(0x4000, 1);
        let local = [UserIoVec {
            base: 0x1000,
            len: PAGE_SIZE_4K,
        }];
        let remote = [UserIoVec {
            base: 0x4000,
            len: PAGE_SIZE_4K,
        }];

        assert_eq!(
            process_vm_copy(
                &caller,
                &target,
                &local,
                &remote,
                PAGE_SIZE_4K,
                ProcessVmOp::ReadRemote,
            ),
            Err(AxError::BadAddress)
        );
    }

    #[test]
    fn process_madvise_capability_gate_distinguishes_same_mm() {
        let caller = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x1000), PAGE_SIZE_4K).unwrap(),
        ));
        let remote = Arc::new(Mutex::new(
            AddrSpace::new_empty(VirtAddr::from(0x4000), PAGE_SIZE_4K).unwrap(),
        ));

        assert!(!process_madvise_is_remote(&caller, &caller));
        assert!(process_madvise_is_remote(&caller, &remote));
    }

    #[test]
    fn process_madvise_only_accepts_implemented_behavior() {
        assert_eq!(validate_process_madvise_behavior(MADV_WILLNEED), Ok(()));
        for behavior in [MADV_COLD, MADV_PAGEOUT, MADV_COLLAPSE] {
            assert_eq!(
                validate_process_madvise_behavior(behavior),
                Err(AxError::OperationNotSupported)
            );
        }
        assert_eq!(
            validate_process_madvise_behavior(u32::MAX),
            Err(AxError::InvalidInput)
        );
    }
}
